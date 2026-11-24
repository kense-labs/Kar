#[path = "support/declaration.rs"]
mod declaration;

use axum::{Json, Router, body::Body, extract::State, http::Response, routing::post};
use kar::{
    Protocol, SessionOptions,
    chat::{ChatReplica, ChatStore, Item, Role, TurnStatus},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Default)]
struct Requests {
    messages: Arc<Mutex<Vec<Value>>>,
    saw_live_tool: Arc<tokio::sync::Notify>,
    model_started: Arc<tokio::sync::Notify>,
    finish_model: Arc<tokio::sync::Notify>,
}
async fn model(State(requests): State<Requests>, Json(body): Json<Value>) -> Response<Body> {
    let msgs = body["messages"].as_array().unwrap();
    let resumed = msgs
        .iter()
        .any(|m| m["role"] == "user" && m["content"].to_string().contains("KAP_SECOND"));
    let marker = msgs
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| {
            let text = m["content"].to_string();
            ["KAP_HOLD", "KAP_STEER", "KAP_FIFO"]
                .into_iter()
                .find(|v| text.contains(*v))
        })
        .next_back();
    if marker == Some("KAP_HOLD") {
        requests.model_started.notify_one();
        requests.finish_model.notified().await;
    }
    let needs_tool = marker.is_none()
        && !resumed
        && !msgs
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "kar-kap-tool");
    requests.messages.lock().unwrap().push(body.clone());
    if marker.is_none() && !resumed && !needs_tool {
        requests.saw_live_tool.notified().await;
    }
    let delta = if needs_tool {
        let arguments =
            json!({"command":"printf kap-native-output","description":"Print a test marker"});
        json!({"role":"assistant","tool_calls":[{
            "index":0, "id":"kar-kap-tool", "type":"function",
            "function":{"name":"Bash","arguments":arguments.to_string()}
        }]})
    } else {
        let reply = if resumed {
            "kap-second-answer"
        } else {
            "kap-first-answer"
        };
        json!({"role":"assistant","content":marker.unwrap_or(reply)})
    };
    let mut sse = String::new();
    for (d, reason) in [
        (delta, Value::Null),
        (
            json!({}),
            json!(if needs_tool { "tool_calls" } else { "stop" }),
        ),
    ] {
        let chunk = json!({"id":"chatcmpl-kap","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":d,"finish_reason":reason}]});
        sse.push_str(&format!("data: {chunk}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}
#[tokio::test]
#[ignore = "requires Kimi KAP; uses an isolated home and a loopback model"]
async fn kap_tool_turn_replay_and_native_resume() {
    exercise(true).await;
}
#[tokio::test]
#[ignore = "requires unpatched Kimi 2.7.2; uses the native ACP fallback and a loopback model"]
async fn official_cli_falls_back_and_resumes_over_acp() {
    exercise(false).await;
}
async fn exercise(kap: bool) {
    let requests = Requests::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(model))
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(state.join("config.toml"),format!("default_model = \"fixture\"\nauto_session_title = false\n[loop_control]\nmax_attempts_per_step = 1\n[providers.local]\ntype = \"openai\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\napi_key = \"local-test\"\n[models.fixture]\nprovider = \"local\"\nmodel = \"fixture\"\nmax_context_size = 65536\n")).unwrap();
    let bin = PathBuf::from(
        std::env::var_os(if kap {
            "KAR_KIMI_BIN"
        } else {
            "KAR_KIMI_ACP_BIN"
        })
        .unwrap(),
    );
    let store = ChatStore::open(dir.path().join("store")).unwrap();
    let mut opts = SessionOptions::new(&work);
    for k in ["HOME", "USERPROFILE"] {
        opts.env.insert(k.into(), dir.path().as_os_str().into());
    }
    for k in [
        "KIMI_CODE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        opts.env.insert(k.into(), state.as_os_str().into());
    }
    for (k, v) in [
        ("KIMI_DISABLE_TELEMETRY", "1"),
        ("KIMI_CODE_MODEL_CATALOG_REFRESH_ON_START", "0"),
    ] {
        opts.env.insert(k.into(), v.into());
    }
    let extra = dir.path().join("additional-workspace");
    std::fs::create_dir(&extra).unwrap();
    let extra = extra.canonicalize().unwrap();
    opts.settings.additional_directories.push(extra.clone());
    if kap {
        opts.settings.append_system_prompt = Some("KAP_APPEND_INSTRUCTION".into());
    }
    opts.model = Some("fixture".into());
    opts.approvals = kar::ApprovalMode::Host;
    if kap {
        let mut missing = opts.clone();
        missing.settings.additional_directories.clear();
        let id = Uuid::new_v4().to_string();
        missing.resume = Some(id.clone());
        let mut runtime = kar::runtime("kimi").unwrap();
        runtime.program = bin.clone();
        let e = runtime
            .open(missing)
            .await
            .err()
            .expect("missing session opened");
        assert!(
            matches!(&e, kar::Error::Open(e) if matches!(e.as_ref(),kar::OpenError::SessionNotFound {session_id,cause,..} if session_id==&id && cause.method=="GET /sessions/{id}")),
            "{e}"
        );
    }
    let mut rt = kar::runtime("kimi").unwrap();
    rt.program = bin.clone();
    let declared = declaration::for_installation(&rt, &opts).await;
    let session = store
        .create("kimi", Some(&bin), opts.clone())
        .await
        .unwrap();
    declaration::check(&declared, session.capabilities());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let reading = session.resources().await.expect("owned Kimi process");
        assert!(reading.rss > 0 && reading.processes >= 1);
        assert!(reading.scope.is_none());
    }
    if !kap {
        assert_eq!(session.info().protocol, Protocol::Acp);
        let id = session.info().id;
        session
            .send(Uuid::new_v4(), "KAP_SECOND before ACP restart")
            .await
            .unwrap();
        session.close().await.unwrap();
        let first_requests = requests.messages.lock().unwrap().len();
        let explicit = store
            .resume_with(id, |o| {
                o.env.clone_from(&opts.env);
                o.settings.additional_directories.push(extra.clone());
            })
            .await;
        assert!(
            matches!(explicit, Err(kar::Error::Unsupported(reason)) if reason.contains("only at creation"))
        );
        let resumed = store
            .resume_with(id, |o| o.env = opts.env.clone())
            .await
            .unwrap();
        resumed
            .send(Uuid::new_v4(), "KAP_SECOND after ACP restart")
            .await
            .unwrap();
        resumed.close().await.unwrap();
        let history = store.history(id).unwrap();
        assert_eq!(history.turns.len(), 2);
        assert!(
            history
                .turns
                .iter()
                .all(|t| t.status == TurnStatus::Completed)
        );
        assert!(requests.messages.lock().unwrap().iter().any(|b| {
            let data = b["messages"].to_string();
            data.contains("before ACP restart") && data.contains("after ACP restart")
        }));
        let observed = requests.messages.lock().unwrap();
        for (marker, batch) in [
            ("before ACP restart", &observed[..first_requests]),
            ("after ACP restart", &observed[first_requests..]),
        ] {
            assert!(
                batch.iter().any(|r| {
                    let messages = r["messages"].as_array().unwrap();
                    messages
                        .iter()
                        .any(|m| m["role"] == "user" && m["content"].to_string().contains(marker))
                        && messages.iter().any(|m| {
                            m["role"] == "system"
                                && m["content"].to_string().contains(extra.to_str().unwrap())
                        })
                }),
                "the model did not receive its additional workspace {marker}"
            );
        }
        http.abort();
        return;
    }
    assert_eq!(session.info().protocol, Protocol::KimiKap);
    let id = session.info().id;
    let native = session.info().native_id.clone();
    let mut a = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut feed = store.follow(id, Some(a.cursor())).unwrap();
    let mut second = store.follow(id, Some(b.cursor())).unwrap();
    {
        let send = session.send(Uuid::new_v4(), "KAP_FIRST");
        tokio::pin!(send);
        let observe = async {
            let mut answered = std::collections::HashSet::new();
            let mut tool_seen = false;
            let result = loop {
                tokio::select! {
                    result = &mut send => break result.unwrap(),
                    next = feed.recv() => {
                        let event = next.unwrap();
                        assert!(a.apply(event.clone()).unwrap());
                        assert!(!a.apply(event).unwrap());
                        if !tool_seen {
                            b.apply(second.recv().await.unwrap()).unwrap();
                            if let Some(turn) = b.turns().last()
                                && turn.items.iter().any(|i| matches!(i.item, Item::Tool { .. }))
                            {
                                assert_eq!(turn.status, TurnStatus::Running);
                                tool_seen = true;
                                requests.saw_live_tool.notify_one();
                            }
                        }
                        for req in a.pending_requests().iter().filter(|q| answered.insert(q.id.to_string())) {
                            session.decide(req, kar::PermissionDecision::AllowOnce).await.unwrap();
                        }
                    }
                }
            };
            assert_eq!(result.status, TurnStatus::Completed);
        };
        tokio::time::timeout(Duration::from_secs(50), observe)
            .await
            .expect("Kimi first turn timed out");
    }
    drop(second);
    drop(feed);
    let items = &store.history(id).unwrap().turns[0].items;
    assert!(
        items
            .iter()
            .any(|i| matches!(&i.item,Item::Tool{id,..} if id=="kar-kap-tool"))
    );
    assert_eq!(
        items
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
    assert!(items.iter().any(|i|matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="kap-first-answer")));
    session.close().await.unwrap();
    // Reproduce a controller that persisted through tool start but missed the terminal
    // frames. The real Kimi session already holds the complete turn.
    let crash_dir = dir.path().join("crash-store");
    let crash_store = ChatStore::open(&crash_dir).unwrap();
    let name = format!("{id}.karchat");
    let prefix = b
        .cursor()
        .to_string()
        .rsplit_once(':')
        .unwrap()
        .1
        .parse::<usize>()
        .unwrap();
    let log = std::fs::read(dir.path().join("store").join(&name)).unwrap();
    std::fs::write(crash_dir.join(name), &log[..prefix]).unwrap();
    std::fs::File::create(crash_dir.join(format!("{id}.lock"))).unwrap();
    let mut recovering = ChatReplica::from_snapshot(crash_store.feed_snapshot(id).unwrap());
    let previous = recovering.cursor();
    let before = requests.messages.lock().unwrap().len();
    let repaired = crash_store
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    repaired.close().await.unwrap();
    let tip = crash_store.feed_snapshot(id).unwrap().cursor;
    let mut replay = crash_store.follow(id, Some(previous)).unwrap();
    while recovering.cursor() != tip {
        let event = replay.recv().await.unwrap();
        assert!(recovering.apply(event.clone()).unwrap());
        assert!(!recovering.apply(event).unwrap());
    }
    let recovered = &recovering.turns()[0];
    assert_eq!(recovered.status, TurnStatus::Completed);
    assert_eq!(recovered.items, *items);
    assert_eq!(requests.messages.lock().unwrap().len(), before);
    let queued = Uuid::new_v4();
    store.enqueue(id, queued, "KAP_SECOND").unwrap();
    let chat = store
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(chat.info().native_id, native);
    tokio::time::timeout(Duration::from_secs(40), chat.drain_queue())
        .await
        .unwrap()
        .unwrap();
    chat.drain_queue().await.unwrap();
    assert_eq!(requests.messages.lock().unwrap().len(), before + 1);
    let compact = chat
        .start_compaction()
        .await
        .unwrap()
        .wait(Some(Duration::from_secs(18)))
        .await;
    assert!(
        matches!(compact.status, kar::CompactionStatus::Completed { .. }),
        "{compact:?}"
    );
    chat.close().await.unwrap();
    let history = store.history(id).unwrap();
    let tip = store.feed_snapshot(id).unwrap().cursor;
    for replica in [&mut a, &mut b] {
        let mut f = store.follow(id, Some(replica.cursor())).unwrap();
        while replica.cursor() != tip {
            replica.apply(f.recv().await.unwrap()).unwrap();
        }
        assert_eq!(replica.turns(), history.turns);
    }
    assert_eq!(history.turns.len(), 2);
    assert_eq!(history.turns[1].id, queued);
    assert!(history.turns[1].items.iter().any(|i|matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="kap-second-answer")));
    assert!(history.turns.iter().all(|t| t.native_turn_id.is_some()));
    let mut runtime = kar::runtime("kimi").unwrap();
    runtime.program = bin;
    let mut unknown = opts.clone();
    unknown.model = Some("unconfigured-kap-model".into());
    let failure = runtime
        .open(unknown)
        .await
        .err()
        .expect("Kimi accepted an unconfirmed model");
    assert!(
        matches!(&failure, kar::Error::Open(e) if matches!(e.as_ref(), kar::OpenError::ModelConfirmation{requested,reported,catalog_selector_ids:None,..} if requested=="unconfigured-kap-model" && reported.as_deref()!=Some(requested))),
        "{failure}"
    );
    let controls = runtime.open(opts.clone()).await.unwrap();
    let held = controls.prompt("KAP_HOLD").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), requests.model_started.notified())
        .await
        .unwrap();
    controls.steer("KAP_STEER").await.unwrap();
    let (_, queued) = controls.queue("KAP_FIFO").await.unwrap();
    assert!(matches!(
        controls.start_compaction().await,
        Err(kar::Error::Busy)
    ));
    requests.finish_model.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(25), held.report())
            .await
            .unwrap()
            .unwrap()
            .outcome,
        kar::Outcome::Completed
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(25), queued.report())
            .await
            .unwrap()
            .unwrap()
            .text,
        "KAP_FIFO"
    );
    controls.close().await.unwrap();
    let seen = requests.messages.lock().unwrap();
    assert!(seen.iter().any(|m| {
        let text = m["messages"].to_string();
        [
            "KAP_FIRST",
            "kap-first-answer",
            "KAP_SECOND",
            "KAP_APPEND_INSTRUCTION",
        ]
        .iter()
        .all(|s| text.contains(s))
    }));
    assert!(seen.iter().any(|b| {
        b["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["content"].to_string().contains("kap-native-output"))
    }));
    http.abort();
}
