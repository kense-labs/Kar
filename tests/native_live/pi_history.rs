#[path = "../support/adapter_queue.rs"]
mod adapter_queue;
#[path = "../support/busy_prompt.rs"]
mod busy_prompt;
#[path = "pi_history/extension_runs.rs"]
mod extension_runs;
use super::{interrupted_copy, respond_in_workspace, sse::SseClient};
use kar::{
    Event, SessionOptions,
    chat::{
        ChatEdit, ChatFeedSnapshot, ChatReplica, ChatStore, ChatUpdate, Item, Role, TurnStatus,
    },
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

struct Fixture {
    temp: tempfile::TempDir,
    options: SessionOptions,
    calls: Arc<Mutex<Vec<Value>>>,
    model: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.model.abort();
    }
}

async fn fixture(retry: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let work = base.join("work");
    let agent = base.join("agent");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&agent).unwrap();
    let file = work.join("sample.txt");
    fs::write(&file, "native-content-marker").unwrap();
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let observed = calls.clone();
    let model = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let s = socket.into_std().unwrap();
            s.set_nonblocking(false).unwrap();
            let (file, observed) = (file.clone(), observed.clone());
            tokio::task::spawn_blocking(move || {
                respond_in_workspace(
                    s,
                    (!retry).then_some(file.as_path()),
                    Some(&observed),
                    file.parent(),
                    false,
                )
            });
        }
    });
    let cfg = json!({"providers":{"fixture":{"baseUrl":endpoint,"api":"anthropic-messages","apiKey":"test-credential","models":[{"id":"fixture","name":"Fixture","reasoning":false,"input":["text"],"contextWindow":200000,"maxTokens":8192}]}}});
    fs::write(agent.join("models.json"), cfg.to_string()).unwrap();
    fs::write(
        agent.join("settings.json"),
        json!({"compaction":{"enabled":false},"retry":{"enabled":retry,"maxRetries":1,"baseDelayMs":350}}).to_string(),
    )
    .unwrap();
    let mut options = SessionOptions::new(&work);
    options.env.insert("KAR_PI_AGENT_DIR".into(), agent.into());
    for k in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ] {
        options.env.insert(k.into(), base.join(k).into());
    }
    options.env.insert("PI_OFFLINE".into(), "1".into());
    options.model = Some("fixture/fixture".into());
    Fixture {
        temp,
        options,
        calls,
        model,
    }
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN; checks real Pi forks, workspace rollback and model input"]
async fn fork_at_turn_restores_workspace_and_native_context() {
    let fixture = fixture(true).await;
    let base = fixture.temp.path();
    let store = ChatStore::open(base.join("chats")).unwrap();
    let bin = std::env::var_os("KAR_PI_BIN").unwrap();
    let chat = store
        .create("pi", Some(Path::new(&bin)), fixture.options.clone())
        .await
        .unwrap();
    let parent = chat.info().id;
    let source = chat.info().native_id.clone();
    let orange = chat.send(Uuid::new_v4(), "orange-original").await.unwrap();
    assert_eq!(orange.status, TurnStatus::Completed);
    assert!(
        orange.fork_anchor.is_some(),
        "missing Pi end-of-turn anchor"
    );
    let blue = chat
        .send(Uuid::new_v4(), "blue-continuation")
        .await
        .unwrap();
    assert_ne!(orange.fork_anchor, blue.fork_anchor);
    assert_eq!(
        fs::read_to_string(base.join("work/version")).unwrap(),
        "blue"
    );
    chat.close().await.unwrap();
    // Recreate a pre-anchor chat: the native input receipt and final workspace
    // checkpoint are present, but no Pi fork cursor has been recorded.
    let log = base.join("chats").join(format!("{parent}.karchat"));
    let mut rows = String::new();
    for line in fs::read_to_string(&log).unwrap().lines() {
        let mut row: Value = serde_json::from_str(line).unwrap();
        if row["id"] == orange.id.to_string()
            && let Some(edits) = row
                .pointer_mut("/frame/edits")
                .and_then(Value::as_array_mut)
        {
            edits.retain(|e| e["event"]["kind"] != "fork_anchor");
        }
        rows.push_str(&row.to_string());
        rows.push('\n');
    }
    fs::write(&log, rows).unwrap();
    assert!(
        store.history(parent).unwrap().turns[0]
            .fork_anchor
            .is_none()
    );
    let mut runtime = kar::runtime("pi").unwrap();
    runtime.program = bin.clone().into();
    let mut original = fixture.options.clone();
    original.resume = Some(source.clone());
    let opened = runtime.open(original).await.unwrap();
    let parent_file = opened.native_session_file().await.unwrap().unwrap();
    opened.close().await.unwrap();
    let parent_before = fs::read(&parent_file).unwrap();
    assert!(
        runtime
            .fork(&source, Some("unknown-anchor"), fixture.options.clone())
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(base.join("work/version")).unwrap(),
        "blue"
    );
    let child = store
        .fork_at_with(parent, orange.id, |o| o.env = fixture.options.env.clone())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(base.join("work/version")).unwrap(),
        "orange"
    );
    let child_id = child.info().id;
    assert_ne!(child.info().native_id, source);
    assert_eq!(
        child.info().forked_from.as_ref().unwrap().native_source,
        source
    );
    assert_eq!(fs::read(&parent_file).unwrap(), parent_before);
    assert_eq!(child.history().unwrap().turns.len(), 1);
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    child.close().await.unwrap();
    let child = store
        .resume_with(child_id, |o| o.env = fixture.options.env.clone())
        .await
        .unwrap();
    let green = child.send(Uuid::new_v4(), "green-branch").await.unwrap();
    assert_eq!(green.status, TurnStatus::Completed);
    assert_eq!(
        fs::read_to_string(base.join("work/version")).unwrap(),
        "green"
    );
    let sent = fixture.calls.lock().unwrap()[2].to_string();
    assert!(
        sent.contains("orange-original")
            && sent.contains("second-block")
            && sent.contains("green-branch")
    );
    assert!(!sent.contains("blue-continuation") && !sent.contains("blue-answer-only"));
    assert_eq!(store.history(parent).unwrap().turns.len(), 2);
    let grandchild = child.fork_at(green.id).await.unwrap();
    assert_ne!(grandchild.info().native_id, child.info().native_id);
    let continued = grandchild
        .send(Uuid::new_v4(), "continue from green")
        .await
        .unwrap();
    assert_eq!(continued.status, TurnStatus::Completed);
    let sent = fixture.calls.lock().unwrap()[3].to_string();
    assert!(
        sent.contains("green-branch")
            && sent.contains("orange-original")
            && !sent.contains("blue-continuation")
    );
    grandchild.close().await.unwrap();
    child.close().await.unwrap();
    let clone = runtime
        .fork(&source, None, fixture.options.clone())
        .await
        .unwrap();
    let cloned_file = clone.native_session_file().await.unwrap().unwrap();
    let raw = fs::read_to_string(cloned_file).unwrap();
    let header: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    assert_eq!(
        Path::new(header["parentSession"].as_str().unwrap()),
        parent_file
    );
    assert_ne!(clone.id(), source);
    assert!(raw.contains("blue-continuation") && !raw.contains("green-branch"));
    assert!(
        !clone
            .records()
            .iter()
            .any(|r| r.native.to_string().contains("blue-continuation"))
    );
    clone.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN; real Pi v3 transcript, native read tool and two HTTP clients"]
