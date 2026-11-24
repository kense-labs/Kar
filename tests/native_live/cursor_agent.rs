use super::*;
use axum::{
    body::{Body, BodyDataStream, Bytes},
    extract::{Request, State},
};
use futures_util::{StreamExt, stream};
use kar::chat::{ChatReplica, ChatSession, HttpChatFeed, event_router};
use kar::{ApprovalMode, Error, PendingRequest, QuestionReply, SessionOptions};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

#[derive(Clone, PartialEq, Message)]
struct Parameter {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    value: String,
}
#[derive(Clone, PartialEq, Message)]
struct Model {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(message, repeated, tag = "3")]
    parameters: Vec<Parameter>,
}
#[derive(Clone, PartialEq, Message)]
struct Checkpoint {
    #[prost(bytes = "vec", repeated, tag = "8")]
    turns: Vec<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
struct UserAction {
    #[prost(bytes = "vec", tag = "1")]
    user: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
struct Action {
    #[prost(message, optional, tag = "1")]
    user: Option<UserAction>,
}
#[derive(Clone, PartialEq, Message)]
struct UserMessage {
    #[prost(string, tag = "1")]
    text: String,
}
#[derive(Clone, PartialEq, Message)]
struct Run {
    #[prost(message, optional, tag = "1")]
    state: Option<Checkpoint>,
    #[prost(message, optional, tag = "2")]
    action: Option<Action>,
    #[prost(string, tag = "5")]
    conversation: String,
    #[prost(message, optional, tag = "9")]
    model: Option<Model>,
}
#[derive(Clone, PartialEq, Message)]
struct BlobRead {
    #[prost(bytes = "vec", optional, tag = "1")]
    data: Option<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
struct KvReply {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(message, optional, tag = "2")]
    read: Option<BlobRead>,
    #[prost(bytes = "vec", optional, tag = "3")]
    written: Option<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
struct Client {
    #[prost(message, optional, tag = "1")]
    run: Option<Run>,
    #[prost(message, optional, tag = "3")]
    kv: Option<KvReply>,
    #[prost(bytes = "vec", optional, tag = "6")]
    interaction: Option<Vec<u8>>,
}

struct Peer {
    input: BodyDataStream,
    buffer: Vec<u8>,
    output: mpsc::Sender<Result<Bytes, Infallible>>,
}
impl Peer {
    async fn read(&mut self) -> Client {
        loop {
            if self.buffer.len() >= 5 {
                assert_eq!(self.buffer[0], 0, "unexpected compressed Connect frame");
                let size = u32::from_be_bytes(self.buffer[1..5].try_into().unwrap()) as usize;
                if self.buffer.len() >= 5 + size {
                    let value = Client::decode(&self.buffer[5..5 + size]).unwrap();
                    self.buffer.drain(..5 + size);
                    return value;
                }
            }
            self.buffer.extend(
                self.input
                    .next()
                    .await
                    .expect("Cursor closed the stream")
                    .unwrap(),
            );
        }
    }
    async fn send(&mut self, bytes: &[u8], flag: u8) {
        let mut out = vec![flag];
        out.extend((bytes.len() as u32).to_be_bytes());
        out.extend(bytes);
        self.output.send(Ok(out.into())).await.unwrap();
    }
    async fn kv(&mut self, id: u8, op: usize, args: &[u8]) -> KvReply {
        let mut message = vec![8, id];
        message.extend(fields(&[(op, args)]));
        self.send(&fields(&[(4, &message)]), 0).await;
        loop {
            let client = self.read().await;
            assert!(client.interaction.is_none(), "duplicate interaction reply");
            if let Some(kv) = client.kv
                && kv.id == u32::from(id)
            {
                return kv;
            }
        }
    }
    async fn ask(&mut self) {
        let choice = fields(&[(1, b"compact"), (2, b"Compact")]);
        let question = fields(&[(1, b"format"), (2, b"Choose a format"), (3, &choice)]);
        let args = fields(&[(1, b"Output settings"), (2, &question)]);
        let call_id: &[u8] = b"ask-format";
        let tool_update = |event, call: &[u8]| {
            let tool = fields(&[(23, call), (57, call_id)]);
            fields(&[(1, &fields(&[(event, &fields(&[(1, call_id), (2, &tool)]))]))])
        };
        self.send(&tool_update(2, &fields(&[(1, &args)])), 0).await;
        let mut query = vec![8, 42];
        query.extend(fields(&[(3, &fields(&[(1, &args), (2, call_id)]))]));
        self.send(&fields(&[(7, &query)]), 0).await;
        let answer = fields(&[(1, b"format"), (2, b"compact")]);
        let result = fields(&[(1, &fields(&[(1, &answer)]))]);
        let mut expected = vec![8, 42];
        expected.extend(fields(&[(3, &fields(&[(1, &result)]))]));
        loop {
            if let Some(reply) = self.read().await.interaction {
                assert_eq!(reply, expected, "unexpected answer from Cursor");
                break;
            }
        }
        self.send(&tool_update(3, &fields(&[(1, &args), (2, &result)])), 0)
            .await;
    }
    async fn store(&mut self, id: u8, data: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(data).to_vec();
        let result = self.kv(id, 3, &fields(&[(1, &hash), (2, data)])).await;
        assert_eq!(
            result.written,
            Some(vec![]),
            "Cursor failed to save its conversation blob"
        );
        hash
    }
}

#[derive(Default)]
struct Observed {
    prompts: Vec<String>,
    turns: Vec<Vec<u8>>,
    last_turn: Vec<u8>,
    sessions: Vec<String>,
}
type Backend = Arc<Mutex<Observed>>;

async fn run(mut peer: Peer, backend: Backend) {
    let run = loop {
        if let Some(run) = peer.read().await.run {
            break run;
        }
    };
    let model = run.model.expect("missing native requested_model");
    assert_eq!(model.id, "offline");
    assert!(
        model
            .parameters
            .iter()
            .any(|p| p.id == "reasoning_effort" && p.value == "high"),
        "{model:?}"
    );
    let mut checkpoint = run.state.unwrap_or_default();
    let (before, last) = {
        let observed = backend.lock().unwrap();
        (observed.turns.clone(), observed.last_turn.clone())
    };
    assert_eq!(
        checkpoint.turns, before,
        "model received a different conversation"
    );
    if let Some(blob) = before.last() {
        let reply = peer.kv(1, 2, &fields(&[(1, blob)])).await;
        assert_eq!(
            reply.read.and_then(|r| r.data),
            Some(last),
            "previous turn missing from Cursor's native store"
        );
    }
    let user = run.action.unwrap().user.unwrap().user;
    let prompt = UserMessage::decode(user.as_slice()).unwrap().text;
    if prompt == "first" {
        peer.ask().await;
    }
    let text = format!("Reply to {prompt}");
    let assistant = fields(&[(1, text.as_bytes())]);
    peer.send(&fields(&[(1, &fields(&[(1, &assistant)]))]), 0)
        .await;
    let user_id = peer.store(2, &user).await;
    let step_id = peer.store(3, &fields(&[(1, &assistant)])).await;
    let turn = fields(&[(1, &fields(&[(1, &user_id), (2, &step_id)]))]);
    let turn_id = peer.store(4, &turn).await;
    checkpoint.turns.push(turn_id);
    peer.send(&fields(&[(3, &checkpoint.encode_to_vec())]), 0)
        .await;
    {
        let mut observed = backend.lock().unwrap();
        observed.turns = checkpoint.turns;
        observed.last_turn = turn;
        observed.prompts.push(prompt);
        observed.sessions.push(run.conversation);
    }
    peer.send(&fields(&[(1, &fields(&[(14, &[])]))]), 0).await;
    peer.send(b"{}", 2).await;
}

async fn receive(State(backend): State<Backend>, req: Request) -> Response {
    if req.uri().path() != "/agent.v1.AgentService/Run" {
        return metadata(req.uri().clone()).await;
    }
    assert_eq!(
        req.headers()[header::CONTENT_TYPE],
        "application/connect+proto"
    );
    let (output, rx) = mpsc::channel(8);
    tokio::spawn(run(
        Peer {
            input: req.into_body().into_data_stream(),
            buffer: vec![],
            output,
        },
        backend,
    ));
    let stream = stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });
    (
        [(header::CONTENT_TYPE, "application/connect+proto")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn pending(client: &mut ChatReplica, feed: &mut HttpChatFeed) -> PendingRequest {
    loop {
        let event = feed.recv(client.cursor()).await.unwrap();
        assert!(client.apply(event).unwrap());
        if let Some(question) = client.pending_requests().first() {
            assert_eq!(question.method, "cursor/ask_question");
            assert_eq!(question.body["toolCallId"], "ask-format");
            let turn = client.turns().last().unwrap();
            assert_eq!(turn.status, TurnStatus::Running);
            assert!(
                turn.items
                    .iter()
                    .any(|i| matches!(&i.item, Item::Tool { activity, .. } if !activity.finished))
            );
            return question.clone();
        }
    }
}

async fn review(session: &ChatSession, base: &str) -> (ChatReplica, HttpChatFeed, ChatReplica) {
    let mut first = HttpChatFeed::new(base).unwrap();
    let mut second = HttpChatFeed::new(base).unwrap();
    let mut a = ChatReplica::from_snapshot(first.snapshot().await.unwrap());
    let mut b = ChatReplica::from_snapshot(second.snapshot().await.unwrap());
    let answer = async {
        let (qa, qb) = tokio::join!(pending(&mut a, &mut first), pending(&mut b, &mut second));
        assert_eq!(qa, qb);
        assert_eq!(
            qa.session_id.as_deref(),
            Some(session.info().native_id.as_str())
        );
        let form = qa.question_form().unwrap();
        assert_eq!(form.message.as_deref(), Some("Output settings"));
        assert_eq!(form.fields[0].options[0].value, "compact");
        drop(second);
        let reply = QuestionReply::Submit {
            values: serde_json::json!({"format":"compact"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let (r1, r2) = tokio::join!(
            session.answer_question(&qa, reply.clone()),
            session.answer_question(&qb, reply)
        );
        assert!(matches!(
            (r1, r2),
            (Ok(()), Err(Error::Refused(_))) | (Err(Error::Refused(_)), Ok(()))
        ));
    };
    let (turn, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(session.send(Uuid::new_v4(), "first"), answer)
    })
    .await
    .unwrap();
    assert_eq!(turn.unwrap().status, TurnStatus::Completed);
    (a, first, b)
}

#[tokio::test]
#[ignore = "requires Cursor CLI with ACP; uses the local AgentService and native conversation store"]
async fn actual_cursor_agent_turn() {
    let program = std::env::var("KAR_CURSOR_BIN").unwrap_or_else(|_| "cursor-agent".into());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::create_dir(dir.join("work")).unwrap();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    let backend = Backend::default();
    let router = Router::new().fallback(receive).with_state(backend.clone());
    let server = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    let invoke = async |args: &[&str]| {
        let mut cmd = command(dir, &endpoint, args);
        cmd.env_remove("HTTPS_PROXY")
            .env_remove("HTTP_PROXY")
            .env("DO_NOT_TRACK", "1");
        let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let store = ChatStore::open(dir.join("chats")).unwrap();
    let mut opts = SessionOptions::new(dir.join("work"));
    opts.approvals = ApprovalMode::Host;
    opts.model = Some("offline".into());
    opts.settings.effort = Some("high".into());
    for key in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
    ] {
        opts.env.insert(key.into(), dir.as_os_str().into());
    }
    for (key, value) in [
        ("HTTPS_PROXY", ""),
        ("HTTP_PROXY", ""),
        ("ALL_PROXY", ""),
        ("CURSOR_API_KEY", "kar-loopback-test"),
        ("CURSOR_API_ENDPOINT", endpoint.as_str()),
        ("NO_PROXY", "127.0.0.1,localhost"),
        ("DO_NOT_TRACK", "1"),
    ] {
        opts.env.insert(key.into(), value.into());
    }
    let session = store
        .create("cursor", Some(Path::new(&program)), opts)
        .await
        .unwrap();
    let chat = session.info().clone();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let router = event_router(store.clone(), chat.id);
    let events = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    let (mut a, mut first, mut b) = review(&session, &base).await;
    session.close().await.unwrap();
    let id = chat.id.to_string();
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    store.enqueue(chat.id, second, "second").unwrap();
    store.enqueue(chat.id, third, "third").unwrap();
    for _ in 0..2 {
        invoke(&["chat", "resume", "--chat", &id]).await;
    }
    let history = store.history(chat.id).unwrap();
    assert_eq!(
        history.chat.workspace,
        dir.join("work").canonicalize().unwrap()
    );
    assert_eq!(history.turns.len(), 3);
    let tools: Vec<_> = history.turns[0]
        .items
        .iter()
        .filter_map(|i| match &i.item {
            Item::Tool { activity, .. } => Some(activity),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1);
    assert!(tools[0].finished);
    assert_eq!(history.turns[1].id, second);
    assert_eq!(history.turns[2].id, third);
    for (turn, text) in history.turns.iter().zip(["first", "second", "third"]) {
        assert_eq!(turn.status, TurnStatus::Completed);
        let replies: Vec<_> = turn
            .items
            .iter()
            .filter_map(|i| match &i.item {
                Item::Message {
                    role: Role::Assistant,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(replies, [format!("Reply to {text}")]);
    }
    let inputs = store.queued_inputs(chat.id).unwrap();
    assert_eq!(inputs.len(), 2);
    assert!(inputs.iter().all(|i| i.status == InputStatus::Sent));
    let snapshot = store.feed_snapshot(chat.id).unwrap();
    let mut reconnected = HttpChatFeed::new(&base).unwrap();
    for (replica, feed) in [(&mut a, &mut first), (&mut b, &mut reconnected)] {
        while replica.cursor() != snapshot.cursor {
            let event = tokio::time::timeout(Duration::from_secs(5), feed.recv(replica.cursor()))
                .await
                .unwrap()
                .unwrap();
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        assert_eq!(replica.turns(), history.turns);
        assert!(replica.pending_requests().is_empty());
    }
    events.abort();
    let observed = backend.lock().unwrap();
    assert_eq!(observed.prompts, ["first", "second", "third"]);
    assert_eq!(observed.sessions, vec![chat.native_id; 3]);
    server.abort();
}
