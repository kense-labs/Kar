use kar::chat::{ChatReplica, ChatStore, HttpChatFeed, Item, Role, TurnStatus, event_router};
use kar::{SessionOptions, TaskStatus};
use serde_json::{Value, json};
use std::{fs, io::BufRead, time::Duration};
use uuid::Uuid;

const ROOT: &str = "cursor-root";
const CHILD: &str = "agent-a";
const LEAF: &str = "agent-b";
const LATE: &str = "agent-c";

fn update(session: &str, body: Value) {
    super::emit(
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":body}}),
    );
}
fn text(session: &str, text: &str) {
    update(
        session,
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}),
    );
}
fn spawn(parent: &str, child: &str, call: &str) {
    update(
        parent,
        json!({"sessionUpdate":"subagent_spawned","subagentSessionId":child,"name":"explore","task":format!("Inspect {child}"),"capabilities":{},"_meta":{"cursor":{"toolCallId":call,"agentId":child.split('.').next().unwrap()}}}),
    );
}
fn end(parent: &str, child: &str, state: &str) {
    update(
        parent,
        json!({"sessionUpdate":"subagent_state_update","subagentSessionId":child,"state":state}),
    );
}
fn tool(parent: &str, call: &str) {
    update(
        parent,
        json!({"sessionUpdate":"tool_call","toolCallId":call,"title":"Task","kind":"other","status":"in_progress"}),
    );
}

pub fn peer() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("2026.11.22-e9042b2");
        return;
    }
    let mut input = std::io::stdin().lock().lines();
    while let Some(line) = input.next() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let reply =
            |result: Value| super::emit(json!({"jsonrpc":"2.0","id":v["id"],"result":result}));
        match v["method"].as_str().unwrap() {
            "initialize" => {
                assert_eq!(
                    v.pointer("/params/clientCapabilities/_meta/subagents"),
                    Some(&Value::Bool(true))
                );
                reply(
                    json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{},"subagents":{}}}}),
                )
            }
            "session/list" => reply(
                json!({"sessions":[{"sessionId":ROOT,"cwd":std::env::current_dir().unwrap()}]}),
            ),
            "session/new" => reply(json!({"sessionId":ROOT})),
            "session/load" => {
                spawn(ROOT, CHILD, "replay-0-1");
                spawn(CHILD, LEAF, "replay-0-3");
                end(CHILD, LEAF, "failed");
                end(ROOT, CHILD, "completed");
                spawn(ROOT, LATE, "replay-0-5");
                end(ROOT, LATE, "completed");
                text(ROOT, "historical duplicate");
                reply(json!({"sessionId":ROOT}));
            }
            "session/prompt" => {
                match v["params"]["prompt"][0]["text"].as_str().unwrap() {
                    "first" => {
                        tool(ROOT, "call-a");
                        spawn(ROOT, CHILD, "call-a");
                        spawn(ROOT, CHILD, "call-a");
                        text(CHILD, "child ");
                        tool(CHILD, "call-b");
                        spawn(CHILD, LEAF, "call-b");
                        text(LEAF, "leaf");
                        let until = std::time::Instant::now() + Duration::from_secs(10);
                        while !std::path::Path::new("client-saw-leaf").exists() {
                            assert!(
                                std::time::Instant::now() < until,
                                "SSE client did not observe the live child"
                            );
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        end(ROOT, LEAF, "completed"); // The root is not this child's parent.
                        end(CHILD, LEAF, "failed");
                        end(CHILD, LEAF, "failed");
                        tool(ROOT, "call-late");
                        text(ROOT, "first root");
                    }
                    "second" => {
                        spawn(ROOT, LATE, "call-late"); // Late spawn belongs to the first turn's tool.
                        text(CHILD, "tail");
                        end(ROOT, CHILD, "completed");
                        text(LATE, "background");
                        update(
                            ROOT,
                            json!({"sessionUpdate":"tool_call_update","toolCallId":"call-late","status":"completed","content":[{"type":"content","content":{"type":"text","text":"late result"}}]}),
                        );
                        text("unannounced", "foreign output");
                        text(ROOT, "second root");
                    }
                    "third" => {
                        tool(ROOT, "again");
                        spawn(ROOT, "agent-a.2", "again");
                        text("agent-a.2", "new child run");
                        end(ROOT, "agent-a.2", "disconnected");
                        text(ROOT, "third root");
                    }
                    mode @ ("review" | "unattended") => {
                        super::cursor_interactions::peer(&mut input, mode == "review");
                    }
                    _ => panic!("unknown prompt"),
                }
                reply(json!({"stopReason":"end_turn"}));
            }
            other => panic!("unexpected Cursor request {other}"),
        }
    }
}

