use super::emit;
use kar::{
    Error, InputReceipt, SessionOptions, SteerStatus,
    chat::{ChatReplica, ChatStore, HttpChatFeed, Item, Role, TurnStatus},
};
use serde_json::{Value, json};
use std::{io::BufRead, sync::Arc};
use uuid::Uuid;

fn notify(method: &str, params: Value) {
    emit(json!({"jsonrpc":"2.0","method":method,"params":params}));
}
fn rpc(id: &Value, body: Value) {
    emit(json!({"jsonrpc":"2.0","id":id,"result":body}));
}
fn running(session: &str, prompt: &str, replay: bool) {
    notify(
        "_x.ai/queue/changed",
        json!({"sessionId":session,"runningPromptId":prompt,"_meta":{"isReplay":replay}}),
    );
}
fn finish(prompt: &str, key: &str, reason: &str) {
    notify(
        "_x.ai/session_notification",
        json!({"sessionId":"root","_meta":{"eventId":key},"update":{
        "sessionUpdate":"turn_completed","prompt_id":prompt,"stop_reason":reason,"agent_result":"quota exceeded"}}),
    );
}
fn echo(session: &str, p: &Value, replay: bool) {
    notify(
        "_x.ai/session/interjection",
        json!({"sessionId":session,"interjectionId":p["interjectionId"],"text":p["text"],"_meta":{"isReplay":replay}}),
    );
}
const WAKE: &str = "subagent-completed-same-child";

pub(super) fn peer() {
    let mut host = None;
    let mut equal = 0;
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = &v["id"];
        let p = &v["params"];
        match v["method"].as_str().unwrap() {
            "initialize" => rpc(
                id,
                json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true}}),
            ),
            "session/new" => {
                running("root", "opening-history", false);
                rpc(id, json!({"sessionId":"root"}));
                running("other", "foreign", false);
                running("root", "historical", true);
            }
            "session/prompt" => {
                let name = p["_meta"]["promptId"].as_str().unwrap().to_owned();
                assert!(host.is_none());
                if p["prompt"][0]["text"] == "held" {
                    for _ in 0..2 {
                        notify(
                            "_x.ai/queue/changed",
                            json!({"sessionId":"root","entries":[{"id":name}],"runningPromptId":null}),
                        );
                        running("root", &name, false);
                    }
                    host = Some((id.clone(), name));
                } else {
                    assert_eq!(p["prompt"][0]["text"], "after");
                    rpc(id, json!({"stopReason":"end_turn"}));
                }
            }
            "_x.ai/interject" => {
                assert!(
                    p["interjectionId"]
                        .as_str()
                        .and_then(|s| Uuid::parse_str(s).ok())
                        .is_some()
                );
                assert_eq!(p["sessionId"], "root");
                assert!(p.get("content").is_none());
                let ack = || rpc(id, json!({"result":{"status":"queued"}}));
                match p["text"].as_str().unwrap() {
                    "same" => {
                        equal += 1;
                        if equal == 1 {
                            ack();
                        }
                        echo("elsewhere", p, false);
                        echo("root", p, true);
                        echo("root", p, false);
                        echo("root", p, false);
                        if equal == 2 {
                            ack();
                        }
                    }
                    "split" => {
                        let (host_id, name) = host.take().unwrap();
                        running("root", "finished-before-answer", false);
                        finish("finished-before-answer", "early", "end_turn");
                        finish(&name, "host-event", "end_turn");
                        running("root", WAKE, false);
                        rpc(&host_id, json!({"stopReason":"end_turn"}));
                        ack();
                        echo("root", p, false);
                        notify(
                            "session/update",
                            json!({"sessionId":"root","update":{"sessionUpdate":"tool_call","toolCallId":"native-tool","title":"Native work","status":"completed"}}),
                        );
                        notify(
                            "session/update",
                            json!({"sessionId":"root","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first wake"}}}),
                        );
                    }
                    "again" => {
                        ack();
                        echo("root", p, false);
                        finish(WAKE, "first", "end_turn");
                        running("root", WAKE, false);
                        finish(WAKE, "first", "end_turn");
                        finish("foreign-prompt", "stale", "error");
                        notify(
                            "session/update",
                            json!({"sessionId":"root","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second wake"}}}),
                        );
                    }
                    "refuse" => emit(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"interjection unavailable"}}),
                    ),
                    "unknown" => rpc(id, json!({"result":{"status":"unexpected"}})),
                    "no-echo" => ack(),
                    other => panic!("unexpected steer {other}"),
                }
            }
            "session/cancel" => finish(WAKE, "second", "cancelled"),
            other => panic!("unexpected Grok request {other}"),
        }
    }
}

