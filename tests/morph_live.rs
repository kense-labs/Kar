#[path = "support/disposal.rs"]
mod disposal;
use axum::{
    Json, Router,
    body::Body,
    http::{Response, header::CONTENT_TYPE},
    routing::post,
};
use kar::{Event, Outcome, SessionOptions, community::Morph};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
static MODEL_CALLS: AtomicUsize = AtomicUsize::new(0);

async fn chat(Json(body): Json<Value>) -> Response<Body> {
    reply(body, "printf morph-tool-output")
}

fn reply(body: Value, shell: &str) -> Response<Body> {
    MODEL_CALLS.fetch_add(1, Ordering::Relaxed);
    let text = r#"{"type":"final","output":"morph-rust-loopback"}"#;
    let messages = body["messages"].as_array().unwrap();
    let prompt = messages.iter().rev().find_map(|m| {
        let content: Value = serde_json::from_str(m["content"].as_str()?).ok()?;
        content
            .pointer("/current_message/text")?
            .as_str()
            .map(str::to_owned)
    });
    let use_tool = prompt
        .as_deref()
        .is_some_and(|s| s.contains("morph-shell-fixture"))
        && !messages
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "morph-readout");
    let call = json!({"id":"morph-readout","type":"function","function":{"name":"bash","arguments":json!({"cmd":shell}).to_string()}});
    let finish = if use_tool { "tool_calls" } else { "stop" };
    if body["stream"] == true {
        let mut data = String::new();
        let first = if use_tool {
            let mut call = call.clone();
            call["index"] = 0.into();
            json!({"role":"assistant","tool_calls":[call]})
        } else {
            json!({"role":"assistant","content":text})
        };
        for (delta, finish) in [(first, Value::Null), (json!({}), finish.into())] {
            let frame = json!({"id":"chatcmpl-fixture","model":"fixture","object":"chat.completion.chunk","created":1,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
            data += &format!("data: {frame}\n\n");
        }
        data += "data: [DONE]\n\n";
        return Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(data))
            .unwrap();
    }
    let message = if use_tool {
        json!({"role":"assistant","tool_calls":[call]})
    } else {
        json!({"role":"assistant","content":text})
    };
    let reply = json!({"id":"chatcmpl-fixture","object":"chat.completion","model":"fixture","created":1,
        "choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":{"prompt_tokens":2,"completion_tokens":4,"total_tokens":6}});
    Response::builder()
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(reply.to_string()))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires an installed Morph executable; uses a local model and an isolated Console"]
async fn actual_morph_shared_console_turn_and_resume() {
    let router = Router::new().fallback(post(chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg,format!("llm:\n  inference_provider: openai_chat_compatible\n  endpoint: http://{address}/v1\n  api_key: fixture\n  model: fixture\n")).unwrap();
    let program = std::env::var_os("KAR_MORPH_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| "morph".into());
    let runtime = Morph::local(&program, &state).with_config(&cfg).unwrap();
    let mut options = SessionOptions::new(dir.path());
    options.model = Some("default".into());
    let (one, two) =
        tokio::try_join!(runtime.open(options.clone()), runtime.open(options.clone())).unwrap();
    assert_ne!(one.id(), two.id());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let (first, second) = tokio::join!(one.resources(), two.resources());
        let reading = first.expect("owned Morph process");
        assert_eq!(Some(reading), second);
        assert_eq!(reading.scope, Some(kar::ResourceScope::SharedProcess));
        assert!(reading.rss > 0 && reading.processes >= 1);
    }
    let endpoint = state.join("console/runtime.json");
    let native: Value = serde_json::from_slice(&std::fs::read(&endpoint).unwrap()).unwrap();
    let url = native["url"].as_str().unwrap().to_owned();
    let key = native["token"].as_str().unwrap();
    let external = Morph::attach(&url, key)
        .unwrap()
        .open(options.clone())
        .await
        .unwrap();
    assert!(external.resources().await.is_none());
    disposal::check(&external, "accepted").await.unwrap();
    let check = reqwest::Client::builder().no_proxy().build().unwrap();
    let cli = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["models", "morph", "--bin"])
        .arg(&program)
        .env("MISTER_MORPH_FILE_STATE_DIR", &state)
        .env("MISTER_MORPH_CONFIG", &cfg)
        .output()
        .await
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let catalog: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(catalog[0]["resolvedId"], "fixture");
    assert!(
        one.models()
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|profile| profile["id"] == "default" && profile["resolvedId"] == "fixture")
    );
    disposal::check(&one, "accepted").await.unwrap();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert_eq!(
        one.resources().await.unwrap().scope,
        Some(kar::ResourceScope::SharedProcess)
    );
    let prompt = if cfg!(unix) {
        "morph-shell-fixture: use bash to print morph-tool-output."
    } else {
        "Reply with a brief greeting."
    };
    let turn = two.prompt(prompt).await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(50), turn.report())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Completed,
        "task terminal frame: {:?}",
        two.records().last().map(|r| &r.native)
    );
    assert!(
        report.text.contains("morph-rust-loopback"),
        "{}",
        report.text
    );
    assert!(two.records().iter().any(|r| {
        r.span_id.is_some()
            && r.events
                .iter()
                .any(|e| matches!(e, Event::TextDelta { .. }))
    }));
    assert!(MODEL_CALLS.load(Ordering::Relaxed) > 0);
    if cfg!(unix) {
        let mut view = kar::observe::SessionView::default();
        for record in two.records() {
            view.apply(&record).unwrap();
        }
        let tool = view
            .agents
            .values()
            .flat_map(|agent| agent.tools.values())
            .find(|tool| tool.name.as_deref() == Some("bash"))
            .expect("Morph did not publish its shell call");
        assert_eq!(tool.input, Some(json!("printf morph-tool-output")));
        let result = tool.result.as_ref().unwrap();
        assert_eq!(
            result.status,
            Some(kar::ToolStatus::Succeeded),
            "{result:?}"
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert!(result.text().unwrap().contains("morph-tool-output"));
    }
    options.resume = Some(two.id().into());
    disposal::check(&two, "exited").await.unwrap();
    assert!(one.resources().await.is_none());
    assert!(two.resources().await.is_none());
    assert!(
        check
            .get(format!("{url}/health"))
            .bearer_auth(key)
            .send()
            .await
            .is_err()
    );
    let resumed = runtime.open(options).await.unwrap();
    assert_eq!(resumed.id(), two.id());
    assert_ne!(two.stream_id(), resumed.stream_id());
    resumed.close().await.unwrap();
    http.abort();
}

