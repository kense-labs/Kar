#[path = "support/abort_turn.rs"]
mod abort_turn;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Query, State,
        ws::{Message, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use kar::{ApprovalMode, Error, Event, InputImage, Outcome, SessionOptions, community::Morph};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Server {
    topics: u64,
    submitted: Vec<Value>,
    tasks: HashMap<String, String>,
    workspace: Vec<Value>,
    decisions: Vec<String>,
    stopped: usize,
    socket_auth: usize,
}
type Shared = Arc<Mutex<Server>>;
fn auth(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|h| h.to_str().ok()) == Some("Bearer test-token")
}
fn trace() -> Value {
    json!({"entries":[
        {"seq":1,"event":{"kind":"tool_start","tool_name":"read","activity_id":"call-1","args":{"path":"note.md"}}},
        {"seq":2,"event":{"kind":"tool_output","tool_name":"read","text":"observed"}},
        {"seq":3,"event":{"kind":"tool_done","tool_name":"read","activity_id":"call-1","text":"exit_code: 7","status":"failed"}},
        {"seq":4,"event":{"kind":"steer_applied","text":"updated input"}}
    ]})
}
async fn ws(
    State(s): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !auth(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    s.lock().unwrap().socket_auth += 1;
    let id = query["task_id"].clone();
    upgrade.on_upgrade(move|mut socket|async move{
        let frames=[
            json!({"task_id":"foreign","text":"wrong conversation"}),
            json!({"task_id":id,"status":"running","preview":true,"text":"internal tool preview"}),
            json!({"task_id":id,"status":"running","text":"hel","trace":trace()}),
            json!({"task_id":id,"status":"running","text":"hello","trace":trace(),"reasoning":"observed"}),
        ];
        for v in frames{if socket.send(Message::Text(v.to_string().into())).await.is_err(){return;}}
        while socket.recv().await.is_some(){}
    })
}
async fn http(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    if !auth(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(Value::Null));
    }
    let v = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let mut s = s.lock().unwrap();
    let parts: Vec<_> = uri.path().trim_matches('/').split('/').collect();
    let out = match (method.as_str(), parts.as_slice()) {
        ("GET", ["runtime", "llm", "profiles"]) => {
            json!({"default":{"name":"test","model":"m"},"items":[{"name":"test"},{"name":"second"}]})
        }
        ("POST", ["runtime", "topics"]) => {
            s.topics += 1;
            json!({"id":format!("topic-{}",s.topics)})
        }
        ("GET", ["runtime", "topics", "topic-1"]) => json!({"id":"topic-1"}),
        ("PUT", ["runtime", "workspace"]) => {
            s.workspace.push(v);
            json!({})
        }
        ("POST", ["runtime", "tasks"]) => {
            if v["task"] == "reject" {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"runtime refusal"})),
                );
            }
            let id = format!("task-{}", s.submitted.len() + 1);
            let status = match v["task"].as_str() {
                Some("approval") => "pending",
                Some("quick") => "done",
                _ => "running",
            };
            let answer = if v["task"] == "steer" {
                json!({"id":id,"status":"done","steer_target_task_id":"task-1"})
            } else if v["task"] == "late-refusal" {
                json!({"id":id,"status":"done"})
            } else {
                s.tasks.insert(id.clone(), status.into());
                json!({"id":id,"status":status})
            };
            s.submitted.push(v);
            answer
        }
        ("GET", ["runtime", "tasks", id]) => {
            let status = s.tasks.get(*id).cloned().unwrap_or_default();
            json!({"id":id,"status":status,"error":if status=="failed" {Some("provider returned 429")}else{None},"approval_request_id":format!("a-{id}"),
                "result":{"final":{"output":if status=="done" {"hello world"}else{"stopped message"}},"reasoning":"observed","trace":trace()}})
        }
        ("POST", ["runtime", "tasks", id, "stop"]) => {
            let found = s.tasks.contains_key(*id);
            if found {
                s.tasks.insert(id.to_string(), "canceled".into());
                s.stopped += 1;
            }
            json!({"found":found})
        }
        ("GET", ["runtime", "approvals", id]) => json!({"id":id,"action":"write"}),
        ("POST", ["runtime", "approvals", id, action @ ("approve" | "deny")]) => {
            s.decisions.push(action.to_string());
            s.tasks.insert(
                id.trim_start_matches("a-").into(),
                if *action == "approve" {
                    "done".into()
                } else {
                    "failed".into()
                },
            );
            json!({"status":action})
        }
        ("GET", ["runtime", "topic", _, "metadata"]) => {
            json!({"context":{"available":true,"used_input_tokens":30,"context_window_tokens":100,"usage_ratio":0.3}})
        }
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"missing route"})),
            );
        }
    };
    (StatusCode::OK, Json(out))
}
async fn setup() -> (Morph, Shared) {
    let s = Arc::new(Mutex::new(Server::default()));
    let router = Router::new()
        .route("/runtime/stream/ws", get(ws))
        .fallback(any(http))
        .with_state(s.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/runtime", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (Morph::attach(&url, "test-token").unwrap(), s)
}
#[tokio::test]
async fn stream_controls_approval_and_resume() {
    let (runtime, state) = setup().await;
    let mut opts = SessionOptions::new(".");
    opts.model = Some("test".into());
    opts.approvals = ApprovalMode::Host;
    let session = runtime.open(opts.clone()).await.unwrap();
    assert_eq!(session.models().await.unwrap().as_array().unwrap().len(), 2);
    assert!(matches!(
        session
            .prompt_images(
                "picture",
                vec![InputImage::from_bytes(b"GIF89a...").unwrap()]
            )
            .await,
        Err(Error::Unsupported(_))
    ));
    let mut stream = session.subscribe(None).unwrap();
    let turn = session.prompt("approval").await.unwrap();
    let (id, withdrawn) = session.queue("withdraw").await.unwrap();
    let admission = withdrawn.admission().unwrap();
    assert_eq!(admission.request.native["type"], "queue");
    assert_eq!(
        admission.request.control_request_id(),
        admission.response.control_response_to()
    );
    assert_eq!(admission.response.native["type"], "accepted");
    session.withdraw(id).await.unwrap();
    assert!(withdrawn.report().await.is_err());
    let (_, queued) = session.queue("quick").await.unwrap();
    assert!(matches!(session.prompt("busy").await, Err(Error::Busy)));
    session.steer("steer").await.unwrap();
    assert!(session.steer("late-refusal").await.is_err());
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let record = stream.next().await.unwrap();
            if let Some(id) = record.events.iter().find_map(|e| {
                if let Event::ApprovalRequested { id, .. } = e {
                    Some(id.clone())
                } else {
                    None
                }
            }) {
                break id;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        session
            .respond(approval.clone(), json!({"decision":"typo"}))
            .await
            .is_err()
    );
    session
        .respond(approval.clone(), json!({"decision":"approve"}))
        .await
        .unwrap();
    assert!(
        session
            .respond(approval, json!({"decision":"approve"}))
            .await
            .is_err()
    );
    assert_eq!(turn.report().await.unwrap().text, "hello world");
    assert_eq!(queued.report().await.unwrap().outcome, Outcome::Completed);
    let rows = session.records();
    let events: Vec<_> = rows
        .iter()
        .filter(|r| r.span_id.as_deref() == Some("task-1"))
        .flat_map(|r| &r.events)
        .collect();
    let output: String = events
        .iter()
        .filter_map(|e| match e {
            Event::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(output, "hello world");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::ToolStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::ToolEnded { .. }))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e,Event::Usage{usage} if usage["context"]["percent"]==30.0))
    );
    let mut view = kar::observe::SessionView::default();
    for record in &rows {
        view.apply(record).unwrap();
    }
    let activity = view
        .agents
        .values()
        .find_map(|a| a.tools.get("call-1"))
        .unwrap();
    assert_eq!(activity.input, Some(json!(r#"{"path":"note.md"}"#)));
    assert_eq!(activity.preview, "observed");
    let result = activity.result.as_ref().unwrap();
    assert_eq!(result.status, Some(kar::ToolStatus::Failed));
    assert_eq!(result.text().as_deref(), Some("exit_code: 7"));
    assert_eq!(result.exit_code, None);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e,Event::ToolResult {id,..} if id=="call-1"))
            .count(),
        1
    );
    assert!(rows.iter().all(|r| r.native["task_id"] != "foreign"));
    assert_eq!(state.lock().unwrap().decisions, ["approve"]);
    assert!(state.lock().unwrap().socket_auth > 0);
    let active = session.prompt("wait").await.unwrap();
    assert_eq!(abort_turn::check(&session, active).await, Outcome::Aborted);
    session.close().await.unwrap();
    opts.resume = Some(session.id().into());
    opts.approvals = ApprovalMode::Deny;
    let resumed = runtime.open(opts).await.unwrap();
    let denied = resumed.prompt("approval").await.unwrap();
    let denied = denied.report().await.unwrap();
    assert_eq!(denied.outcome, Outcome::Failed);
    let failure = denied.failure.unwrap();
    assert_eq!(failure.class, kar::FailureClass::RateLimited);
    assert_eq!(failure.reason, "provider returned 429");
    let active = resumed.prompt("wait").await.unwrap();
    resumed.close().await.unwrap();
    assert_eq!(active.wait().await.unwrap(), Outcome::Aborted);
    let state = state.lock().unwrap();
    assert_eq!(state.decisions, ["approve", "deny"]);
    assert_eq!(state.workspace.len(), 2);
    assert_eq!(state.topics, 1);
    assert_eq!(state.stopped, 2);
    assert!(state.submitted.iter().all(|v| v["llm_profile"] == "test"
        && v["topic_id"] == "topic-1"
        && v["workspace_dir"].is_string()));
}