pub(super) async fn run() -> kar::Result<()> {
    let temp = tempfile::tempdir()?;
    let work = temp.path().join("workspace");
    std::fs::create_dir(&work)?;
    let store = ChatStore::open(temp.path().join("chats"))?;
    let mut opts = SessionOptions::new(&work);
    opts.env.insert("KAR_TEST_GROK_TURNS".into(), "1".into());
    let chat = Arc::new(
        store
            .create("grok", Some(&std::env::current_exe()?), opts)
            .await?,
    );
    assert!(matches!(chat.steer("idle").await, Err(Error::Refused(_))));
    let id = chat.info().id;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let router = kar::chat::event_router(store.clone(), id);
    let service = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut live = HttpChatFeed::new(&url)?;
    let mut offline = HttpChatFeed::new(&url)?;
    let mut a = ChatReplica::from_snapshot(live.snapshot().await?);
    let mut b = ChatReplica::from_snapshot(offline.snapshot().await?);
    let input = Uuid::new_v4();
    let owner = chat.clone();
    let sending = tokio::spawn(async move { owner.send(input, "held").await });
    for (feed, replica) in [(&mut live, &mut a), (&mut offline, &mut b)] {
        while !replica
            .turns()
            .first()
            .is_some_and(|t| t.native_input_receipt == Some(InputReceipt::Started))
        {
            replica.apply(feed.recv(replica.cursor()).await?)?;
        }
    }
    assert_eq!(a.turns(), b.turns());
    drop(offline);
    chat.steer("same").await?;
    chat.steer("same").await?;
    chat.steer("split").await?;
    assert_eq!(sending.await.unwrap()?.status, TurnStatus::Completed);
    assert!(matches!(
        chat.send(Uuid::new_v4(), "busy").await,
        Err(Error::Busy)
    ));
    chat.steer("again").await?;
    while !a.turns().last().is_some_and(|t| {
        t.items
            .iter()
            .any(|i| matches!(&i.item,Item::Message{text,..} if text=="second wake"))
    }) {
        a.apply(live.recv(a.cursor()).await?)?;
    }
    assert!(chat.steer("refuse").await.is_err());
    assert!(chat.steer("unknown").await.is_err());
    chat.steer("no-echo").await?;
    let queued = Uuid::new_v4();
    store.enqueue(id, queued, "after")?;
    chat.abort().await?;
    chat.drain_queue().await?;
    Arc::try_unwrap(chat).ok().unwrap().close().await?;
    let tip = store.feed_snapshot(id)?;
    let turns = &tip.history.turns;
    assert_eq!(turns.len(), 4, "{turns:#?}");
    assert_eq!(turns[0].id, input);
    assert_eq!(turns[0].steered_inputs.len(), 3);
    assert!(
        turns[0]
            .steered_inputs
            .values()
            .all(|s| *s == SteerStatus::Received)
    );
    assert_eq!(
        turns[0]
            .items
            .iter()
            .filter(|i| matches!(&i.item,Item::Message{role:Role::User,text,..} if text=="same"))
            .count(),
        2
    );
    assert_eq!(turns[1].native_turn_id.as_deref(), Some(WAKE));
    assert!(
        turns[1]
            .items
            .iter()
            .any(|i| matches!(&i.item,Item::Tool{id,..} if id=="native-tool"))
    );
    assert_eq!(turns[1].status, TurnStatus::Completed);
    assert_eq!(turns[2].native_turn_id.as_deref(), Some(WAKE));
    assert_ne!(turns[1].id, turns[2].id);
    assert_eq!(turns[2].status, TurnStatus::Aborted);
    assert_eq!(
        turns[2]
            .steered_inputs
            .values()
            .filter(|s| **s == SteerStatus::Unconfirmed)
            .count(),
        2
    );
    assert!(
        turns[2]
            .steered_inputs
            .values()
            .any(|s| *s == SteerStatus::Refused)
    );
    assert_eq!(turns[3].id, queued);
    let mut resumed = HttpChatFeed::new(&url)?;
    for (feed, replica) in [(&mut live, &mut a), (&mut resumed, &mut b)] {
        while replica.cursor() != tip.cursor {
            let event = feed.recv(replica.cursor()).await?;
            assert!(replica.apply(event.clone())?);
            assert!(!replica.apply(event)?);
        }
        assert_eq!(replica.turns(), turns);
    }
    service.abort();
    Ok(())
}
