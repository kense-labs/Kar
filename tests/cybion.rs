#[path = "support/abort_turn.rs"]
mod abort_turn;
#[path = "support/disposal.rs"]
mod disposal;
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    routing::any,
};
use kar::{Error, Event, InputImage, Outcome, SessionOptions, community::Cybion};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Deployment {
    created: Value,
    history: Vec<Value>,
    display: String,
    model: Option<String>,
    polls: u32,
    submissions: Vec<Value>,
    cancels: usize,
}
impl Deployment {
    fn view(&self) -> Value {
        json!({"id":"thread/42","model":self.model.as_deref().unwrap_or("fixture"),"reasoning_effort":"high",
            "status":if self.display == "running" {"running"} else {"idle"},
            "display_status": self.display,
            "usage":{"input_tokens":10 + self.submissions.len() * 4,"output_tokens":2+self.submissions.len(),"cached_tokens":5+self.submissions.len()*2}})
    }
    fn row(&mut self, kind: &str, payload: Value) -> usize {
        let id = self.history.len() + 1;
        self.history
            .push(json!({"id":id,"kind":kind,"payload":payload}));
        id
    }
    fn reply(&mut self, text: &str) {
        self.row(
            "response_output",
            json!({"type":"message","content":[{"type":"output_text","text":text}]}),
        );
        self.display = "completed".into();
    }
}
async fn handle(
    State(state): State<Arc<Mutex<Deployment>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> (StatusCode, Json<Value>) {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer local-fixture") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"bad credential"})),
        );
    }
    let mut s = state.lock().unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let result = match (method.as_str(), uri.path()) {
        ("POST", "/deploy/v1/threads") => {
            s.created = body;
            s.view()
        }
        ("GET", "/deploy/v1/threads/thread%2F42") => {
            s.polls += 1;
            s.view()
        }
        ("GET", "/deploy/v1/threads/thread%2F42/history") => json!(s.history),
        ("POST", "/deploy/v1/threads/thread%2F42/inputs") => {
            if body["input"] == "reject" {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error":"input refused"})),
                );
            }
            let idx = s.row("input", json!({"content":body["input"]}));
            s.display = if body["input"] == "defer until later" {
                "completed"
            } else {
                "running"
            }
            .into();
            if body["input"] == "quick" {
                s.reply("quick answer");
            }
            s.submissions.push(body);
            json!({"thread_id":"thread/42","record_idx":idx,"status":"accepted"})
        }
        ("POST", "/deploy/api/threads/thread%2F42/cancel") => {
            s.cancels += 1;
            s.display = "stopped".into();
            s.view()
        }
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"unknown thread or route"})),
            );
        }
    };
    (StatusCode::OK, Json(result))
}
async fn fixture() -> (String, Arc<Mutex<Deployment>>) {
    let state = Arc::new(Mutex::new(Deployment::default()));
    let router = Router::new()
        .fallback(any(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/deploy/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, state)
}

fn sourced(kind: kar::InputOriginKind, source: &str) -> kar::InputOptions {
    kar::InputOptions {
        origin: Some(kar::InputOrigin {
            kind,
            source: Some(source.into()),
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn equal_inputs_replay_with_distinct_host_origins() {
    let (url, state) = fixture().await;
    let session = Cybion::new(&url, "local-fixture")
        .unwrap()
        .open(SessionOptions::new("/nonlocal"))
        .await
        .unwrap();
    for (kind, label) in [
        (kar::InputOriginKind::User, "keyboard"),
        (kar::InputOriginKind::Automation, "ci-worker"),
    ] {
        let turn = session
            .prompt_with_options("quick", sourced(kind, label))
            .await
            .unwrap();
        let receipt = turn.admission().unwrap();
        let saved: kar::Record =
            serde_json::from_slice(&serde_json::to_vec(&*receipt.request).unwrap()).unwrap();
        assert_eq!(saved.origin, sourced(kind, label).origin);
        assert_eq!(turn.report().await.unwrap().text, "quick answer");
    }
    let history = session.conversation();
    assert_eq!(history.inputs().len(), 2);
    for (item, source) in history.inputs().iter().zip(["keyboard", "ci-worker"]) {
        assert_eq!(item.text, "quick");
        assert_eq!(
            item.origin.as_ref().and_then(|o| o.source.as_deref()),
            Some(source)
        );
        assert_eq!(item.state, kar::observe::DeliveryState::Accepted);
        assert_eq!(item.attempts.len(), 1);
        assert_eq!(item.attempts[0].state, kar::observe::AttemptState::Accepted);
        assert!(
            item.observations.is_empty(),
            "Cybion transcripts do not identify the host UUID"
        );
    }
    assert_ne!(history.inputs()[0].key, history.inputs()[1].key);
    let mut initial = session.observe_conversation(None).unwrap();
    assert_eq!(initial.next().await.unwrap().inputs().len(), 2);
    let after = history.inputs()[0].attempts[0]
        .response
        .as_ref()
        .unwrap()
        .cursor();
    let mut subscription = session.observe_conversation(Some(&after)).unwrap();
    let resumed = subscription.next().await.unwrap();
    assert_eq!(resumed.cursors()[&after.stream_id], after.after_seq + 1);
    assert_eq!(resumed.inputs()[0].origin, history.inputs()[0].origin);
    let foreign = kar::Cursor {
        stream_id: uuid::Uuid::new_v4(),
        ..after
    };
    assert!(matches!(
        session.observe_conversation(Some(&foreign)),
        Err(Error::InvalidCursor)
    ));
    assert_eq!(
        serde_json::to_value(&history).unwrap(),
        serde_json::to_value(kar::observe::conversation_of(
            &session.records()[..=history.cursors()[&session.stream_id()] as usize]
        ))
        .unwrap()
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(state.submissions.len(), 2);
        assert_eq!(state.submissions[0], state.submissions[1]);
        assert!(!state.submissions[0].to_string().contains("keyboard"));
        assert!(!state.submissions[0].to_string().contains("ci-worker"));
    }
    session.close().await.unwrap();
}

#[tokio::test]
async fn delayed_steer_preserves_the_running_turn_and_holds_the_queue() {
    let (url, state) = fixture().await;
    let session = Cybion::new(&url, "local-fixture")
        .unwrap()
        .open(SessionOptions::new("/nonlocal"))
        .await
        .unwrap();
    let turn = session.prompt("running").await.unwrap();
    state.lock().unwrap().row(
        "response_output",
        json!({"type":"message","content":[{"type":"output_text","text":"already working"}]}),
    );
    tokio::time::timeout(Duration::from_secs(6), async {
        while !session
            .records()
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e,Event::TextDelta {text} if text=="already working"))
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    session
        .steer_with_options(
            "defer until later",
            sourced(kar::InputOriginKind::Notification, "worker"),
        )
        .await
        .unwrap();
    let queued = session
        .queue_with_options(
            "quick",
            sourced(kar::InputOriginKind::Automation, "scheduler"),
        )
        .await
        .unwrap();
    let id = queued.input_id();
    session.withdraw(id).await.unwrap();
    assert!(matches!(queued.report().await, Err(Error::Refused(_))));
    let lookup = kar::observe::ConversationKey {
        session_id: Some(session.id().into()),
        agent_path: vec![],
        stream_id: None,
        id: id.to_string(),
    };
    assert_eq!(
        session.conversation().get(&lookup).unwrap().state,
        kar::observe::DeliveryState::Withdrawn
    );
    let queued = session
        .queue_with_options(
            "quick",
            kar::InputOptions {
                input_id: Some(id),
                ..sourced(kar::InputOriginKind::Automation, "retry-scheduler")
            },
        )
        .await
        .unwrap();
    for (control, label) in [("steer", "worker"), ("queue", "scheduler")] {
        assert!(session.records().iter().any(|r| r.native["type"] == control
            && r.origin.as_ref().and_then(|o| o.source.as_deref()) == Some(label)));
    }
    assert!(
        state
            .lock()
            .unwrap()
            .submissions
            .iter()
            .all(|request| request.get("origin").is_none())
    );
    let initial = {
        let mut s = state.lock().unwrap();
        s.model = Some("updated-fixture".into());
        s.polls
    };
    tokio::time::timeout(Duration::from_secs(9), async {
        while state.lock().unwrap().polls < initial + 3
            || session.snapshot().model.as_deref() != Some("updated-fixture")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    {
        let mut s = state.lock().unwrap();
        assert_eq!(s.submissions.len(), 2);
        // The owner stops the same already-observed turn while the steer is still waiting.
        s.display = "stopped".into();
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(6), turn.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Aborted
    );
    assert_eq!(queued.report().await.unwrap().text, "quick answer");
    use kar::observe::{AttemptState, InputAttemptKind::*};
    let history = session.conversation();
    let sent = history.get(&lookup).unwrap();
    assert_eq!(
        sent.attempts.iter().map(|a| a.kind).collect::<Vec<_>>(),
        [Queue, Withdraw, Queue, Prompt]
    );
    assert!(
        sent.attempts
            .iter()
            .all(|a| a.state == AttemptState::Accepted)
    );
    assert_eq!(sent.carries, [id.to_string()]);
    assert_eq!(
        sent.origin.as_ref().unwrap().source.as_deref(),
        Some("retry-scheduler")
    );

    assert_eq!(state.lock().unwrap().submissions.len(), 3);
    session.close().await.unwrap();
}

#[tokio::test]
async fn native_completion_during_dispose_precedes_its_receipt() {
    let (url, state) = fixture().await;
    let session = Cybion::new(&url, "local-fixture")
        .unwrap()
        .open(SessionOptions::new("/nonlocal"))
        .await
        .unwrap();
    let turn = session.prompt("finish while detaching").await.unwrap();
    let (_, queued) = session.queue("held").await.unwrap();
    let mut stream = session.subscribe(None).unwrap();
    let responder = async {
        while let Some(row) = stream.next().await {
            if row.native["type"] == "dispose" {
                state.lock().unwrap().reply("finished remotely");
                return;
            }
        }
        panic!("dispose request missing");
    };
    let (closed, ()) = tokio::join!(disposal::check(&session, "accepted"), responder);
    closed.unwrap();
    assert_eq!(turn.report().await.unwrap().text, "finished remotely");
    assert!(queued.report().await.is_err());
    let records = session.records();
    let end = records
        .iter()
        .find(|r| {
            r.events.iter().any(|e| {
                matches!(
                    e,
                    Event::TurnEnded {
                        outcome: Outcome::Completed
                    }
                )
            })
        })
        .unwrap();
    assert!(end.seq < records.last().unwrap().seq);
    let state = state.lock().unwrap();
    assert_eq!((state.submissions.len(), state.cancels), (1, 0));
}

#[tokio::test]
async fn thread_control_and_replay_use_the_native_api() {
    let (url, state) = fixture().await;
    let api = Cybion::new(&url, "local-fixture").unwrap();
    let mut opts = SessionOptions::new("/nonlocal/workspace");
    opts.model = Some("fixture".into());
    opts.settings.name = Some("Review".into());
    opts.settings.effort = Some("high".into());
    let session = api.open(opts.clone()).await.unwrap();
    assert_eq!(
        state.lock().unwrap().created,
        json!({"title":"Review","model":"fixture","reasoning_effort":"high"})
    );
    assert_eq!(session.id(), "thread/42");
    assert!(session.capabilities().steering);
    assert!(!session.capabilities().session_mcp);
    assert_eq!(session.capabilities().mcp_transports, Some([].as_slice()));
    assert!(matches!(session.models().await, Err(Error::Unsupported(_))));
    let image = InputImage::from_bytes(b"GIF89a...").unwrap();
    let turn = session
        .prompt_images("hold", vec![image.clone()])
        .await
        .unwrap();
    assert!(matches!(session.prompt("overlap").await, Err(Error::Busy)));
    let (_, queued) = session.queue("quick").await.unwrap();
    let (id, withdrawn) = session.queue("discard").await.unwrap();
    let admission = withdrawn.admission().unwrap();
    assert_eq!(admission.request.native["type"], "queue");
    assert_eq!(
        admission.request.control_request_id(),
        admission.response.control_response_to()
    );
    assert_eq!(admission.response.native["type"], "accepted");
    assert_eq!(admission.response.native["queued"], "adapter");
    session.withdraw(id).await.unwrap();
    assert!(withdrawn.wait().await.is_err());
    session
        .steer_turn(
            turn.input_id(),
            Some(
                turn.admission()
                    .unwrap()
                    .request
                    .control_request_id()
                    .unwrap()
                    .parse()
                    .unwrap(),
            ),
            uuid::Uuid::new_v4(),
            "follow-up",
            vec![image],
        )
        .await
        .unwrap();
    {
        let mut s = state.lock().unwrap();
        assert!(s.submissions.iter().any(|v| {
            v["input"] == "follow-up"
                && v["images"][0]
                    .as_str()
                    .is_some_and(|s| s.starts_with("data:image/gif;base64,"))
        }));
        assert!(
            s.submissions[0]["images"][0]
                .as_str()
                .unwrap()
                .starts_with("data:image/gif;base64,")
        );
        s.row(
            "response_output",
            json!({"type":"reasoning","summary":[{"text":"consider"}]}),
        );
        s.row(
            "response_output",
            json!({"type":"function_call","call_id":"t1","name":"read","arguments":r#"{"path":"example.rs"}"#}),
        );
        s.row("tool_output", json!({"call_id":"t1","output":"file"}));
        s.row(
            "response_output",
            json!({"type":"custom_tool_call","call_id":"t2","name":"apply","input":"raw input"}),
        );
        s.row(
            "tool_output",
            json!({"call_id":"t2","output":[{"type":"private","data":"preserved"}]}),
        );
        s.row("activity", json!({"unmapped":true}));
        s.reply("first answer");
    }
    let report = tokio::time::timeout(Duration::from_secs(5), turn.report())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (report.outcome, report.text),
        (Outcome::Completed, "first answer".into())
    );
    assert_eq!(queued.report().await.unwrap().text, "quick answer");
    let records = session.records();
    let mut view = kar::observe::SessionView::default();
    for record in &records {
        view.apply(record).unwrap();
    }
    let tools = &view
        .agents
        .values()
        .find(|a| a.tools.contains_key("t1"))
        .unwrap()
        .tools;
    assert_eq!(tools["t1"].input, Some(json!(r#"{"path":"example.rs"}"#)));
    let result = tools["t1"].result.as_ref().unwrap();
    assert_eq!(result.text().as_deref(), Some("file"));
    assert!(result.status.is_none() && result.exit_code.is_none());
    assert_eq!(tools["t2"].input, Some(json!("raw input")));
    assert_eq!(
        tools["t2"].result.as_ref().unwrap().content,
        Some(vec![kar::OutputPart::Other {
            value: json!([{"type":"private","data":"preserved"}])
        }])
    );
    let measured = session.usage().total.unwrap();
    assert_eq!(
        (measured.input, measured.output, measured.cache_read),
        (12, 3, Some(6))
    );
    let projected: Vec<_> = records.iter().flat_map(|r| &r.events).collect();
    assert_eq!(
        projected
            .iter()
            .filter(|e| matches!(e, Event::TextDelta { .. }))
            .count(),
        2
    );
    assert!(
        projected
            .iter()
            .any(|e| matches!(e,Event::ReasoningDelta{text} if text == "consider"))
    );
    assert!(
        projected
            .iter()
            .any(|e| matches!(e,Event::ToolEnded{id} if id == "t1"))
    );
    assert!(
        projected
            .iter()
            .any(|e| matches!(e,Event::UserMessage{text} if text == "follow-up"))
    );
    assert!(
        projected
            .iter()
            .any(|e| matches!(e,Event::Usage{usage} if usage["input_tokens"] == 12))
    );
    assert!(
        records
            .iter()
            .any(|r| r.native["kind"] == "activity" && r.events.is_empty())
    );
    assert!(
        records
            .iter()
            .all(|r| r.session_id.as_deref() == Some("thread/42")
                && !r.native.to_string().contains("local-fixture"))
    );
    session.close().await.unwrap();

    opts.resume = Some(session.id().into());
    let resumed = api.open(opts).await.unwrap();
    assert_ne!(session.stream_id(), resumed.stream_id());
    let turn = resumed.prompt("another").await.unwrap();
    assert_eq!(abort_turn::check(&resumed, turn).await, Outcome::Aborted);
    assert!(
        !resumed
            .records()
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e, Event::TextDelta { .. }))
    );
    let detached = resumed.prompt("continue on worker").await.unwrap();
    disposal::check(&resumed, "accepted").await.unwrap();
    assert!(matches!(detached.wait().await, Err(Error::Closed(_))));
    let s = state.lock().unwrap();
    assert_eq!(s.cancels, 1);
    assert_eq!(s.display, "running");
    assert_eq!(s.submissions.len(), 5);
}

#[tokio::test]
async fn refusal_and_cli_resume() {
    let (url, _) = fixture().await;
    let opts = SessionOptions::new(".");
    assert!(
        matches!(Cybion::new(&url,"wrong").unwrap().open(opts.clone()).await,Err(Error::Refused(m)) if m.contains("401"))
    );
    let api = Cybion::new(&url, "local-fixture").unwrap();
    let mut mismatch = opts.clone();
    mismatch.resume = Some("thread/42".into());
    mismatch.model = Some("other".into());
    assert!(matches!(
        api.open(mismatch).await,
        Err(Error::Unsupported(_))
    ));
    let mut unknown = opts.clone();
    unknown.resume = Some("missing".into());
    assert!(
        matches!(api.open(unknown).await,Err(Error::Open(e)) if matches!(e.as_ref(),kar::OpenError::SessionNotFound{session_id,cause,..} if session_id=="missing" && cause.method=="GET /v1/threads/{id}" && cause.native["status"]==404))
    );
    let session = api.open(opts).await.unwrap();
    assert!(matches!(session.prompt("reject").await,Err(Error::Refused(m)) if m.contains("422")));
    assert!(session.abort().await.is_err());
    session.close().await.unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["run", "cybion", "quick", "--resume", "thread/42"])
        .env("KAR_CYBION_BASE_URL", &url)
        .env("KAR_CYBION_API_KEY", "local-fixture")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("quick answer"));
}

#[tokio::test]
#[ignore = "requires a real Cybion deployment, authorised test key, version label and a model-enabled worker"]
async fn deployed_thread_creation_and_resume() {
    let version =
        std::env::var("KAR_CYBION_DEPLOYMENT_VERSION").expect("deployment release or commit label");
    assert!(!version.trim().is_empty());
    eprintln!("Cybion deployment version supplied by operator {version}");
    let api = Cybion::from_env().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut opts = SessionOptions::new(workspace.path());
    let name = format!("Kar acceptance {}", uuid::Uuid::new_v4());
    opts.settings.name = Some(name);
    opts.settings.effort = std::env::var("KAR_CYBION_TEST_EFFORT").ok();
    opts.model = std::env::var("KAR_CYBION_TEST_MODEL").ok();
    let first = api.open(opts.clone()).await.unwrap();
    let id = first.id().to_owned();
    assert!(!id.is_empty());
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(180),
            first
                .prompt("Reply with the word ready.")
                .await
                .unwrap()
                .wait()
        )
        .await
        .unwrap()
        .unwrap(),
        Outcome::Completed
    );
    let original = first.stream_id();
    first.close().await.unwrap();
    opts.resume = Some(id.clone());
    let second = api.open(opts.clone()).await.unwrap();
    assert_eq!(second.id(), id);
    assert_ne!(second.stream_id(), original);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(180),
            second
                .prompt("Reply with the word resumed.")
                .await
                .unwrap()
                .wait()
        )
        .await
        .unwrap()
        .unwrap(),
        Outcome::Completed
    );
    second.close().await.unwrap();
    opts.resume = Some(format!("kar-absent-{}", uuid::Uuid::new_v4()));
    let missing = opts.resume.clone().unwrap();
    assert!(
        matches!(api.open(opts).await,Err(Error::Open(e)) if matches!(e.as_ref(),kar::OpenError::SessionNotFound{session_id,cause,..} if session_id==&missing && cause.native["status"]==404))
    );
}
