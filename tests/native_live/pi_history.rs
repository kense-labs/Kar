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
    for msg in messages.iter().filter(|msg| msg["role"] == "user") {
        let mut visit = |text: &str| {
            if let Some(stage) = text.strip_prefix("kar-fork-stage ")
                && ["first", "middle", "late", "child", "inspect"].contains(&stage)
            {
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

#[tokio::test]
#[ignore = "requires KAR_PI_BIN; raw public prompt identities and projection cursors"]
async fn public_prompt_readbacks() {
    let fixture = fixture(true).await;
    let mut runtime = kar::runtime("pi").unwrap();
    runtime.program = std::env::var_os("KAR_PI_BIN").unwrap().into();
    let session = runtime.open(fixture.options.clone()).await.unwrap();
    super::public_readbacks::check(&session).await.unwrap();
    session.close().await.unwrap();
}
