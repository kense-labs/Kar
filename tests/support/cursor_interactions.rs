use kar::chat::{ChatReplica, ChatStore, HttpChatFeed, TurnStatus, event_router};
use kar::{
    ApprovalMode, Error, FieldKind, PendingRequest, PermissionDecision, QuestionReply, RequestKind,
    SessionOptions,
};
use serde_json::{Value, json};
use std::{io, time::Duration};
use uuid::Uuid;

const ASK: &str = "cursor/ask_question";
const PLAN: &str = "cursor/create_plan";

pub fn peer(input: &mut impl Iterator<Item = io::Result<String>>, interactive: bool) {
    // The official ACP handler sends no sessionId and reuses the connection for replies.
    for (id, method) in [
        (311, "cursor/update_todos"),
        (312, "cursor/task"),
        (313, "cursor/generate_image"),
    ] {
        super::emit(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":{"toolCallId":"finished"}}),
        );
        let frame: Value = serde_json::from_str(&input.next().unwrap().unwrap()).unwrap();
        assert_eq!(frame, json!({"jsonrpc":"2.0","id":id,"result":{}}));
    }
    let mut request = |method, body, expected| {
        super::emit(json!({"jsonrpc":"2.0","id":314,"method":method,"params":body}));
        let frame: Value = serde_json::from_str(&input.next().unwrap().unwrap()).unwrap();
        assert_eq!(
            frame,
            json!({"jsonrpc":"2.0","id":314,"result":{"outcome":expected}})
        );
    };
    let ask = json!({"toolCallId":"ask-1","title":"Choose settings","questions":[
        {"id":"z","prompt":"Format","options":[{"id":"compact","label":"Same"},{"id":"expanded","label":"Same"}],"allowMultiple":false},
        {"id":"a","prompt":"Platforms","options":[{"id":"mac","label":"Mac"},{"id":"win","label":"Windows"}],"allowMultiple":true}
    ]});
    request(
        ASK,
        ask.clone(),
        if interactive {
            json!({"outcome":"answered","answers":[
                {"questionId":"z","selectedOptionIds":["expanded"]},{"questionId":"a","selectedOptionIds":["win","mac"]}
            ]})
        } else {
            json!({"outcome":"cancelled"})
        },
    );
    if interactive {
        let mut followup = ask;
        followup["toolCallId"] = "ask-2".into();
        request(ASK, followup, json!({"outcome":"skipped"}));
    }
    request(
        PLAN,
        json!({"toolCallId":"plan-1","name":"Migration","overview":"Review edits","plan":"1. Edit settings","todos":[],"phases":[]}),
        json!({"outcome":if interactive {"accepted"} else {"cancelled"}}),
    );
}

async fn next_request(
    client: &mut ChatReplica,
    feed: &mut HttpChatFeed,
    call: &str,
) -> PendingRequest {
    loop {
        let event = feed.recv(client.cursor()).await.unwrap();
        assert!(client.apply(event).unwrap());
        assert!(
            client
                .pending_requests()
                .iter()
                .all(|p| matches!(p.method.as_str(), ASK | PLAN))
        );
        if let Some(p) = client
            .pending_requests()
            .iter()
            .find(|p| p.body["toolCallId"] == call)
        {
            return p.clone();
        }
    }
}

fn submit(v: Value) -> QuestionReply {
    QuestionReply::Submit {
        values: v.as_object().unwrap().clone(),
    }
}

