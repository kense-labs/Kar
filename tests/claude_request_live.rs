use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use kar::{
    Error, ModelRequests, Outcome, SessionOptions,
    chat::{ChatReplica, ChatStore},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

type Requests = Arc<Mutex<Vec<Value>>>;
async fn model(
    State((calls, target)): State<(Requests, PathBuf)>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let id = format!("msg_{}", Uuid::new_v4().simple());
    let model = body["model"].clone();
    let last = body["messages"]
        .as_array()
        .and_then(|v| v.last())
        .map(|v| &v["content"]);
    let write = last.is_some_and(|v| {
        v.to_string().contains("WAIT-FOR-WRITE") && !v.to_string().contains("tool_result")
    });
    let (block, delta, stop) = if write {
        (
            json!({"type":"tool_use","id":"toolu_request_capture","name":"Write","input":{}}),
            json!({"type":"input_json_delta","partial_json":json!({"file_path":target,"content":"inspected"}).to_string()}),
            "tool_use",
        )
    } else {
        (
            json!({"type":"text","text":""}),
            json!({"type":"text_delta","text":"fixture answer"}),
            "end_turn",
        )
    };
    calls.lock().unwrap().push(body);
    let frames = [
        json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_delta","index":0,"delta":delta}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ];
    let sse = frames
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect::<String>();
    (
        [
            ("content-type", "text/event-stream".to_owned()),
            ("request-id", format!("req_{}", Uuid::new_v4().simple())),
        ],
        sse,
    )
}

#[tokio::test]
#[ignore = "requires Claude Code >=2.1.274; runs real request capture with a loopback model"]
async fn native_root_request_matches_the_model_and_expires_on_close() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("capture-write");
    let calls = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/messages", post(model))
        .with_state((calls.clone(), target.clone()));
    let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let bin = std::env::var_os("KAR_CLAUDE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| "claude".into());
    let secret = "request-redaction-fixture-321";
    let mut opts = SessionOptions::new(&work);
    for (name, value) in [
        ("HOME", dir.path().to_str().unwrap()),
        ("CLAUDE_CONFIG_DIR", dir.path().to_str().unwrap()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("ANTHROPIC_BASE_URL", &url),
        ("ANTHROPIC_API_KEY", "only-a-fixture-key"),
        ("ANTHROPIC_AUTH_TOKEN", ""),
        ("KAR_FIXTURE_TOKEN", secret),
    ] {
        opts.env.insert(name.into(), value.into());
    }
    opts.model = Some("sonnet".into());
    opts.settings.system_prompt = Some("system-capture-fixture".into());
    let rt = kar::Runtime::new("claude", &bin, &[], kar::Protocol::Claude);
    let plain = rt.open(opts.clone()).await.unwrap();
    assert!(!plain.capabilities().model_requests);
    assert!(matches!(
        plain.last_model_request().await,
        Err(Error::Unsupported(_))
    ));
    plain.close().await.unwrap();

    opts.settings.model_requests = Some(ModelRequests::Latest);
    let store = ChatStore::open(dir.path().join("store")).unwrap();
    #[cfg(unix)]
    let selected_launcher = {
        let actual = rt.probe_installation(&Default::default()).await.unwrap();
        let folder = dir.path().join("first-cli");
        std::fs::create_dir(&folder).unwrap();
        let launcher = folder.join("claude");
        std::os::unix::fs::symlink(actual.command().unwrap(), &launcher).unwrap();
        let mut dirs = vec![folder];
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        opts.env
            .insert("PATH".into(), std::env::join_paths(dirs).unwrap());
        opts.env.insert("KAR_CLAUDE_BIN".into(), "".into());
        launcher
    };
    #[cfg(unix)]
    let chat = store.create("claude", None, opts.clone()).await.unwrap();
    #[cfg(not(unix))]
    let chat = store
        .create("claude", Some(&bin), opts.clone())
        .await
        .unwrap();
    #[cfg(unix)]
    assert_eq!(chat.info().program, selected_launcher);
    let id = chat.info().id;
    let original = chat.info().native_id.clone();
    assert!(chat.last_model_request().await.unwrap().is_none());
    for text in [
        format!("first root request {secret}"),
        "second root request".to_owned(),
    ] {
        chat.send(Uuid::new_v4(), &text).await.unwrap();
        let cursor = store.feed_snapshot(id).unwrap().cursor;
        let (one, two) = tokio::join!(chat.last_model_request(), chat.last_model_request());
        let req = one.unwrap().unwrap();
        assert_eq!(two.unwrap(), Some(req.clone()));
        let body = req.body.unwrap();
        assert_eq!(req.format, "anthropic.messages");
        assert_eq!(req.provider.as_deref(), Some("firstParty"));
        assert!(req.at.ends_with('Z') && req.withheld.is_empty());
        let wire = calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|v| v["messages"].to_string().contains(&text))
            .unwrap()
            .clone();
        assert_eq!(req.model.as_deref(), wire["model"].as_str());
        for key in ["model", "system", "tools", "messages", "max_tokens"] {
            let expected: Value =
                serde_json::from_str(&wire[key].to_string().replace(secret, "[redacted]")).unwrap();
            assert_eq!(body[key], expected, "captured field {key}");
        }
        assert!(!body.to_string().contains(secret));
        assert_eq!(cursor, store.feed_snapshot(id).unwrap().cursor);
    }
    assert!(
        !serde_json::to_string(&store.history(id).unwrap().turns)
            .unwrap()
            .contains("system-capture-fixture")
    );
    chat.close().await.unwrap();
    let reopened = store
        .resume_with(id, |o| {
            o.env = opts.env.clone();
            #[cfg(unix)]
            {
                o.env.insert("PATH".into(), "/usr/bin:/bin".into());
                o.env.insert(
                    "KAR_CLAUDE_BIN".into(),
                    dir.path().join("missing-cli").into_os_string(),
                );
            }
        })
        .await
        .unwrap();
    assert_eq!(reopened.info().native_id, original);
    assert!(reopened.last_model_request().await.unwrap().is_none());
    let mut replica = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut feed = store.follow(id, Some(replica.cursor())).unwrap();
    reopened
        .send(Uuid::new_v4(), "root after restart")
        .await
        .unwrap();
    let end = store.feed_snapshot(id).unwrap().cursor;
    while replica.cursor() != end {
        replica
            .apply(
                tokio::time::timeout(Duration::from_secs(3), feed.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
    }
    assert!(
        reopened
            .last_model_request()
            .await
            .unwrap()
            .unwrap()
            .body
            .unwrap()["messages"]
            .to_string()
            .contains("root after restart")
    );
    reopened.close().await.unwrap();

    opts.approvals = kar::ApprovalMode::Host;
    let raw = rt.open(opts).await.unwrap();
    assert!(raw.capabilities().model_requests);
    assert_eq!(
        raw.prompt("before dispose")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    let mut events = raw.subscribe(raw.snapshot().cursor.as_ref()).unwrap();
    let turn = raw.prompt("WAIT-FOR-WRITE").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while events.next().await.is_some() {
            if let Some(req) = raw
                .snapshot()
                .pending_requests
                .into_iter()
                .find(|r| r.kind == kar::RequestKind::Permission)
            {
                assert!(!target.exists());
                let body = raw
                    .last_model_request()
                    .await
                    .unwrap()
                    .unwrap()
                    .body
                    .unwrap();
                assert!(
                    body["messages"].as_array().unwrap().last().unwrap()["content"]
                        .to_string()
                        .contains("WAIT-FOR-WRITE")
                );
                raw.decide(&req, kar::PermissionDecision::AllowOnce)
                    .await
                    .unwrap();
                return;
            }
        }
        panic!("Claude exited before requesting Write");
    })
    .await
    .unwrap();
    assert_eq!(turn.wait().await.unwrap(), Outcome::Completed);
    let body = raw
        .last_model_request()
        .await
        .unwrap()
        .unwrap()
        .body
        .unwrap();
    assert!(
        body["messages"].as_array().unwrap().last().unwrap()["content"]
            .to_string()
            .contains("tool_result")
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "inspected");
    raw.close().await.unwrap();
    assert!(raw.last_model_request().await.unwrap().is_none());
    http.abort();
}

#[tokio::test]
#[ignore = "requires KAR_CLAUDE_BIN >=2.1.326; isolated source transcript, SessionStart hook and loopback model"]
async fn side_questions_leave_running_and_stopped_native_transcripts_intact() {
    use axum::response::Response;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;
    type SideState = (Requests, PathBuf, Arc<Notify>, Arc<Notify>, Arc<AtomicBool>);
    async fn delay_source(
        State((calls, target, held, release, active)): State<SideState>,
        Json(body): Json<Value>,
    ) -> Response {
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .map(|m| m["content"].to_string())
            .unwrap_or_default();
        if last.contains("hold this source turn")
            && !last.contains("question for")
            && active.swap(false, Ordering::SeqCst)
        {
            held.notify_one();
            release.notified().await;
        }
        model(State((calls, target)), Json(body))
            .await
            .into_response()
    }
    fn transcripts(dir: &std::path::Path) -> std::collections::BTreeSet<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|path| path.extension().is_some_and(|s| s == "jsonl"))
            .collect()
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    let marker = root.join("observed-hooks");
    std::fs::write(home.join("settings.json"),json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"printf x >> \"$KAR_TEST_SIDE_HOOK\""}]}]}}).to_string()).unwrap();
    let calls = Requests::default();
    let held = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/messages", post(delay_source))
        .with_state((
            calls.clone(),
            root.join("write"),
            held.clone(),
            release.clone(),
            Arc::new(AtomicBool::new(true)),
        ));
    let http = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut opts = SessionOptions::new(&work);
    for (key, value) in [
        ("HOME", home.as_os_str()),
        ("CLAUDE_CONFIG_DIR", home.as_os_str()),
        ("KAR_TEST_SIDE_HOOK", marker.as_os_str()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".as_ref()),
        ("ANTHROPIC_API_KEY", "loopback-only".as_ref()),
        ("ANTHROPIC_AUTH_TOKEN", "".as_ref()),
        ("ANTHROPIC_BASE_URL", url.as_ref()),
    ] {
        opts.env.insert(key.into(), value.into());
    }
    opts.model = Some("sonnet".into());
    let mut rt = kar::runtime("claude").unwrap();
    rt.program = std::env::var_os("KAR_CLAUDE_BIN").unwrap().into();
    let installation = rt
        .probe_installation(&kar::InstallationOptions {
            cwd: Some(work.clone()),
            env: opts
                .env
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect(),
        })
        .await
        .unwrap();
    assert_eq!(
        rt.session_capabilities(&installation)
            .unwrap()
            .side_question,
        Some(true)
    );
    let source = rt.open(opts.clone()).await.unwrap();
    assert_eq!(
        source
            .prompt("saved source history")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    let file = source.native_session_file().await.unwrap().unwrap();
    let mut side = kar::SideQuestionOptions::new(&work);
    side.env = opts.env;
    side.model = opts.model;
    side.timeout = Duration::from_secs(30);
    let mut cancelled = side.clone();
    cancelled.signal = tokio_util::sync::CancellationToken::new();
    cancelled.signal.cancel();
    let before = calls.lock().unwrap().len();
    let bytes = std::fs::read(&file).unwrap();
    assert!(matches!(
        rt.side_question(source.id(), "cancel before fork", &cancelled).await,
        Err(kar::Error::Refused(reason)) if reason == "side question cancelled"
    ));
    assert_eq!(calls.lock().unwrap().len(), before);
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    let ask = |question: &'static str, running: bool| {
        let (rt, side, source, file, marker, calls) = (&rt, &side, &source, &file, &marker, &calls);
        async move {
            let source_before = std::fs::read(file).unwrap();
            let files = transcripts(file.parent().unwrap());
            let hooks = std::fs::read(marker).unwrap();
            assert!(!hooks.is_empty());
            assert_eq!(
                rt.side_question(source.id(), question, side).await.unwrap(),
                "fixture answer"
            );
            let capture = calls.lock().unwrap();
            let matches: Vec<_> = capture
                .iter()
                .filter(|body| body["messages"].to_string().contains(question))
                .collect();
            assert_eq!(matches.len(), 1);
            assert!(matches[0]["tools"].as_array().is_some_and(Vec::is_empty));
            assert!(
                matches[0]["messages"]
                    .to_string()
                    .contains("saved source history")
            );
            assert_eq!(std::fs::read(marker).unwrap(), hooks);
            assert_eq!(transcripts(file.parent().unwrap()), files);
            let after = std::fs::read(file).unwrap();
            assert!(!String::from_utf8_lossy(&after).contains(question));
            if running {
                assert!(after.starts_with(&source_before));
            } else {
                assert_eq!(after, source_before);
            }
        }
    };
    let turn = source.prompt("hold this source turn").await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), held.notified())
        .await
        .unwrap();
    assert!(source.status().is_running());
    ask("question for running source", true).await;
    release.notify_one();
    assert_eq!(turn.wait().await.unwrap(), Outcome::Completed);
    source.close().await.unwrap();
    ask("question for stopped source", false).await;
    assert!(
        rt.side_question(
            &Uuid::new_v4().to_string(),
            "question for missing source",
            &side
        )
        .await
        .is_err()
    );
    assert!(!calls.lock().unwrap().iter().any(|body| {
        body["messages"]
            .to_string()
            .contains("question for missing source")
    }));
    http.abort();
}
