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
    session.steer("defer until later").await.unwrap();
    let (_, queued) = session.queue("quick").await.unwrap();
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
    assert_eq!(state.lock().unwrap().submissions.len(), 3);
    session.close().await.unwrap();
}

#[tokio::test]
async fn thread_control_and_replay_use_the_native_api() {
    let (url, state) = fixture().await;
    let api = Cybion::new(&url, "local-fixture").unwrap();
    let mut opts = SessionOptions::new("/nonlocal/workspace");
    opts.model = Some("fixture".into());
    let session = api
        .open_thread(opts.clone(), Some("Review"), Some("high"))
        .await
        .unwrap();
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
    let turn = session.prompt_images("hold", vec![image]).await.unwrap();
    assert!(matches!(session.prompt("overlap").await, Err(Error::Busy)));
    let (_, queued) = session.queue("quick").await.unwrap();
    let (id, withdrawn) = session.queue("discard").await.unwrap();
    session.withdraw(id).await.unwrap();
    assert!(withdrawn.wait().await.is_err());
    session.steer("follow-up").await.unwrap();
    {
        let mut s = state.lock().unwrap();
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
    resumed.abort().await.unwrap();
    assert_eq!(turn.wait().await.unwrap(), Outcome::Aborted);
    assert!(
        !resumed
            .records()
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e, Event::TextDelta { .. }))
    );
    let detached = resumed.prompt("continue on worker").await.unwrap();
    resumed.close().await.unwrap();
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
    assert!(matches!(api.open(mismatch).await, Err(Error::Refused(_))));
    let mut unknown = opts.clone();
    unknown.resume = Some("missing".into());
    assert!(matches!(api.open(unknown).await,Err(Error::Refused(m)) if m.contains("404")));
    let session = api.open(opts).await.unwrap();
    assert!(matches!(session.prompt("reject").await,Err(Error::Refused(m)) if m.contains("422")));
    assert!(session.abort().await.is_err());
    session.close().await.unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["run", "cybion", "quick", "--resume", "thread/42"])
        .env("OAR_CYBION_BASE_URL", &url)
        .env("OAR_CYBION_API_KEY", "local-fixture")
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