#[tokio::main]
pub async fn run() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("workspace");
    std::fs::create_dir(&cwd).unwrap();
    let store = ChatStore::open(dir.path().join("chats")).unwrap();
    let mut opts = SessionOptions::new(&cwd);
    opts.env.insert("OAR_TEST_CURSOR_ACP".into(), "1".into());
    let program = std::env::current_exe().unwrap();
    let unattended = store
        .create("cursor", Some(&program), opts.clone())
        .await
        .unwrap();
    assert_eq!(
        unattended
            .send(Uuid::new_v4(), "unattended")
            .await
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    unattended.close().await.unwrap();
    opts.approvals = ApprovalMode::Host;
    let session = store.create("cursor", Some(&program), opts).await.unwrap();
    let id = session.info().id;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = event_router(store.clone(), id);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut first = HttpChatFeed::new(&base).unwrap();
    let mut second = HttpChatFeed::new(&base).unwrap();
    let mut a = ChatReplica::from_snapshot(first.snapshot().await.unwrap());
    let mut b = ChatReplica::from_snapshot(second.snapshot().await.unwrap());
    let review = async {
        let (pa, pb) = tokio::join!(
            next_request(&mut a, &mut first, "ask-1"),
            next_request(&mut b, &mut second, "ask-1")
        );
        assert_eq!(pa, pb);
        assert_eq!(pa.kind, RequestKind::Question);
        assert_eq!(pa.session_id.as_deref(), Some("cursor-root"));
        let form = pa.question_form().unwrap();
        assert_eq!(form.message.as_deref(), Some("Choose settings"));
        assert_eq!(form.fields[0].options[1].label, "Same");
        assert_eq!(form.fields[0].options[1].value, "expanded");
        assert_eq!(form.fields[1].kind, FieldKind::MultiSelect);
        for invalid in [
            json!({"z":"Same","a":["win"]}),
            json!({"z":"compact","a":["mac","mac"]}),
            json!({"z":"expanded"}),
            json!({"a":[],"z":"expanded"}),
        ] {
            assert!(matches!(
                session.answer_question(&pa, submit(invalid)).await,
                Err(Error::Refused(_))
            ));
        }
        let reply = submit(json!({"a":["win","mac"],"z":"expanded"}));
        let (r1, r2) = tokio::join!(
            session.answer_question(&pa, reply.clone()),
            session.answer_question(&pb, reply)
        );
        assert!(matches!(
            (r1, r2),
            (Ok(()), Err(Error::Refused(_))) | (Err(Error::Refused(_)), Ok(()))
        ));
        drop(second);
        let followup = next_request(&mut a, &mut first, "ask-2").await;
        assert_eq!(followup.id, pb.id);
        assert_ne!(followup.cursor, pb.cursor);
        assert!(matches!(
            session.answer_question(&pb, QuestionReply::Cancel).await,
            Err(Error::Refused(_))
        ));
        session
            .answer_question(&followup, QuestionReply::Decline)
            .await
            .unwrap();
        let plan = next_request(&mut a, &mut first, "plan-1").await;
        assert_eq!(plan.kind, RequestKind::Permission);
        assert_eq!(plan.display_text().as_deref(), Some("1. Edit settings"));
        assert_eq!(
            plan.decisions(),
            [
                PermissionDecision::AllowOnce,
                PermissionDecision::Deny,
                PermissionDecision::Cancel
            ]
        );
        session
            .decide(&plan, PermissionDecision::AllowOnce)
            .await
            .unwrap();
    };
    let (turn, _) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(session.send(Uuid::new_v4(), "review"), review)
    })
    .await
    .unwrap();
    assert_eq!(turn.unwrap().status, TurnStatus::Completed);
    session.close().await.unwrap();
    let final_state = store.feed_snapshot(id).unwrap();
    let mut reconnected = HttpChatFeed::new(&base).unwrap();
    for (client, feed) in [(&mut a, &mut first), (&mut b, &mut reconnected)] {
        while client.cursor() != final_state.cursor {
            let event = tokio::time::timeout(Duration::from_secs(3), feed.recv(client.cursor()))
                .await
                .unwrap()
                .unwrap();
            assert!(client.apply(event.clone()).unwrap());
            assert!(!client.apply(event).unwrap());
        }
        assert!(client.pending_requests().is_empty());
        assert_eq!(client.turns(), final_state.history.turns);
    }
    server.abort();
}
