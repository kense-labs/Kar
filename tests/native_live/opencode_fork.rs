use super::*;
use kar::chat::{ChatStore, TurnStatus};
use std::sync::{Arc, Mutex};

#[tokio::test]
#[ignore = "requires OpenCode with native ACP session/fork; verifies the next model requests on both branches"]
async fn native_whole_session_fork_keeps_independent_model_contexts() {
    let dir = tempfile::tempdir().unwrap();
    let (opts, calls) = fixture(&dir.path().canonicalize().unwrap());
    let rt = kar::runtime("opencode").unwrap();
    let installed = rt
        .probe_installation(&opts.installation_options())
        .await
        .unwrap();
    let declared = rt
        .session_capabilities(&installed)
        .expect("available OpenCode");
    assert!(!declared.refused_session_options.contains_key("fork"));
    let source = rt.open(opts.clone()).await.unwrap();
    assert!(source.capabilities().fork);
    assert_eq!(
        declared.manual_compaction.unwrap().is_supported(),
        source.capabilities().manual_compaction
    );
    assert_eq!(
        source
            .prompt("original-before-branch")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    let child = rt.fork(source.id(), None, opts).await.unwrap();
    assert_ne!(child.id(), source.id());
    assert_eq!(
        child
            .prompt("branch-only-after-copy")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    assert_eq!(
        source
            .prompt("original-only-after-copy")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    child.close().await.unwrap();
    source.close().await.unwrap();
    let recorded = calls.lock().unwrap();
    let history = |marker: &str| {
        recorded
            .iter()
            .rev()
            .find_map(|req| {
                let messages = req.as_array()?;
                let user = messages.iter().rev().find(|m| m["role"] == "user")?;
                user["content"]
                    .to_string()
                    .contains(marker)
                    .then(|| req.to_string())
            })
            .expect("native model request")
    };
    let branch = history("branch-only-after-copy");
    let original = history("original-only-after-copy");
    assert!(branch.contains("original-before-branch"));
    assert!(original.contains("original-before-branch"));
    assert!(!original.contains("branch-only-after-copy"));
}

#[tokio::test]
#[ignore = "requires OpenCode 2; uses its native session database and a loopback model"]
async fn native_turn_fork_rolls_back_workspace_and_context() {
    let program = std::env::var_os("OAR_OPENCODE_BIN").expect("set OAR_OPENCODE_BIN");
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let (opts, calls) = fixture(&dir);
    let work = dir.join("work");
    let store = ChatStore::open(dir.join("chats")).unwrap();
    let cli = async |args: &[&str]| {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"));
        cmd.current_dir(&dir)
            .envs(&opts.env)
            .env("OAR_OPENCODE_BIN", &program)
            .env("OAR_CHAT_DIR", dir.join("chats"))
            .args(["chat", "--json"])
            .args(args)
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(45), cmd.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    cli(&[
        "new",
        "opencode",
        "orange-original",
        "--cwd",
        "work",
        "--model",
        "fixture/fixture",
        "--add-dir",
        "../shared",
    ])
    .await;
    let parent = store.latest().unwrap();
    let parent_id = parent.to_string();
    cli(&["resume", "orange-original", "--chat", &parent_id]).await;
    cli(&["resume", "blue-continuation", "--chat", &parent_id]).await;
    let source = store.history(parent).unwrap();
    assert_eq!(source.turns.len(), 3);
    assert_eq!(
        std::fs::read_to_string(work.join("version")).unwrap(),
        "blue"
    );
    let selected = source.turns[1].id.to_string();
    let fork = cli(&["fork", &selected, "--chat", &parent_id]).await;
    let child = store.latest().unwrap();
    let child_id = child.to_string();
    assert_ne!(child, parent);
    assert_ne!(fork["chat"]["native_id"], source.chat.native_id);
    assert_eq!(fork["turns"].as_array().unwrap().len(), 2);
    assert_eq!(fork["turns"][1]["id"], selected);
    assert_eq!(
        std::fs::read_to_string(work.join("version")).unwrap(),
        "orange"
    );
    cli(&[
        "resume",
        "green-branch OAR-FORK-READ-SHARED",
        "--chat",
        &child_id,
    ])
    .await;
    assert_eq!(
        std::fs::read_to_string(work.join("version")).unwrap(),
        "green"
    );
    {
        let calls = calls.lock().unwrap();
        let request = calls
            .iter()
            .find(|messages| {
                messages.as_array().is_some_and(|a| {
                    a.last()
                        .is_some_and(|last| last.to_string().contains("green-branch"))
                })
            })
            .expect("child model request");
        let history = request.to_string();
        assert_eq!(history.matches("orange-original").count(), 2, "{history}");
        assert!(!history.contains("blue-continuation") && !history.contains("blue-answer-only"));
        assert!(history.contains("rust-wire-ok"));
        assert!(
            calls
                .iter()
                .any(|messages| messages.to_string().contains("approved-root-793")),
            "OpenCode did not return the file from the additional root to the model"
        );
    }
    assert_eq!(source.turns, store.history(parent).unwrap().turns);
    let history = store.history(child).unwrap();
    assert_eq!(history.turns[2].status, TurnStatus::Completed);
    // Latest turn has no next message. The native receipt must name the inclusive idle message.
    let leaf = cli(&[
        "fork",
        &history.turns[2].id.to_string(),
        "--chat",
        &child_id,
    ])
    .await;
    assert_eq!(leaf["turns"].as_array().unwrap().len(), 3);
    let nested = cli(&["fork", &selected, "--chat", &child_id]).await;
    assert_eq!(nested["chat"]["forked_from"]["chat"], child_id);
    assert_eq!(nested["turns"].as_array().unwrap().len(), 2);
    assert_eq!(
        std::fs::read_to_string(work.join("version")).unwrap(),
        "orange"
    );
    // A new context entry can exist before OpenCode writes any message about it.
    let route = format!(
        "/api/experimental/session/{}/instructions/entries/later",
        source.chat.native_id
    );
    let update = tokio::process::Command::new(&program)
        .current_dir(&work)
        .envs(&opts.env)
        .args([
            "api",
            "--standalone",
            "PUT",
            &route,
            "--data",
            r#"{"value":"after the selected turn"}"#,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let current = store.latest().unwrap();
    let before = std::fs::read(work.join("version")).unwrap();
    let blue_turn = &source.turns[2].id.to_string();
    let denied = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(&dir)
        .envs(&opts.env)
        .env("OAR_OPENCODE_BIN", &program)
        .env("OAR_CHAT_DIR", dir.join("chats"))
        .args(["chat", "--json", "fork", blue_turn, "--chat", &parent_id])
        .output()
        .await
        .unwrap();
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("API-managed instructions"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert_eq!(std::fs::read(work.join("version")).unwrap(), before);
    assert_eq!(store.latest().unwrap(), current);
}

fn fixture(dir: &std::path::Path) -> (SessionOptions, Arc<Mutex<Vec<Value>>>) {
    let work = dir.join("work");
    std::fs::create_dir(&work).unwrap();
    let shared = dir.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("grant.txt"), "approved-root-793").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let folder = work.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let log = log.clone();
            let folder = folder.clone();
            std::thread::spawn(move || {
                let _ = respond_in_workspace(conn, None, Some(&log), Some(&folder), false);
            });
        }
    });
    let mut opts = SessionOptions::new(&work);
    for key in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        opts.env.insert(key.into(), dir.join(key).into_os_string());
    }
    opencode_model(&mut opts, &endpoint);
    (opts, calls)
}

#[tokio::test]
#[ignore = "requires OpenCode 2, checks its real transcript, native read tool and two SSE clients"]
async fn recover_finished_turn_from_native_archive() {
    use super::sse::SseClient;
    use kar::{
        Event,
        chat::{ChatEdit, ChatReplica, ChatUpdate, Item, Role},
    };
    use uuid::Uuid;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let (mut opts, calls) = fixture(&dir);
    opts.model = Some("fixture/fixture".into());
    opts.settings
        .additional_directories
        .push(dir.join("shared"));
    let program = std::env::var_os("OAR_OPENCODE_BIN").unwrap();
    let original = ChatStore::open(dir.join("original")).unwrap();
    let chat = original
        .create(
            "opencode",
            Some(std::path::Path::new(&program)),
            opts.clone(),
        )
        .await
        .unwrap();
    let id = chat.info().id;
    let root = chat.info().native_id.clone();
    let request = Uuid::new_v4();
    let prompt = "repeat OAR-FORK-READ-SHARED";
    let initial = original.feed_snapshot(id).unwrap();
    let mut updates = original.follow(id, Some(initial.cursor)).unwrap();
    let expected = chat.send(request, prompt).await.unwrap();
    assert_eq!(expected.status, TurnStatus::Completed);
    assert!(expected.items.iter().any(|i| matches!(&i.item, Item::Tool {activity,..} if activity.result.as_ref().and_then(|r| r.text()).is_some_and(|s|s.contains("approved-root-793")))));
    let end = original.feed_snapshot(id).unwrap().cursor;
    let mut cuts = Vec::new();
    while updates.cursor() != end {
        let e = updates.recv().await.unwrap();
        let label = match &e.update {
            ChatUpdate::InputSent { .. } => Some("before-id"),
            ChatUpdate::Frame { frame, .. }
                if frame.edits.iter().any(|edit| {
                    matches!(
                        edit,
                        ChatEdit::Activity {
                            event: Event::ToolStarted { .. }
                        }
                    )
                }) =>
            {
                Some("tool-start")
            }
            ChatUpdate::Frame { frame, .. }
                if frame.edits.iter().any(|edit| {
                    matches!(
                        edit,
                        ChatEdit::Activity {
                            event: Event::ToolResult { .. }
                        }
                    )
                }) =>
            {
                Some("tool-end")
            }
            ChatUpdate::Frame { frame, .. }
                if frame.edits.iter().any(|edit| {
                    matches!(
                        edit,
                        ChatEdit::Text {
                            role: Role::Assistant,
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
            && !cuts.iter().any(|(tag, _)| *tag == label)
        {
            cuts.push((label, e.cursor));
        }
    }
    assert_eq!(cuts.len(), 4, "{cuts:?}");
    let second = chat.send(Uuid::new_v4(), prompt).await.unwrap();
    assert_eq!(second.status, TurnStatus::Completed);
    chat.close().await.unwrap();
    for (label, cut) in cuts {
        let count = calls.lock().unwrap().len();
        let store = super::interrupted_copy(&dir.join("original"), &dir.join(label), id, cut);
        let queued = label == "partial-reply";
        let pending = Uuid::new_v4();
        let pending_text = "offline-after-repair";
        if queued {
            store.enqueue(id, pending, pending_text).unwrap();
        }
        let listen = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listen.local_addr().unwrap());
        let router = kar::chat::event_router(store.clone(), id);
        let http = tokio::spawn(async move { axum::serve(listen, router).await.unwrap() });
        let mut a = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
        let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
        let mut live = SseClient::open(&url, &a.cursor().to_string(), None).await;
        let resumed = store
            .resume_with(id, |o| o.env = opts.env.clone())
            .await
            .unwrap();
        assert_eq!(resumed.info().native_id, root);
        let result = resumed.send(request, prompt).await.unwrap();
        if label == "before-id" {
            assert_eq!(result.status, TurnStatus::Interrupted);
        } else {
            assert_eq!(
                result.status,
                TurnStatus::Completed,
                "{label}: {:?}",
                result.items
            );
            assert_eq!(result.items, expected.items, "{label}");
            assert!(
                result.input_sent
                    && result.native_turn_id.is_some()
                    && result.fork_anchor.is_some()
            );
            assert!(result.checkpoint.as_ref().unwrap().after_tree.is_none());
        }
        resumed.drain_queue().await.unwrap();
        if queued {
            let accepted = resumed.send(pending, pending_text).await.unwrap();
            assert_eq!(accepted.status, TurnStatus::Completed);
        }
        resumed.close().await.unwrap();
        let again = store
            .resume_with(id, |o| o.env = opts.env.clone())
            .await
            .unwrap();
        again.close().await.unwrap();
        {
            let entries = calls.lock().unwrap();
            // OpenCode may ask its model to title the existing conversation when
            // opening a new CLI process. Count the actual conversation inputs.
            let inputs: Vec<_> = entries[count..]
                .iter()
                .map(|messages| {
                    messages.as_array().unwrap().last().unwrap()["content"]
                        .as_array()
                        .unwrap()
                        .last()
                        .unwrap()["text"]
                        .as_str()
                        .unwrap()
                })
                .filter(|text| !text.ends_with("Reply with only the title."))
                .collect();
            let expected: Vec<_> = queued.then_some(pending_text).into_iter().collect();
            assert_eq!(inputs, expected, "{label}");
        }
        let final_turns = store.history(id).unwrap().turns;
        assert_eq!(final_turns.len(), 1 + usize::from(queued));
        assert_eq!(final_turns[0], result);
        if queued {
            assert_eq!(final_turns[1].id, pending);
            assert_eq!(
                store.queued_inputs(id).unwrap()[0].status,
                kar::chat::InputStatus::Sent
            );
        }
        let mut replay = SseClient::open(
            &url,
            &initial.cursor.to_string(),
            Some(&b.cursor().to_string()),
        )
        .await;
        let tip = store.feed_snapshot(id).unwrap().cursor;
        for (replica, client) in [(&mut a, &mut live), (&mut b, &mut replay)] {
            let mut repairs = 0;
            while replica.cursor() != tip {
                let event = client.next().await;
                repairs += usize::from(matches!(event.update, ChatUpdate::Reconciled { .. }));
                assert!(replica.apply(event.clone()).unwrap());
                assert!(!replica.apply(event).unwrap());
            }
            assert_eq!(repairs, usize::from(label != "before-id"));
            assert_eq!(replica.turns(), final_turns);
        }
        http.abort();
    }
}

#[tokio::test]
#[ignore = "requires OpenCode 2; real failed model step after a completed native read"]
async fn recover_failed_turn_after_tool() {
    use kar::chat::{ChatEdit, ChatUpdate};
    use uuid::Uuid;
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (mut opts, _) = fixture(&base);
    std::fs::write(
        base.join("shared/grant.txt"),
        "oar-fixture-decline-credential",
    )
    .unwrap();
    opts.model = Some("fixture/fixture".into());
    opts.settings
        .additional_directories
        .push(base.join("shared"));
    let store = ChatStore::open(base.join("original")).unwrap();
    let program = std::env::var_os("OAR_OPENCODE_BIN").unwrap();
    let chat = store
        .create(
            "opencode",
            Some(std::path::Path::new(&program)),
            opts.clone(),
        )
        .await
        .unwrap();
    let id = chat.info().id;
    let request = Uuid::new_v4();
    let prompt = "OAR-FORK-READ-SHARED";
    let mut events = store
        .follow(id, Some(store.feed_snapshot(id).unwrap().cursor))
        .unwrap();
    assert!(chat.send(request, prompt).await.is_err());
    let failed = store.history(id).unwrap().turns.pop().unwrap();
    assert_eq!(failed.status, TurnStatus::Failed);
    let end = store.feed_snapshot(id).unwrap().cursor;
    let mut cut = None;
    while events.cursor() != end {
        let event = events.recv().await.unwrap();
        if matches!(&event.update, ChatUpdate::Frame {frame,..} if frame.edits.iter().any(|e| matches!(e,ChatEdit::Activity{event:Event::ToolResult{..}})))
        {
            cut = Some(event.cursor);
        }
    }
    chat.close().await.unwrap();
    let recovered = super::interrupted_copy(
        &base.join("original"),
        &base.join("failed-restart"),
        id,
        cut.unwrap(),
    );
    let session = recovered
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    let repaired = session.send(request, prompt).await.unwrap();
    assert_eq!(repaired.status, TurnStatus::Failed);
    let failure = repaired.failure.unwrap();
    assert_eq!(failure.class, kar::FailureClass::Auth, "{failure:?}");
    assert_eq!(repaired.items, failed.items);
    session.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires OpenCode 2 and Unix signals; kills only the test controller after a real native commit"]
async fn kill_controller_after_native_end_and_deliver_queue() {
    use kar::chat::{ChatEdit, ChatUpdate, Item};
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use uuid::Uuid;

    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (mut opts, calls) = fixture(&base);
    opts.model = Some("fixture/fixture".into());
    opts.settings
        .additional_directories
        .push(base.join("shared"));
    std::fs::write(base.join("shared/grant.txt"), "OAR-CRASH-AFTER-READ").unwrap();
    let dir = base.join("chats");
    let store = ChatStore::open(&dir).unwrap();
    let program = std::env::var_os("OAR_OPENCODE_BIN").unwrap();
    let session = store
        .create(
            "opencode",
            Some(std::path::Path::new(&program)),
            opts.clone(),
        )
        .await
        .unwrap();
    let id = session.info().id;
    let root = session.info().native_id.clone();
    session.close().await.unwrap();
    let request = Uuid::new_v4();
    let queued = Uuid::new_v4();
    let prompt = "read it OAR-FORK-READ-SHARED";
    let next = "queued after hard kill";
    let mut events = store
        .follow(id, Some(store.feed_snapshot(id).unwrap().cursor))
        .unwrap();
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(&base)
        .envs(&opts.env)
        .env("OAR_CHAT_DIR", &dir)
        .env("OAR_OPENCODE_BIN", &program)
        .args([
            "chat",
            "--json",
            "resume",
            prompt,
            "--chat",
            &id.to_string(),
            "--request-id",
            &request.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15),async{
        loop {
            let e=events.recv().await.unwrap();
            if matches!(e.update,ChatUpdate::Frame{frame,..} if frame.edits.iter().any(|edit| matches!(edit,ChatEdit::Activity{event:Event::ToolResult{..}}))) {break;}
        }
        while !base.join("model-waiting").is_file(){tokio::time::sleep(Duration::from_millis(10)).await;}
    }).await.expect("native read must finish before releasing the model");
    store.enqueue(id, queued, next).unwrap();
    kill(
        Pid::from_raw(controller.id().unwrap() as i32),
        Signal::SIGSTOP,
    )
    .unwrap();
    std::fs::write(base.join("model-finish"), "").unwrap();
    let route = format!("/api/session/{root}/message?order=desc&limit=1");
    let idle = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let read = tokio::process::Command::new(&program)
                .current_dir(&opts.cwd)
                .envs(&opts.env)
                .args(["api", "--standalone", "GET", &route])
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                read.status.success(),
                "{}",
                String::from_utf8_lossy(&read.stderr)
            );
            let page: Value = serde_json::from_slice(&read.stdout).unwrap();
            let msg = &page["data"][0];
            if msg["type"] == "idle" && msg["outcome"] == "succeeded" {
                break msg["id"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("OpenCode must persist its native terminal while only the host is stopped");
    controller.kill().await.unwrap();
    assert!(!controller.wait().await.unwrap().success());
    let broken = store.history(id).unwrap();
    assert_ne!(broken.turns[0].status, TurnStatus::Completed);
    let resumed = store
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, root);
    resumed.drain_queue().await.unwrap();
    let original = resumed.send(request, prompt).await.unwrap();
    assert_eq!(original.status, TurnStatus::Completed);
    assert_eq!(original.fork_anchor.as_deref(), Some(idle.as_str()));
    assert_eq!(
        original
            .items
            .iter()
            .filter(|i| matches!(&i.item, Item::Tool { .. }))
            .count(),
        1
    );
    assert!(
        original
            .items
            .iter()
            .any(|i| matches!(&i.item,Item::Message{text,..} if text=="rust-wire-ok"))
    );
    assert_eq!(
        resumed.send(queued, next).await.unwrap().status,
        TurnStatus::Completed
    );
    resumed.close().await.unwrap();
    let history = store.history(id).unwrap();
    assert_eq!(
        history.turns.iter().map(|t| t.id).collect::<Vec<_>>(),
        [request, queued]
    );
    let calls = calls.lock().unwrap();
    for input in [prompt, next] {
        assert_eq!(
            calls
                .iter()
                .filter(
                    |messages| messages.as_array().and_then(|a| a.last()).is_some_and(|m| {
                        m["content"]
                            .as_array()
                            .is_some_and(|ps| ps.iter().any(|part| part["text"] == input))
                    })
                )
                .count(),
            1
        );
    }
}