#[tokio::test]
#[ignore = "requires an installed Morph executable; uses an isolated native task archive"]
async fn native_task_archive_recovers_two_chat_clients() {
    use kar::chat::{ChatReplica, ChatStore, Item, Role, TurnStatus};
    use std::{fs, path::Path, sync::Arc};
    use tokio::process::Command;
    use uuid::Uuid;

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(post(move |body: Json<Value>| {
        counter.fetch_add(1, Ordering::Relaxed);
        chat(body)
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let cwd = dir.path().join("workspace");
    fs::create_dir(&cwd).unwrap();
    fs::write(&cfg,format!("llm:\n  inference_provider: openai_chat_compatible\n  endpoint: http://{address}/v1\n  api_key: fixture\n  model: fixture\n")).unwrap();
    let program = std::env::var_os("KAR_MORPH_BIN").unwrap_or_else(|| "morph".into());
    let cli = |store: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kar"));
        command
            .args(["chat", "--json"])
            .env("KAR_CHAT_DIR", store)
            .env("MISTER_MORPH_CONFIG", &cfg)
            .env("MISTER_MORPH_FILE_STATE_DIR", dir.path().join("state"));
        command
    };
    let original = dir.path().join("chats");
    let request = Uuid::new_v4().to_string();
    let prompt = if cfg!(unix) {
        "morph-shell-fixture: use bash to print morph-tool-output."
    } else {
        "reply briefly"
    };
    let output = cli(&original)
        .args(["new", "morph", "--model", "default", "--bin"])
        .arg(&program)
        .arg("--cwd")
        .arg(&cwd)
        .args(["--request-id", &request, prompt])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = ChatStore::open(&original).unwrap();
    let id = store.latest().unwrap();
    let expected = store.history(id).unwrap().turns.remove(0);
    assert_eq!(expected.status, TurnStatus::Completed);
    let before = calls.load(Ordering::Relaxed);
    assert!(before > 0);
    let filename = format!("{id}.karchat");
    let history = fs::read_to_string(original.join(&filename)).unwrap();
    let mut prefix = String::new();
    let mut cases = Vec::new();
    for line in history.lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let progress = &row["frame"]["morph_progress"];
        let accepted = progress["accepted"] == true;
        if accepted {
            cases.push(("before-receipt", prefix.clone(), false));
        }
        prefix.push_str(line);
        prefix.push('\n');
        if accepted {
            cases.push(("after-receipt", prefix.clone(), true));
        } else if progress["seq"].as_u64().is_some_and(|seq| seq > 0)
            && !cases.iter().any(|(name, ..)| *name == "partial-trace")
        {
            cases.push(("partial-trace", prefix.clone(), true));
        }
    }
    assert!(cases.len() >= 2, "missing the durable native task receipt");
    for (name, data, confirmed) in cases {
        let path = dir.path().join(name);
        let store = ChatStore::open(&path).unwrap();
        fs::write(path.join(&filename), data).unwrap();
        fs::File::create(path.join(format!("{id}.lock"))).unwrap();
        let mut first = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
        let mut second = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
        assert_eq!(first.turns()[0].status, TurnStatus::Interrupted);
        let mut feed = store.follow(id, Some(first.cursor())).unwrap();
        let output = cli(&path)
            .args([
                "resume",
                "--chat",
                &id.to_string(),
                "--request-id",
                &request,
                prompt,
            ])
            .output()
            .await
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if confirmed {
            assert!(output.status.success(), "{name}: {stderr}");
        } else {
            assert!(
                stderr.contains("turn status: Interrupted"),
                "{name}: {stderr}"
            );
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            before,
            "the controller resent a native prompt"
        );
        let end = store.feed_snapshot(id).unwrap();
        let mut offline = store.follow(id, Some(second.cursor())).unwrap();
        for (replica, feed) in [(&mut first, &mut feed), (&mut second, &mut offline)] {
            while replica.cursor() != end.cursor {
                let event = tokio::time::timeout(Duration::from_secs(3), feed.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(replica.apply(event.clone()).unwrap());
                assert!(!replica.apply(event).unwrap());
            }
            assert_eq!(replica.turns().len(), 1);
            let turn = &replica.turns()[0];
            if confirmed {
                assert_eq!(turn.status, TurnStatus::Completed, "{name}: {turn:?}");
                assert_eq!(turn.native_turn_id, expected.native_turn_id);
                assert!(turn.input_sent);
                let items = serde_json::to_value(&turn.items).unwrap();
                assert_eq!(
                    items,
                    serde_json::to_value(&expected.items).unwrap(),
                    "{name}"
                );
                assert!(turn.items.iter().any(|i|matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="morph-rust-loopback")));
            } else {
                assert_eq!(turn.status, TurnStatus::Interrupted);
                assert!(turn.native_turn_id.is_none());
            }
        }
    }
    http.abort();
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires Morph and bash; kills an isolated controller while its native tool is executing"]
async fn reattach_running_task_then_deliver_offline_queue() {
    use kar::chat::{
        ChatReplica, ChatStore, ChatUpdate, HttpChatFeed, Item, Role, TurnStatus, event_router,
    };
    use std::{fs, process::Stdio, sync::Arc};
    use tokio::process::Command;
    use uuid::Uuid;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    fs::create_dir(&cwd).unwrap();
    let state = dir.path().join("state");
    let cfg = dir.path().join("config.yaml");
    let chats = dir.path().join("chats");
    let store = ChatStore::open(&chats).unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let counter = calls.clone();
    const WAIT_FOR_RELEASE: &str = concat!(
        "for i in {1..400}; do if test -f release; then ",
        "printf resumed > edited.txt; printf morph-tool-output; exit 0; ",
        "fi; sleep 0.1; done; exit 1"
    );
    let model = Router::new().fallback(post(move |Json(body): Json<Value>| {
        // Topic naming also uses inference. Its calls have no execution task ID.
        let task = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|message| {
                let content: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
                content
                    .pointer("/mister_morph_meta/console_task_id")?
                    .as_str()
                    .map(str::to_owned)
            });
        if let Some(task) = task {
            counter.lock().unwrap().push(task);
        }
        async move { reply(body, WAIT_FOR_RELEASE) }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let model = tokio::spawn(async move { axum::serve(listener, model).await.unwrap() });
    fs::write(&cfg,format!("llm:\n  inference_provider: openai_chat_compatible\n  endpoint: http://{address}/v1\n  api_key: fixture\n  model: fixture\n")).unwrap();
    let program = std::env::var_os("KAR_MORPH_BIN").unwrap_or_else(|| "morph".into());
    let owner = Morph::local(&program, &state)
        .with_config(&cfg)
        .unwrap()
        .open(SessionOptions::new(&cwd))
        .await
        .unwrap();
    let cli = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
        cmd.args(["chat", "--json"])
            .env("KAR_CHAT_DIR", &chats)
            .env("MISTER_MORPH_CONFIG", &cfg)
            .env("MISTER_MORPH_FILE_STATE_DIR", &state)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    };
    let request = Uuid::new_v4();
    let prompt = "morph-shell-fixture: wait for release before finishing.";
    let mut controller = cli()
        .args(["new", "morph", "--bin"])
        .arg(&program)
        .arg("--cwd")
        .arg(&cwd)
        .args(["--request-id", &request.to_string(), prompt])
        .spawn()
        .unwrap();
    let id = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(exit) = controller.try_wait().unwrap() {
                panic!("initial controller exited: {exit}");
            }
            if let Ok(id) = store.latest()
                && store.history(id).unwrap().turns.iter().any(|t| {
                    t.id == request
                        && t.items.iter().any(
                            |i| matches!(&i.item,Item::Tool{activity,..} if !activity.finished),
                        )
                })
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("native shell did not start");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = event_router(store.clone(), id);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut live = HttpChatFeed::new(&url).unwrap();
    let snapshot = live.snapshot().await.unwrap();
    let task = snapshot.history.turns[0].native_turn_id.clone().unwrap();
    let mut online = ChatReplica::from_snapshot(snapshot);
    let mut offline = ChatReplica::from_snapshot(live.snapshot().await.unwrap());
    controller.kill().await.unwrap();
    assert_eq!(
        store.feed_snapshot(id).unwrap().history.turns[0].status,
        TurnStatus::Interrupted
    );
    let closed = cli()
        .args(["resume", "--chat", &id.to_string()])
        .output()
        .await
        .unwrap();
    assert!(
        closed.status.success(),
        "{}",
        String::from_utf8_lossy(&closed.stderr)
    );
    assert_eq!(
        store.feed_snapshot(id).unwrap().history.turns[0].status,
        TurnStatus::Interrupted
    );
    let queued = [Uuid::new_v4(), Uuid::new_v4()];
    for (n, q) in queued.into_iter().enumerate() {
        store
            .enqueue(id, q, format!("queued follow-up {n}"))
            .unwrap();
    }
    let resumed = cli()
        .args(["resume", "--chat", &id.to_string()])
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(12), async {
        let mut attachments = 0;
        loop {
            let event = live.recv(online.cursor()).await.unwrap();
            let reattached =
                matches!(&event.update,ChatUpdate::MorphReattached{task:t,..} if t==&task);
            online.apply(event).unwrap();
            if reattached {
                assert_eq!(online.turns().len(), 1);
                assert_eq!(online.turns()[0].status, TurnStatus::Running);
                assert!(
                    online.turns()[0]
                        .items
                        .iter()
                        .any(|i| matches!(&i.item,Item::Tool{activity,..} if !activity.finished))
                );
                attachments += 1;
                if attachments == 2 {
                    break;
                }
            }
        }
    })
    .await
    .expect("two controller attachments");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        std::slice::from_ref(&task),
        "a queued prompt ran before the active native task ended"
    );
    fs::write(cwd.join("release"), "go").unwrap();
    let output = tokio::time::timeout(Duration::from_secs(40), resumed.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let final_state = store.feed_snapshot(id).unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        [
            task.clone(),
            task.clone(),
            final_state.history.turns[1].native_turn_id.clone().unwrap(),
            final_state.history.turns[2].native_turn_id.clone().unwrap()
        ],
        "expected shell request, shell result, and two queued prompts"
    );
    let mut reconnected = HttpChatFeed::new(&url).unwrap();
    for (replica, feed) in [(&mut online, &mut live), (&mut offline, &mut reconnected)] {
        while replica.cursor() != final_state.cursor {
            let event = tokio::time::timeout(Duration::from_secs(3), feed.recv(replica.cursor()))
                .await
                .unwrap()
                .unwrap();
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        let turns = replica.turns();
        assert_eq!(
            turns.iter().map(|t| t.id).collect::<Vec<_>>(),
            [request, queued[0], queued[1]]
        );
        assert!(
            turns
                .iter()
                .all(|t| t.status == TurnStatus::Completed && t.input_sent)
        );
        let first = &turns[0];
        assert_eq!(first.native_turn_id.as_deref(), Some(task.as_str()));
        let tools = first
            .items
            .iter()
            .filter_map(|i| match &i.item {
                Item::Tool { activity, .. } => Some(activity),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1);
        let result = tools[0].result.as_ref().unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.text().unwrap().contains("morph-tool-output"));
        assert_eq!(first.items.iter().filter(|i|matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="morph-rust-loopback")).count(),1);
        assert!(
            first
                .checkpoint
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|f| f.path == std::path::Path::new("edited.txt"))
        );
        assert_eq!(
            serde_json::to_value(turns).unwrap(),
            serde_json::to_value(&final_state.history.turns).unwrap()
        );
    }
    owner.close().await.unwrap();
    server.abort();
    model.abort();
}