async fn observe_leaf(client: &mut ChatReplica, feed: &mut HttpChatFeed) {
    loop {
        let event = feed.recv(client.cursor()).await.unwrap();
        assert!(client.apply(event).unwrap());
        if let Some(turn) = client.turns().first()
            && turn.items.iter().any(|item| {
                item.source.session_id.as_deref() == Some(LEAF)
                    && matches!(&item.item,Item::Message{text,..} if text=="leaf")
            })
        {
            assert_eq!(turn.status, TurnStatus::Running);
            assert_eq!(
                client.tasks().iter().find(|t| t.id == LEAF).unwrap().status,
                TaskStatus::Running
            );
            return;
        }
    }
}

#[tokio::main]
pub async fn run() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("workspace");
    fs::create_dir(&cwd).unwrap();
    let store = ChatStore::open(dir.path().join("chats")).unwrap();
    let setup = |opts: &mut SessionOptions| {
        opts.env.insert("OAR_TEST_CURSOR_ACP".into(), "1".into());
    };
    let mut opts = SessionOptions::new(&cwd);
    setup(&mut opts);
    let session = store
        .create("cursor", Some(&std::env::current_exe().unwrap()), opts)
        .await
        .unwrap();
    let id = session.info().id;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = event_router(store.clone(), id);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut feed = HttpChatFeed::new(&base).unwrap();
    let mut early = ChatReplica::from_snapshot(feed.snapshot().await.unwrap());
    let mut second_feed = HttpChatFeed::new(&base).unwrap();
    let mut offline = ChatReplica::from_snapshot(second_feed.snapshot().await.unwrap());
    let observe = async {
        tokio::join!(
            observe_leaf(&mut early, &mut feed),
            observe_leaf(&mut offline, &mut second_feed)
        );
        drop(second_feed);
        fs::write(cwd.join("client-saw-leaf"), "ok").unwrap();
    };
    let (first, observed) = tokio::join!(
        session.send(Uuid::new_v4(), "first"),
        tokio::time::timeout(Duration::from_secs(10), observe)
    );
    observed.unwrap();
    first.unwrap();
    session.send(Uuid::new_v4(), "second").await.unwrap();
    assert_eq!(session.snapshot().graph.edges.len(), 3);
    session.close().await.unwrap();
    let before = store.history(id).unwrap().tasks;
    let known = before.iter().find(|t| t.id == CHILD).unwrap();
    assert_eq!(known.status, TaskStatus::Completed);
    let session = store.resume_with(id, setup).await.unwrap();
    let resumed = session.history().unwrap();
    let prior = resumed.tasks.iter().find(|t| t.id == CHILD).unwrap();
    assert_eq!(prior, known, "history replay reset a settled task");
    let late = resumed.tasks.iter().find(|t| t.id == LATE).unwrap();
    assert_eq!(late.status, TaskStatus::Completed);
    assert!(!late.ended_on_exit);
    session.send(Uuid::new_v4(), "third").await.unwrap();
    session.close().await.unwrap();
    // Recover a crash between the durable task receipt and its turn binding.
    let filename = format!("{id}.oarchat");
    let log = fs::read_to_string(dir.path().join("chats").join(&filename)).unwrap();
    let mut prefix = String::new();
    for line in log.lines() {
        prefix.push_str(line);
        prefix.push('\n');
        let row: Value = serde_json::from_str(line).unwrap();
        if row["kind"] == "tasks_changed"
            && row["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|task| task["id"] == CHILD)
        {
            break;
        }
    }
    let partial_dir = dir.path().join("partial");
    let partial = ChatStore::open(&partial_dir).unwrap();
    fs::write(partial_dir.join(filename), prefix).unwrap();
    fs::File::create(partial_dir.join(format!("{id}.lock"))).unwrap();
    assert!(
        !partial.history(id).unwrap().turns[0]
            .child_sessions
            .contains_key(CHILD)
    );
    let recovered = partial.resume_with(id, setup).await.unwrap();
    let repaired = recovered.history().unwrap();
    assert_eq!(repaired.turns[0].child_sessions[CHILD], ROOT);
    assert_eq!(
        repaired
            .tasks
            .iter()
            .find(|t| t.id == CHILD)
            .unwrap()
            .status,
        TaskStatus::Completed
    );
    recovered.close().await.unwrap();

    let snapshot = store.feed_snapshot(id).unwrap();
    let mut after_disconnect = HttpChatFeed::new(&base).unwrap();
    for (client, events) in [
        (&mut early, &mut feed),
        (&mut offline, &mut after_disconnect),
    ] {
        while client.cursor() != snapshot.cursor {
            let event = tokio::time::timeout(Duration::from_secs(3), events.recv(client.cursor()))
                .await
                .unwrap()
                .unwrap();
            assert!(client.apply(event.clone()).unwrap());
            assert!(!client.apply(event).unwrap());
        }
        let turns = client.turns();
        assert_eq!(turns.len(), 3);
        assert!(turns.iter().all(|t| t.status == TurnStatus::Completed));
        assert_eq!(turns[0].child_sessions[CHILD], ROOT);
        assert_eq!(turns[0].child_sessions[LEAF], CHILD);
        assert_eq!(turns[0].child_sessions[LATE], ROOT);
        assert!(turns[1].child_sessions.is_empty());
        assert!(
            !turns[1]
                .items
                .iter()
                .any(|i| matches!(&i.item, Item::Tool{id,..} if id=="call-late"))
        );
        assert!(turns[0].items.iter().any(
            |i| matches!(&i.item,Item::Tool{id,activity} if id=="call-late" && activity.finished)
        ));
        assert_eq!(client.tasks().len(), 4);
        assert_eq!(turns[2].child_sessions["agent-a.2"], ROOT);
        let mut messages = Vec::new();
        for (index, turn) in turns.iter().enumerate() {
            for item in &turn.items {
                if let Item::Message {
                    role: Role::Assistant,
                    text,
                    complete,
                    ..
                } = &item.item
                {
                    assert!(complete, "unfinished {text}");
                    messages.push((
                        index,
                        item.source.session_id.as_deref().unwrap(),
                        item.source.path.join("/"),
                        text.as_str(),
                    ));
                }
            }
        }
        assert!(messages.iter().any(|(i, s, path, text)| *i == 0
            && *s == LEAF
            && path == "call-a/call-b"
            && *text == "leaf"));
        assert!(messages.iter().any(|(i, s, path, text)| *i == 0
            && *s == LATE
            && path == "call-late"
            && *text == "background"));
        assert_eq!(
            messages
                .iter()
                .filter(|(_, s, _, _)| *s == CHILD)
                .map(|(_, _, _, t)| *t)
                .collect::<String>(),
            "child tail"
        );
        assert!(messages.iter().any(|(i, s, path, t)| *i == 1
            && *s == ROOT
            && path.is_empty()
            && *t == "second root"));
        assert!(
            !messages
                .iter()
                .any(|(_, _, _, t)| t.contains("foreign") || t.contains("duplicate"))
        );
        let leaf = client.tasks().iter().find(|t| t.id == LEAF).unwrap();
        assert_eq!(leaf.status, TaskStatus::Failed);
        assert_eq!(leaf.details.tool_call_id.as_deref(), Some("call-b"));
        assert_eq!(leaf.source.session_id.as_deref(), Some(CHILD));
        assert_eq!(leaf.source.path, ["call-a"]);
        assert_eq!(
            client
                .tasks()
                .iter()
                .find(|t| t.id == "agent-a.2")
                .unwrap()
                .status,
            TaskStatus::Disconnected
        );
        assert_eq!(
            serde_json::to_value(turns).unwrap(),
            serde_json::to_value(&snapshot.history.turns).unwrap()
        );
    }
    server.abort();
}