async fn restore_native_turn_and_reconnect_both_clients() {
    let fixture = fixture(false).await;
    let base = fixture.temp.path();
    let options = &fixture.options;
    let calls = &fixture.calls;
    let store = ChatStore::open(base.join("original")).unwrap();
    let bin = std::env::var_os("KAR_PI_BIN").unwrap();
    let chat = store
        .create("pi", Some(Path::new(&bin)), options.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let initial = store.feed_snapshot(id).unwrap();
    let start = initial.cursor;
    let initial = serde_json::to_value(initial).unwrap();
    let key = Uuid::new_v4();
    let prompt = "kar-pi-recover";
    let original = chat.send(key, prompt).await.unwrap();
    assert_eq!(original.status, TurnStatus::Completed);
    assert!(original.pi_input_timestamp_ms.is_some());
    let expected_calls = calls.lock().unwrap().len();
    assert_eq!(expected_calls, 2);
    let mut updates = store.follow(id, Some(start)).unwrap();
    let after = store.feed_snapshot(id).unwrap().cursor;
    let mut cuts = Vec::new();
    while updates.cursor() != after {
        let update = updates.recv().await.unwrap();
        let label = match &update.update {
            ChatUpdate::InputSent { .. } => Some("before-echo"),
            ChatUpdate::PiInput { .. } => Some("after-echo"),
            ChatUpdate::Frame { frame, .. }
                if frame.edits.iter().any(|e| {
                    matches!(
                        e,
                        ChatEdit::Activity {
                            event: Event::ToolStarted { .. }
                        }
                    )
                }) =>
            {
                Some("tool-started")
            }
            ChatUpdate::Frame { frame, .. }
                if frame.edits.iter().any(|e| {
                    matches!(
                        e,
                        ChatEdit::Text {
                            role: Role::Assistant,
                            final_text: false,
                            ..
                        }
                    )
                }) =>
            {
                Some("partial-reply")
            }
            _ => None,
        };
        if let Some(label) = label
            && !cuts.iter().any(|(name, _)| *name == label)
        {
            cuts.push((label, update.cursor));
        }
    }
    assert_eq!(cuts.len(), 4, "{cuts:?}");
    // The same words appear twice in Pi, with independent native user echoes.
    let repeated = chat.send(Uuid::new_v4(), prompt).await.unwrap();
    assert_ne!(
        repeated.pi_input_timestamp_ms,
        original.pi_input_timestamp_ms
    );
    chat.close().await.unwrap();
    let baseline = calls.lock().unwrap().len();
    for (label, cut) in cuts {
        let store = interrupted_copy(&base.join("original"), &base.join(label), id, cut);
        let listen = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listen.local_addr().unwrap());
        let router = kar::chat::event_router(store.clone(), id);
        let http = tokio::spawn(async move { axum::serve(listen, router).await.unwrap() });
        let mut a = ChatReplica::from_snapshot(
            serde_json::from_value::<ChatFeedSnapshot>(initial.clone()).unwrap(),
        );
        let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
        let mut live = SseClient::open(&url, &b.cursor().to_string(), None).await;
        let session = store
            .resume_with(id, |o| o.env = options.env.clone())
            .await
            .unwrap();
        let actual = session.send(key, prompt).await.unwrap();
        if label == "before-echo" {
            assert_eq!(actual.status, TurnStatus::Interrupted);
        } else {
            assert_eq!(
                actual.status,
                TurnStatus::Completed,
                "{label}: {:?}",
                actual.items
            );
            assert!(actual.input_sent && actual.native_turn_id.is_some());
            assert_eq!(
                actual
                    .items
                    .iter()
                    .filter(|i| matches!(
                        &i.item,
                        Item::Message {
                            role: Role::User,
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert_eq!(actual.items.iter().filter(|i|matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="rust-wire-ok")).count(),1);
            let tools: Vec<_> = actual
                .items
                .iter()
                .filter_map(|i| {
                    if let Item::Tool { id, activity } = &i.item {
                        Some((id, activity))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(
                tools.len(),
                1,
                "{label}: {:?}",
                actual
                    .items
                    .iter()
                    .filter_map(|i| match &i.item {
                        Item::Tool { id, activity } => Some((id, &i.source, activity.finished)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            );
            assert_eq!(tools[0].0, "pi_read_1");
            assert!(
                serde_json::to_string(&tools[0].1.result)
                    .unwrap()
                    .contains("native-content-marker")
            );
            assert!(actual.checkpoint.as_ref().unwrap().after_tree.is_none());
        }
        session.close().await.unwrap();
        let again = store
            .resume_with(id, |o| o.env = options.env.clone())
            .await
            .unwrap();
        again.close().await.unwrap();
        let tip = store.feed_snapshot(id).unwrap().cursor;
        let mut reconnected =
            SseClient::open(&url, &b.cursor().to_string(), Some(&a.cursor().to_string())).await;
        for (replica, client) in [(&mut a, &mut reconnected), (&mut b, &mut live)] {
            let mut repairs = 0;
            while replica.cursor() != tip {
                let event = client.next().await;
                repairs += usize::from(matches!(event.update, ChatUpdate::Reconciled { .. }));
                assert!(replica.apply(event.clone()).unwrap());
                assert!(!replica.apply(event).unwrap());
            }
            assert_eq!(repairs, usize::from(label != "before-echo"));
            assert_eq!(replica.turns(), std::slice::from_ref(&actual));
        }
        assert_eq!(calls.lock().unwrap().len(), baseline);
        http.abort();
    }
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN; restores a retried turn from its real Pi v3 transcript"]
async fn recover_after_a_native_retry_without_resending() {
    let fixture = fixture(true).await;
    let base = fixture.temp.path();
    let store = ChatStore::open(base.join("original")).unwrap();
    let bin = std::env::var_os("KAR_PI_BIN").unwrap();
    let chat = store
        .create("pi", Some(Path::new(&bin)), fixture.options.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let mut updates = store
        .follow(id, Some(store.feed_snapshot(id).unwrap().cursor))
        .unwrap();
    let key = Uuid::new_v4();
    let prompt = "kar-retry-once";
    assert_eq!(
        chat.send(key, prompt).await.unwrap().status,
        TurnStatus::Completed
    );
    let tip = store.feed_snapshot(id).unwrap().cursor;
    let mut receipt = None;
    while updates.cursor() != tip {
        let update = updates.recv().await.unwrap();
        if matches!(update.update, ChatUpdate::PiInput { .. }) {
            receipt = Some(update.cursor);
        }
    }
    chat.close().await.unwrap();
    let resumed = interrupted_copy(
        &base.join("original"),
        &base.join("after-echo"),
        id,
        receipt.unwrap(),
    )
    .resume_with(id, |o| o.env = fixture.options.env.clone())
    .await
    .unwrap();
    let recovered = resumed.send(key, prompt).await.unwrap();
    assert_eq!(recovered.status, TurnStatus::Completed);
    assert!(recovered.failure.is_none());
    assert_eq!(recovered.items.iter().filter(|i| matches!(&i.item, Item::Message { role: Role::Assistant, text, .. } if text == "rust-wire-ok")).count(), 1);
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    resumed.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN with native retry and settlement signals; loopback model only"]
async fn queue_waits_for_native_retry_to_settle() {
    use kar::Outcome;
    use tokio::time::{Duration, timeout};
    let fixture = fixture(true).await;
    let mut runtime = kar::runtime("pi").unwrap();
    runtime.program = std::env::var_os("KAR_PI_BIN").unwrap().into();
    let session = runtime.open(fixture.options.clone()).await.unwrap();
    let mut stream = session.subscribe(None).unwrap();
    let turn = session.prompt("kar-retry-once").await.unwrap();
    timeout(Duration::from_secs(20), async {
        while let Some(record) = stream.next().await {
            if record.native["type"] == "auto_retry_start" {
                return;
            }
            assert!(
                !record
                    .events
                    .iter()
                    .any(|e| matches!(e, Event::TurnEnded { .. })),
                "ended before retry"
            );
        }
        panic!("Pi closed before retry");
    })
    .await
    .unwrap();
    let (_, next) = session.queue("queued after retry").await.unwrap();
    let first = timeout(Duration::from_secs(20), turn.report())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.outcome, Outcome::Completed);
    assert!(first.failure.is_none());
    assert_eq!(first.text, "rust-wire-ok");
    assert_eq!(
        timeout(Duration::from_secs(20), next.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Completed
    );
    let records = session.records();
    let finals: Vec<_> = records
        .iter()
        .filter(|r| {
            r.events
                .iter()
                .any(|e| matches!(e, Event::TurnEnded { .. }))
        })
        .collect();
    assert_eq!(finals.len(), 2);
    assert!(finals.iter().all(|r| {
        r.native["type"] == "agent_settled"
            && r.events
                .iter()
                .any(|e| matches!(e, Event::ContextUsage { .. }))
    }));
    let next_prompt = records
        .iter()
        .find(|r| r.native["type"] == "prompt" && r.native["message"] == "queued after retry")
        .unwrap();
    assert!(finals[0].seq < next_prompt.seq && next_prompt.seq < finals[1].seq);
    assert!(!records.iter().any(|r| {
        r.events
            .iter()
            .any(|e| matches!(e, Event::TurnFailure { .. }))
    }));
    let calls = fixture.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0], calls[1]);
    assert!(calls[2].to_string().contains("queued after retry"));
    session.close().await.unwrap();
}

// The model requests a real Pi file write for each stage. Its final response lists
// only stage markers found in the native request so stale contexts are observable.
pub(super) fn checkpoint_response(request: &Value) -> Option<Vec<Value>> {
    let messages = request["messages"].as_array()?;
    let mut stages = Vec::new();
    let mut finished = false;
    let mut active = false;
    for msg in messages.iter().filter(|msg| msg["role"] == "user") {
        let mut visit = |text: &str| {
            active = false;
            if let Some(stage) = text.strip_prefix("kar-fork-stage ")
                && ["first", "middle", "late", "child", "inspect"].contains(&stage)
            {
                active = true;
                stages.push(stage.to_owned());
                finished = false;
            }
        };
        if let Some(text) = msg["content"].as_str() {
            visit(text);
        } else if let Some(parts) = msg["content"].as_array() {
            for text in parts.iter().filter_map(|p| p["text"].as_str()) {
                visit(text);
            }
            if parts.iter().any(|part| part["type"] == "tool_result") {
                finished = true;
            }
        }
    }
    if !active {
        return None;
    }
    let stage = stages.last()?;
    let (block, delta, stop) = if finished || stage == "inspect" {
        let reply = stages.join("/");
        (
            json!({"type":"text","text":""}),
            json!({"type":"text_delta","text":reply}),
            "end_turn",
        )
    } else {
        let args = json!({"path":format!("{stage}.txt"),"content":format!("file for {stage}\n")});
        (
            json!({"type":"tool_use","id":format!("toolu_{stage}"),"name":"write","input":{}}),
            json!({"type":"input_json_delta","partial_json":args.to_string()}),
            "tool_use",
        )
    };
    Some(vec![
        json!({"type":"message_start","message":{"id":format!("msg_{}",Uuid::new_v4().simple()),"role":"assistant","type":"message","model":request["model"],"content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_delta","index":0,"delta":delta}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ])
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN; native write tools in three turns and a mid-history fork"]
async fn three_native_writes_fork_at_middle_and_reopen_both_contexts() {
    use kar::chat::{ChangeKind, ChatTurn};
    let fixture = fixture(true).await;
    let root = fixture.temp.path();
    let work = root.join("work");
    let store = ChatStore::open(root.join("chats")).unwrap();
    let bin = std::env::var_os("KAR_PI_BIN").unwrap();
    let parent = store
        .create("pi", Some(Path::new(&bin)), fixture.options.clone())
        .await
        .unwrap();
    let parent_id = parent.info().id;
    let source = parent.info().native_id.clone();
    let mut turns = Vec::new();
    let wrote = |turn: &ChatTurn, stage: &str| {
        assert_eq!(turn.status, TurnStatus::Completed);
        let checkpoint = turn.checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.files.len(), 1, "stage {stage}");
        assert_eq!(checkpoint.files[0].kind, ChangeKind::Added);
        assert_eq!(checkpoint.files[0].path, Path::new(&format!("{stage}.txt")));
        assert_eq!(
            fs::read_to_string(work.join(format!("{stage}.txt"))).unwrap(),
            format!("file for {stage}\n")
        );
        assert!(
            turn.items
                .iter()
                .any(|item| matches!(&item.item, Item::Tool { .. }))
        );
    };
    let replied = |turn: &ChatTurn, expected: &str| {
        assert!(turn.items.iter().any(|item|matches!(&item.item,Item::Message{role:Role::Assistant,text,..} if text==expected)),
            "model did not return stage sequence {expected}");
    };
    for stage in ["first", "middle", "late"] {
        let turn = parent
            .send(Uuid::new_v4(), format!("kar-fork-stage {stage}"))
            .await
            .unwrap();
        wrote(&turn, stage);
        turns.push(turn);
    }
    replied(&turns[2], "first/middle/late");
    parent.close().await.unwrap();
    let child = store
        .fork_at_with(parent_id, turns[1].id, |o| {
            o.env = fixture.options.env.clone()
        })
        .await
        .unwrap();
    assert_ne!(child.info().native_id, source);
    assert_eq!(
        child
            .history()
            .unwrap()
            .turns
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [turns[0].id, turns[1].id]
    );
    for name in ["first", "middle"] {
        assert_eq!(
            fs::read_to_string(work.join(format!("{name}.txt"))).unwrap(),
            format!("file for {name}\n")
        );
    }
    assert!(!work.join("late.txt").exists());
    let child_id = child.info().id;
    child.close().await.unwrap();
    drop(store);
    let store = ChatStore::open(root.join("chats")).unwrap();
    let child = store
        .resume_with(child_id, |o| o.env = fixture.options.env.clone())
        .await
        .unwrap();
    let turn = child
        .send(Uuid::new_v4(), "kar-fork-stage child")
        .await
        .unwrap();
    wrote(&turn, "child");
    replied(&turn, "first/middle/child");
    child.close().await.unwrap();
    let parent = store
        .resume_with(parent_id, |o| o.env = fixture.options.env.clone())
        .await
        .unwrap();
    assert_eq!(parent.info().native_id, source);
    let turn = parent
        .send(Uuid::new_v4(), "kar-fork-stage inspect")
        .await
        .unwrap();
    replied(&turn, "first/middle/late/inspect");
    parent.close().await.unwrap();
}

fn enable_input_transform(agent: &Path) {
    let ext = agent.join("extensions");
    fs::create_dir(&ext).unwrap();
    fs::copy(
        std::env::var_os("KAR_PI_INPUT_EXTENSION").unwrap(),
        ext.join("input-transform.ts"),
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "requires Pi RPC and its published examples/extensions/input-transform.ts fixture"]
async fn native_handled_prompt_settles_without_a_model_turn() {
    let f = fixture(true).await;
    enable_input_transform(&f.temp.path().join("agent"));
    let mut rt = kar::runtime("pi").unwrap();
    rt.program = std::env::var_os("KAR_PI_BIN").unwrap().into();
    let s = rt.open(f.options.clone()).await.unwrap();
    let id = Uuid::new_v4();
    let start = s.records().len();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = s.subscribe(None).unwrap();
    let listener = tokio::spawn(async move {
        while let Some(row) = events.next().await {
            tx.send(row).unwrap();
        }
    });
    for _ in 0..2 {
        let t = s.prompt_with_id(id, "ping").await.unwrap();
        let report = tokio::time::timeout(std::time::Duration::from_secs(5), t.report())
            .await
            .expect("native handled input must settle without agent_start")
            .unwrap();
        assert_eq!(report.outcome, kar::Outcome::Completed);
        assert_eq!(
            report.native_input_receipt,
            Some(kar::InputReceipt::Handled)
        );
        assert!(matches!(
            s.status(),
            kar::observe::AgentStatus::Idle {
                last_outcome: None,
                ..
            }
        ));
    }
    let rows = s.records();
    let rows = &rows[start..];
    let requests: Vec<_> = rows.iter().filter(|r| r.prompt.is_some()).collect();
    assert_eq!(requests.len(), 2);
    let mut ids = std::collections::HashSet::new();
    for req in requests {
        let identity = req.prompt.as_ref().unwrap();
        let request = identity
            .request_id
            .as_deref()
            .expect("missing public request identity");
        assert!(
            ids.insert(request.to_owned()),
            "retried input reused its control request identity"
        );
        assert_eq!(identity.input_id, id.to_string());
        assert_eq!(
            rows.iter()
                .filter(|r| r.native["type"] == "accepted"
                    && r.native["requestId"] == request
                    && r.seq > req.seq)
                .count(),
            1
        );
    }
    assert_eq!(
        rows.iter()
            .filter(
                |r| r.native["command"] == "prompt" && r.native["data"]["disposition"] == "handled"
            )
            .count(),
        2
    );
    assert!(
        !rows
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e, Event::TurnStarted | Event::TurnEnded { .. }))
    );
    assert!(f.calls.lock().unwrap().is_empty());
    let held = s.prompt("kar-queue-held-at-model").await.unwrap();
    busy_prompt::check(&s, held.input_id()).await;
    std::fs::write(f.options.cwd.join("release"), "").unwrap();
    assert_eq!(held.wait().await.unwrap(), kar::Outcome::Completed);
    assert!(!s.status().is_running());
    assert_eq!(
        s.prompt("normal model turn")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        kar::Outcome::Completed
    );
    s.close().await.unwrap();
    listener.await.unwrap();
    for record in s.records() {
        let live = rx.recv().await.unwrap();
        assert_eq!(
            serde_json::to_value(record.as_ref()).unwrap(),
            serde_json::to_value(live.as_ref()).unwrap()
        );
    }
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
#[ignore = "requires Pi RPC and its published examples/extensions/input-transform.ts fixture"]
async fn explicit_native_refusal_and_handled_input_drain_in_order_after_restart() {
    let f = fixture(true).await;
    let agent = f.temp.path().join("agent");
    enable_input_transform(&agent);
    let bin = std::env::var_os("KAR_PI_BIN").unwrap();
    let store = ChatStore::open(f.temp.path().join("chats")).unwrap();
    let s = store
        .create("pi", Some(Path::new(&bin)), f.options.clone())
        .await
        .unwrap();
    let chat = s.info().id;
    let native = s.info().native_id.clone();
    let work = s.info().workspace.clone();
    s.send(Uuid::new_v4(), "seed the native session")
        .await
        .unwrap();
    s.close().await.unwrap();
    let calls = f.calls.lock().unwrap().len();
    let model_path = agent.join("models.json");
    let configured = fs::read(&model_path).unwrap();
    let mut config: Value = serde_json::from_slice(&configured).unwrap();
    config["providers"]["fixture"]
        .as_object_mut()
        .unwrap()
        .remove("apiKey");
    fs::write(&model_path, config.to_string()).unwrap();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    store
        .enqueue(chat, first, "needs model credentials")
        .unwrap();
    store.enqueue(chat, second, "ping").unwrap();
    let s = store
        .resume_with(chat, |o| o.env.clone_from(&f.options.env))
        .await
        .unwrap();
    assert_eq!(s.info().native_id, native);
    assert_eq!(s.info().workspace, work);
    tokio::time::timeout(std::time::Duration::from_secs(10), s.drain_queue())
        .await
        .unwrap()
        .unwrap();
    s.close().await.unwrap();
    let history = store.history(chat).unwrap();
    assert_eq!(history.turns.len(), 3);
    let refused = &history.turns[1];
    assert_eq!((refused.id, refused.status), (first, TurnStatus::Failed));
    assert_eq!(
        refused.native_input_receipt,
        Some(kar::InputReceipt::Refused)
    );
    assert!(
        refused
            .failure
            .as_ref()
            .unwrap()
            .reason
            .to_ascii_lowercase()
            .contains("api key"),
        "{:?}",
        refused.failure
    );
    let handled = &history.turns[2];
    assert_eq!(
        (handled.id, handled.status),
        (second, TurnStatus::Completed)
    );
    assert_eq!(
        handled.native_input_receipt,
        Some(kar::InputReceipt::Handled)
    );
    assert!(handled.fork_anchor.is_none());
    assert_eq!(f.calls.lock().unwrap().len(), calls);
    fs::write(model_path, configured).unwrap();
    assert_eq!(
        store
            .enqueue(chat, first, "needs model credentials")
            .unwrap()
            .status,
        kar::chat::InputStatus::Refused
    );
    assert_eq!(
        store.enqueue(chat, second, "ping").unwrap().status,
        kar::chat::InputStatus::Sent
    );
    let s = store
        .resume_with(chat, |o| o.env.clone_from(&f.options.env))
        .await
        .unwrap();
    s.drain_queue().await.unwrap();
    assert_eq!(f.calls.lock().unwrap().len(), calls);
    assert_eq!(
        s.send(Uuid::new_v4(), "continue with credentials")
            .await
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    assert!(f.calls.lock().unwrap().len() > calls);
    s.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN and KAR_OAR_PI_PREFLIGHT pointing to the fixed upstream support/pi-preflight.ts"]
async fn admission_holds_while_a_second_input_queues_and_both_are_refused() {
    use sha2::{Digest, Sha256};
    use tokio::time::{Duration, sleep, timeout};

    let mut f = fixture(true).await;
    let agent = f.temp.path().join("agent");
    let entered = agent.join("preflight-entered");
    let release = agent.join("preflight-release");
    let models = agent.join("models.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
    let mut provider = config["providers"]
        .as_object_mut()
        .unwrap()
        .remove("fixture")
        .unwrap();
    provider["models"][0]["id"] = "aimock-model".into();
    config["providers"]["aimock"] = provider;
    fs::write(&models, config.to_string()).unwrap();
    f.options.model = Some("aimock/aimock-model".into());

    // Instantiate the extension from the upstream test, changing only its temporary paths.
    let original = fs::read_to_string(std::env::var_os("KAR_OAR_PI_PREFLIGHT").unwrap()).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(original.as_bytes())),
        "c8361424c55e6210a4993d941be4aea7ba06878ceb5f566b2a38773ec6d3b4ec"
    );
    let mut extension = original
        .split_once("extensions, \"preflight.ts\"), `")
        .unwrap()
        .1
        .split_once("`);")
        .unwrap()
        .0
        .to_owned();
    for (name, path) in [
        ("entered", &entered),
        ("release", &release),
        ("models", &models),
    ] {
        extension = extension.replace(
            &format!("${{JSON.stringify({name})}}"),
            &serde_json::to_string(path).unwrap(),
        );
    }
    let ext = agent.join("extensions");
    fs::create_dir(&ext).unwrap();
    fs::write(ext.join("preflight.ts"), extension).unwrap();

    let mut rt = kar::runtime("pi").unwrap();
    rt.program = std::env::var_os("KAR_PI_BIN").unwrap().into();
    let s = rt.open(f.options.clone()).await.unwrap();
    let first_id = Uuid::new_v4();
    let first = s.prompt_with_id(first_id, "A");
    tokio::pin!(first);
    timeout(Duration::from_secs(10), async {
        tokio::select! {
            _ = &mut first => panic!("prompt returned before native preflight released"),
            () = async { while !entered.is_file() { sleep(Duration::from_millis(5)).await; } } => {}
        }
    })
    .await
    .unwrap();
    let (queued_id, queued) = s.queue("B").await.unwrap();
    fs::write(&release, "").unwrap();
    let limit = Duration::from_secs(15);
    assert!(
        matches!(timeout(limit, first).await.unwrap(), Err(kar::Error::Refused(e)) if e.to_ascii_lowercase().contains("no api key"))
    );
    assert!(
        matches!(timeout(limit, queued.wait()).await.unwrap(), Err(kar::Error::Refused(e)) if e.to_ascii_lowercase().contains("no api key"))
    );
    let rows = s.records();
    let identity = rows
        .iter()
        .filter_map(|r| r.prompt.as_ref())
        .find(|p| p.input_id == first_id.to_string())
        .unwrap();
    let attempt = identity.request_id.as_ref().unwrap();
    assert!(rows.iter().any(|r| {
        r.native["requestId"] == attempt.as_str()
            && r.native["type"] == "rejected"
            && r.native["code"] == "runtime_refused"
            && r.native["reason"]
                .as_str()
                .is_some_and(|reason| reason.to_ascii_lowercase().contains("no api key"))
    }));
    assert!(
        !rows
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e, Event::TurnStarted | Event::TurnEnded { .. }))
    );
    let dropped: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.events
                .iter()
                .any(|event| matches!(event, Event::InputDropped { .. }))
        })
        .collect();
    assert_eq!(
        dropped.len(),
        1,
        "only the already accepted queue entry was dropped"
    );
    assert!(dropped[0].events.iter().any(|event| matches!(event, Event::InputDropped { input_id } if *input_id == queued_id.to_string())));
    assert!(
        dropped[0].native["error"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("no api key")
    );
    assert!(f.calls.lock().unwrap().is_empty());
    assert!(matches!(s.abort().await, Err(kar::Error::Refused(e)) if e.contains("no active turn")));
    let rows = s.records();
    let abort = rows
        .iter()
        .find(|r| r.native["type"] == "abort" && r.kind == kar::RecordKind::Request)
        .unwrap();
    assert!(rows.iter().any(|r| r.seq > abort.seq
        && r.native["requestId"] == abort.native["id"]
        && r.native["type"] == "rejected"
        && r.native["code"] == "no_active_turn"));
    let recovered = timeout(limit, async { s.prompt("C").await?.wait().await })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered, kar::Outcome::Completed);
    assert_eq!(f.calls.lock().unwrap().len(), 1);
    s.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN; raw public prompt identities and projection cursors"]
async fn public_prompt_readbacks() {
    let fixture = fixture(true).await;
    let mut runtime = kar::runtime("pi").unwrap();
    runtime.program = std::env::var_os("KAR_PI_BIN").unwrap().into();
    let session = runtime.open(fixture.options.clone()).await.unwrap();
    super::public_readbacks::check(&session).await.unwrap();
    super::public_readbacks::stream::check(&session)
        .await
        .unwrap();
    let active = runtime.open(fixture.options.clone()).await.unwrap();
    super::public_readbacks::stream::dispose_mid_turn(&active)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires KAR_PI_BIN; hard-kills a Kar CLI during a real Pi model request"]
async fn queued_inputs_and_two_http_clients_survive_controller_kill() {
    use kar::chat::{ChatCursor, InputStatus};

    async fn serve(store: &ChatStore, id: Uuid) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = kar::chat::event_router(store.clone(), id);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (url, server)
    }
    async fn receive(
        sub: &mut SseClient,
        replica: &mut ChatReplica,
        observed: &mut Vec<Value>,
        through: ChatCursor,
    ) {
        while replica.cursor() != through {
            let event = sub.next().await;
            observed.push(serde_json::to_value(&event).unwrap());
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
    }

    let f = fixture(true).await;
    let path = f.temp.path().join("chats");
    let store = ChatStore::open(&path).unwrap();
    let program = std::env::var_os("KAR_PI_BIN").unwrap();
    let session = store
        .create("pi", Some(Path::new(&program)), f.options.clone())
        .await
        .unwrap();
    let id = session.info().id;
    let root = session.info().native_id.clone();
    let mut a = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let start = a.cursor();
    let (url, http) = serve(&store, id).await;
    let mut left = SseClient::open(&url, &start.to_string(), None).await;
    let mut right = SseClient::open(&url, &start.to_string(), None).await;
    let (mut left_seen, mut right_seen) = (vec![], vec![]);
    let first = session
        .send(Uuid::new_v4(), "kar-fork-stage first")
        .await
        .unwrap();
    assert!(
        first
            .items
            .iter()
            .any(|i| matches!(i.item, Item::Tool { .. }))
    );
    assert_eq!(first.status, TurnStatus::Completed);
    let first_tip = store.feed_snapshot(id).unwrap().cursor;
    receive(&mut left, &mut a, &mut left_seen, first_tip).await;
    drop(left);
    let second = session
        .send(Uuid::new_v4(), "kar-queue-seed-two")
        .await
        .unwrap();
    assert_eq!(second.status, TurnStatus::Completed);
    session.close().await.unwrap();

    let held = (Uuid::new_v4(), "kar-queue-held-at-model");
    let queued = [
        (Uuid::new_v4(), "kar-queue-after-one"),
        (Uuid::new_v4(), "kar-queue-after-two"),
    ];
    store.enqueue(id, held.0, held.1).unwrap();
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(f.temp.path())
        .envs(&f.options.env)
        .env("KAR_CHAT_DIR", &path)
        .env("KAR_PI_BIN", &program)
        .args(["chat", "--json", "resume", "--chat", &id.to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if f.options.cwd.join("requested").exists()
                && store.queued_inputs(id).unwrap()[0].status == InputStatus::Sent
            {
                break;
            }
            if let Some(status) = controller.try_wait().unwrap() {
                panic!("queue controller exited before reaching the held model call: {status}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out at native model gate: requested={}, queue={:?}, turns={:?}, model call count={}",
        f.options.cwd.join("requested").exists(), store.queued_inputs(id).unwrap(),
        store.history(id).unwrap().turns.iter().map(|t| (t.id,t.status)).collect::<Vec<_>>(),
        f.calls.lock().unwrap().len()));
    for (uuid, text) in queued {
        assert_eq!(
            store.enqueue(id, uuid, text).unwrap().status,
            InputStatus::Pending
        );
    }
    let held_tip = store.feed_snapshot(id).unwrap().cursor;
    receive(&mut right, &mut b, &mut right_seen, held_tip).await;
    drop(right);
    http.abort();
    controller.kill().await.unwrap();
    assert!(!controller.wait().await.unwrap().success());
    fs::write(f.options.cwd.join("release"), "").unwrap();

    let reopened = ChatStore::open(&path).unwrap();
    let resume = reopened
        .resume_with(id, |opts| opts.env = f.options.env.clone())
        .await
        .unwrap();
    assert_eq!(resume.info().native_id, root);
    assert_eq!(resume.info().workspace, f.options.cwd);
    resume.drain_queue().await.unwrap();
    for (uuid, text) in queued {
        assert_eq!(
            resume.send(uuid, text).await.unwrap().status,
            TurnStatus::Completed
        );
        assert_eq!(
            reopened.enqueue(id, uuid, text).unwrap().status,
            InputStatus::Sent
        );
    }
    assert_eq!(
        reopened.enqueue(id, held.0, held.1).unwrap().status,
        InputStatus::Sent
    );
    resume.drain_queue().await.unwrap();
    resume.close().await.unwrap();
    let history = reopened.history(id).unwrap();
    assert_eq!(
        history.turns.iter().map(|t| t.id).collect::<Vec<_>>(),
        [first.id, second.id, held.0, queued[0].0, queued[1].0]
    );
    let mut runtime = kar::runtime("pi").unwrap();
    runtime.program = program.into();
    let mut options = f.options.clone();
    options.resume = Some(root.clone());
    let native = runtime.open(options).await.unwrap();
    let transcript = native.native_session_file().await.unwrap().unwrap();
    native.close().await.unwrap();
    let mut in_held_turn = false;
    let mut stop = None;
    for line in fs::read_to_string(transcript).unwrap().lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let message = &row["message"];
        if message["role"] == "user" {
            in_held_turn = message["content"].to_string().contains(held.1);
        } else if in_held_turn && message["role"] == "assistant" {
            stop = message["stopReason"].as_str().map(str::to_owned);
        }
    }
    let expected = match stop.as_deref() {
        Some("stop" | "length") => TurnStatus::Completed,
        Some("aborted") => TurnStatus::Aborted,
        Some("error") => TurnStatus::Failed,
        None => TurnStatus::Interrupted,
        other => panic!("unrecognized Pi terminal {other:?}"),
    };
    assert_eq!(
        history.turns[2].status, expected,
        "saved Pi stopReason={stop:?}"
    );
    assert_eq!(history.turns[0], first);
    assert_eq!(history.turns[1], second);
    let tip = reopened.feed_snapshot(id).unwrap().cursor;
    let (url, http) = serve(&reopened, id).await;
    let mut canonical = vec![];
    let mut feed = reopened.follow(id, Some(start)).unwrap();
    while feed.cursor() != tip {
        canonical.push(serde_json::to_value(feed.recv().await.unwrap()).unwrap());
    }
    assert_ne!(a.cursor(), b.cursor());
    for (replica, observed) in [(&mut a, &mut left_seen), (&mut b, &mut right_seen)] {
        let mut sub = SseClient::open(
            &url,
            &start.to_string(),
            Some(&replica.cursor().to_string()),
        )
        .await;
        receive(&mut sub, replica, observed, tip).await;
        assert_eq!(*observed, canonical);
        assert_eq!(replica.turns(), history.turns);
    }
    http.abort();
    let calls = f.calls.lock().unwrap();
    let markers: Vec<_> = calls
        .iter()
        .filter_map(|messages| {
            let msg = messages.as_array()?.last()?;
            if msg["role"] != "user" {
                return None;
            }
            let content = &msg["content"];
            let text = content
                .as_str()
                .or_else(|| content.as_array()?.iter().find_map(|p| p["text"].as_str()))?;
            [held.1, queued[0].1, queued[1].1]
                .into_iter()
                .find(|s| text.trim() == *s)
        })
        .collect();
    assert_eq!(markers, [held.1, queued[0].1, queued[1].1]);
    println!(
        "Pi model received {markers:?} once each; both HTTP clients matched all {} durable events",
        canonical.len()
    );
}

#[tokio::test]
#[ignore = "requires real KAR_PI_BIN to verify queue dispatch and withdrawal"]
async fn public_adapter_queue_and_withdraw() {
    let f = fixture(true).await;
    let mut rt = kar::runtime("pi").unwrap();
    rt.program = std::env::var_os("KAR_PI_BIN").unwrap().into();
    let s = rt.open(f.options.clone()).await.unwrap();
    let release = f.options.cwd.join("release");
    adapter_queue::check(
        &s,
        "kar-queue-held-at-model",
        || {
            if release.exists() {
                fs::remove_file(&release).unwrap();
            }
        },
        || fs::write(&release, "").unwrap(),
    )
    .await;
    assert_eq!(f.calls.lock().unwrap().len(), 3);
}
