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
