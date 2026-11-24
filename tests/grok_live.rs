use kar::forms::{IssueCode, ModelEntry, ModelListing, ValidationContext, config_form};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command};
#[path = "support/image_input.rs"]
mod image_input;

#[test]
#[ignore = "requires the official Grok CLI with the ACP model listing extension"]
fn isolated_grok_lists_models_for_the_runtime_form_without_a_chat() {
    let grok = PathBuf::from(std::env::var_os("KAR_GROK_BIN").expect("set KAR_GROK_BIN"))
        .canonicalize()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_kar"))
            .env_clear()
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("GROK_HOME", dir.path().join(".grok"))
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let form = config_form("grok").unwrap();
    assert_eq!(run(&["config-form"]), json!({"grok":form}));
    assert_eq!(
        run(&["config-form", "grok"]),
        serde_json::to_value(&form).unwrap()
    );
    let rows: Vec<ModelEntry> =
        serde_json::from_value(run(&["models", "grok", "--bin", grok.to_str().unwrap()])).unwrap();
    let default = rows
        .iter()
        .find(|model| model.default)
        .expect("native default model");
    let effort = default
        .effort_levels
        .as_ref()
        .and_then(|v| v.first())
        .expect("default model offers effort");
    let good = json!({"model":{"kind":"preset","id":default.id},"reasoningEffort":effort});
    let bad = json!({"model":{"kind":"preset","id":default.id},"reasoningEffort":"fixture-value-not-in-menu"});
    let listing = ModelListing::from_result(Ok(rows), vec![]);
    assert!(
        form.validate(
            good.as_object().unwrap(),
            ValidationContext::Create { models: &listing }
        )
        .is_empty()
    );
    assert_eq!(
        form.validate(
            bad.as_object().unwrap(),
            ValidationContext::Create { models: &listing }
        )[0]
        .code,
        IssueCode::EffortNotOffered
    );
}

use image_input::PNG;

#[tokio::test]
#[ignore = "requires KAR_GROK_BIN; a local chat-completions model observes the image bytes"]
async fn native_image_reaches_the_model() {
    use axum::{
        Json, Router,
        extract::State,
        response::{IntoResponse, Response},
        routing::post,
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::sync::{Arc, Mutex};
    async fn chat(
        State(requests): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> Response {
        let main = body["messages"].to_string().contains("KAR_GROK_IMAGE");
        requests.lock().unwrap().push(body["messages"].clone());
        let text = if main {
            "GROK_IMAGE_RECEIVED"
        } else {
            "Fixture title"
        };
        if body["stream"] == true {
            let chunks = [
                json!({"delta":{"role":"assistant","content":text},"finish_reason":null,"index":0}),
                json!({"delta":{},"finish_reason":"stop","index":0}),
            ];
            let mut sse = String::new();
            for choice in chunks {
                let data = json!({"id":"grok-fixture","object":"chat.completion.chunk","model":"fixture","created":1,"choices":[choice]});
                sse.push_str(&format!("data: {data}\n\n"));
            }
            sse.push_str("data: [DONE]\n\n");
            ([("content-type", "text/event-stream")], sse).into_response()
        } else {
            Json(json!({"id":"grok-fixture","object":"chat.completion","model":"fixture","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})).into_response()
        }
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(post(chat))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let opts = local_options(&temp, addr);
    let expected_image = format!("data:image/png;base64,{PNG}");
    let last_user_has_image = |calls: &[Value]| {
        calls.iter().any(|messages| {
            messages
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| m["content"].as_array())
                .is_some_and(|parts| {
                    parts.iter().any(|p| {
                        p.pointer("/image_url/url").and_then(Value::as_str)
                            == Some(expected_image.as_str())
                    })
                })
        })
    };
    let mut rt = kar::runtime("grok").unwrap();
    rt.program = std::env::var_os("KAR_GROK_BIN").unwrap().into();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let session = rt.open(opts).await.unwrap();
        assert!(session.capabilities().image_input);
        image_input::empty_controls(&session).await;
        let path = temp.path().join(".").join("grok image.png");
        std::fs::write(&path, STANDARD.decode(PNG).unwrap()).unwrap();
        for (name, code) in [
            ("missing.png", "error"),
            ("notes.txt", "unsupported"),
            ("broken.png", "unsupported"),
        ] {
            let invalid = temp.path().join(name);
            if name == "broken.png" {
                std::fs::write(&invalid, b"plain text").unwrap();
            }
            image_input::refused_files(&session, "what is this?", &[path.clone(), invalid], code)
                .await;
        }
        assert!(requests.lock().unwrap().iter().all(|messages| {
            let messages = messages.to_string();
            !messages.contains("what is this?") && !messages.contains(PNG)
        }));
        let image = kar::InputImage::from_bytes(&STANDARD.decode(PNG).unwrap()).unwrap();
        let turn = session
            .prompt_images("KAR_GROK_IMAGE", vec![image])
            .await
            .unwrap()
            .report()
            .await
            .unwrap();
        assert_eq!(turn.outcome, kar::Outcome::Completed, "{turn:?}");
        assert!(turn.text.contains("GROK_IMAGE_RECEIVED"), "{turn:?}");
        let before_second = {
            let calls = requests.lock().unwrap();
            assert!(last_user_has_image(&calls));
            calls.len()
        };
        let image_only = session
            .prompt_image_paths("", std::slice::from_ref(&path))
            .await
            .unwrap()
            .report()
            .await
            .unwrap();
        assert_eq!(image_only.outcome, kar::Outcome::Completed);
        assert!(last_user_has_image(
            &requests.lock().unwrap()[before_second..]
        ));
        let sent = session
            .records()
            .into_iter()
            .rev()
            .find(|r| r.native["method"] == "session/prompt")
            .unwrap();
        let prompt = sent.native["params"]["prompt"].as_array().unwrap();
        assert_eq!(prompt.len(), 1);
        assert_eq!(prompt[0]["type"], "image");
        let identity = sent.prompt.as_ref().unwrap();
        assert_eq!(
            serde_json::to_value(identity.submission.as_ref().unwrap()).unwrap(),
            json!({"input":"","images":[{"path":path}]})
        );
        assert!(!serde_json::to_string(sent.as_ref()).unwrap().contains(PNG));
        let mut legacy = serde_json::to_value(sent.as_ref()).unwrap();
        legacy["prompt"]
            .as_object_mut()
            .unwrap()
            .remove("submission");
        let before_image_paths: kar::Record = serde_json::from_value(legacy).unwrap();
        assert!(before_image_paths.prompt.unwrap().submission.is_none());
        assert!(session.records().iter().any(|r| r.seq > sent.seq
            && r.native["type"] == "accepted"
            && r.native["requestId"].as_str() == identity.request_id.as_deref()));
        session.close().await.unwrap();
    })
    .await
    .expect("Grok did not finish the local image turn");
    server.abort();
}

fn local_options(temp: &tempfile::TempDir, addr: std::net::SocketAddr) -> kar::SessionOptions {
    let home = temp.path().join("home");
    let grok = home.join(".grok");
    let work = temp.path().join("workspace");
    std::fs::create_dir_all(&grok).unwrap();
    std::fs::create_dir(&work).unwrap();
    std::fs::write(
        grok.join("config.toml"),
        format!(
            r#"[cli]
auto_update = false
use_leader = false
[features]
remote_fetch = false
telemetry = false
[telemetry]
trace_upload = false
[models]
default = "fixture"
[model.fixture]
model = "fixture"
base_url = "http://{addr}/v1"
api_key = "loopback-fixture"
api_backend = "chat_completions"
context_window = 128000
max_completion_tokens = 1000
"#
        ),
    )
    .unwrap();
    let mut opts = kar::SessionOptions::new(work);
    for key in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        opts.env.insert(key.into(), home.clone().into());
    }
    opts.env.insert("GROK_HOME".into(), grok.into());
    opts.env
        .insert("XAI_API_KEY".into(), "loopback-fixture".into());
    opts.env
        .insert("GROK_DISABLE_AUTOUPDATER".into(), "1".into());
    opts
}

#[tokio::test]
#[ignore = "requires KAR_GROK_BIN; exercises real Grok interjection against a loopback model"]
async fn native_interjection_reaches_the_next_model_step_and_both_clients() {
    use axum::{Json, Router, extract::State, response::Response, routing::post};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use kar::{
        SteerStatus,
        chat::{ChatReplica, ChatStore, HttpChatFeed, Item, Role, TurnStatus},
    };
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;
    #[derive(Default)]
    struct Model {
        entered: Notify,
        release: Notify,
        calls: Mutex<Vec<Value>>,
    }
    async fn respond(State(model): State<Arc<Model>>, Json(body): Json<Value>) -> Response {
        let messages = &body["messages"];
        let main = body["tools"].is_array() && messages.to_string().contains("GROK_ORIGINAL");
        let first = main && {
            let mut calls = model.calls.lock().unwrap();
            calls.push(messages.clone());
            calls.len() == 1
        };
        let (message, stop) = if first {
            model.entered.notify_one();
            model.release.notified().await;
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == "run_terminal_command")
            );
            (
                json!({"role":"assistant","tool_calls":[{"type":"function","id":"grok-local-command",
                "function":{"name":"run_terminal_command","arguments":json!({"command":"echo grok-local-fixture","description":"Print a test marker"}).to_string()}}]}),
                "tool_calls",
            )
        } else {
            (
                json!({"role":"assistant","content":if main {"GROK_COMPLETE"} else {"Fixture title"}}),
                "stop",
            )
        };
        completion(&body, message, stop)
    }
    tokio::time::timeout(std::time::Duration::from_secs(45),async {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model=Arc::new(Model::default());
        let temp=tempfile::tempdir().unwrap();
        let opts=local_options(&temp,listener.local_addr().unwrap());
        let router=Router::new().fallback(post(respond)).with_state(model.clone());
        let server=tokio::spawn(async move {axum::serve(listener,router).await.unwrap()});
        let store=ChatStore::open(temp.path().join("chats")).unwrap();
        let program=PathBuf::from(std::env::var_os("KAR_GROK_BIN").unwrap());
        let chat=Arc::new(store.create("grok",Some(&program),opts).await.unwrap());
        let id=chat.info().id;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url=format!("http://{}",listener.local_addr().unwrap());
        let feed=kar::chat::event_router(store.clone(),id);
        let serving=tokio::spawn(async move {axum::serve(listener,feed).await.unwrap()});
        let mut live=HttpChatFeed::new(&url).unwrap();
        let mut offline=HttpChatFeed::new(&url).unwrap();
        let mut a=ChatReplica::from_snapshot(live.snapshot().await.unwrap());
        let mut b=ChatReplica::from_snapshot(offline.snapshot().await.unwrap());
        let owner=chat.clone();
        let mut sending=tokio::spawn(async move {owner.send(uuid::Uuid::new_v4(),"GROK_ORIGINAL").await});
        tokio::select! {
            _ = model.entered.notified() => {},
            result = &mut sending => panic!("original turn ended before model request: {result:?}"),
        }
        let image=kar::InputImage::from_bytes(&STANDARD.decode(PNG).unwrap()).unwrap();
        chat.steer_images("GROK_INTERJECTION",vec![image]).await.unwrap();
        for (feed,replica) in [(&mut live,&mut a),(&mut offline,&mut b)] {
            while !replica.turns().first().is_some_and(|t| t.steered_inputs.values().any(|s|*s==SteerStatus::Submitted)) {
                let event=feed.recv(replica.cursor()).await.unwrap();
                replica.apply(event).unwrap();
            }
        }
        drop(offline);
        while !a.turns()[0].steered_inputs.values().any(|s|*s==SteerStatus::Received) {
            a.apply(live.recv(a.cursor()).await.unwrap()).unwrap();
        }
        model.release.notify_one();
        let turn=sending.await.unwrap().unwrap();
        assert_eq!(turn.status,TurnStatus::Completed,"{turn:?}");
        let calls=model.calls.lock().unwrap().clone();
        let image_url=format!("data:image/png;base64,{PNG}");
        assert!(calls.iter().skip(1).any(|messages| {
            messages.as_array().unwrap().iter().any(|m|m["role"]=="user" && m["content"].to_string().contains("GROK_INTERJECTION") && m["content"].to_string().contains(&image_url))
        }),"interjection content was missing in {} model calls",calls.len());
        assert!(turn.items.iter().any(|i|matches!(&i.item,Item::Tool{..})));
        assert_eq!(turn.items.iter().filter(|i|matches!(&i.item,Item::Message{role:Role::User,text,..} if text=="GROK_INTERJECTION")).count(),1);
        assert!(turn.items.iter().any(|i|matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text.contains("GROK_COMPLETE"))));
        Arc::try_unwrap(chat).ok().unwrap().close().await.unwrap();
        let tip=store.feed_snapshot(id).unwrap();
        assert_eq!(tip.history.turns.len(),1);
        let mut resumed=HttpChatFeed::new(&url).unwrap();
        for (feed,replica) in [(&mut live,&mut a),(&mut resumed,&mut b)] {
            while replica.cursor()!=tip.cursor {
                let event=feed.recv(replica.cursor()).await.unwrap();
                assert!(replica.apply(event.clone()).unwrap());
                assert!(!replica.apply(event).unwrap());
            }
            assert_eq!(replica.turns(),tip.history.turns);
        }
        serving.abort();
        server.abort();
    }).await.expect("timed out during native Grok interjection");
}

fn completion(body: &Value, message: Value, stop: &str) -> axum::response::Response {
    use axum::{Json, response::IntoResponse};
    if body["stream"] == true {
        let mut delta = message;
        if let Some(call) = delta.pointer_mut("/tool_calls/0") {
            call["index"] = 0.into();
        }
        let choices = [
            json!({"delta":delta,"finish_reason":null,"index":0}),
            json!({"delta":{},"finish_reason":stop,"index":0}),
        ];
        let mut out = String::new();
        for choice in choices {
            let data = json!({"id":"grok-turn","object":"chat.completion.chunk","model":"fixture","created":1,"choices":[choice]});
            out.push_str(&format!("data: {data}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        ([("content-type", "text/event-stream")], out).into_response()
    } else {
        Json(json!({"id":"grok-turn","object":"chat.completion","model":"fixture","created":1,"choices":[{"index":0,"message":message,"finish_reason":stop}]})).into_response()
    }
}

#[cfg(unix)]
async fn terminal_cleanup(enabled: bool) {
    use axum::{Json, Router, routing::post};
    use kar::{
        PermissionDecision, ProcessCleanupResult,
        chat::{ChatReplica, ChatStore, HttpChatFeed, TurnStatus, event_router},
    };
    use std::{collections::HashSet, time::Duration};
    async fn model(Json(body): Json<Value>) -> axum::response::Response {
        let should_run = body["tools"].is_array()
            && body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .is_some_and(|m| {
                    m["role"] == "user" && m["content"].to_string().contains("RUN_LOCAL_TERMINAL")
                });
        if should_run {
            let command = r#"printf '%s' "${KAR_GROK_BIN+present}" > kar-env; printf started > kar-started; sleep 2; printf finished > kar-finished"#;
            let args = json!({"command":command, "description":"Write fixture markers in the temporary workspace."});
            let message = json!({"role":"assistant","tool_calls":[{"type":"function","id":"terminal-marker","function":{"name":"run_terminal_command","arguments":args.to_string()}}]});
            completion(&body, message, "tool_calls")
        } else {
            completion(
                &body,
                json!({"role":"assistant","content":"fixture reply"}),
                "stop",
            )
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut options = local_options(&tmp, listener.local_addr().unwrap());
    let server = tokio::spawn(async {
        axum::serve(listener, Router::new().fallback(post(model)))
            .await
            .unwrap()
    });
    if enabled {
        options.remove_env("KAR_GROK_BIN").unwrap();
    }
    options.settings.end_turn_processes_on_abort = enabled;
    options.approvals = kar::ApprovalMode::Host;
    let work = options.cwd.clone();
    let program = PathBuf::from(std::env::var_os("KAR_GROK_BIN").unwrap());
    let store = ChatStore::open(tmp.path().join("chats")).unwrap();
    let chat = store.create("grok", Some(&program), options).await.unwrap();
    let id = chat.info().id;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = event_router(store.clone(), id);
    let serving = tokio::spawn(async { axum::serve(listener, router).await.unwrap() });
    let mut live = HttpChatFeed::new(&url).unwrap();
    let mut view = ChatReplica::from_snapshot(live.snapshot().await.unwrap());
    let mut offline =
        ChatReplica::from_snapshot(HttpChatFeed::new(&url).unwrap().snapshot().await.unwrap());
    let turn = uuid::Uuid::new_v4();
    let abort = async {
        let mut answered = HashSet::new();
        while !work.join("kar-started").exists() {
            tokio::select! {
                event = live.recv(view.cursor()) => {
                    view.apply(event.unwrap()).unwrap();
                    for req in view.pending_requests() {
                        if answered.insert((req.cursor.stream_id,req.cursor.after_seq)) {
                            chat.decide(req, PermissionDecision::AllowOnce).await.unwrap();
                        }
                    }
                    assert!(view.turns().last().is_none_or(|t|t.status == TurnStatus::Running),"{:?}",view.turns());
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
        chat.abort().await.unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(chat.send(turn, "RUN_LOCAL_TERMINAL"), abort)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().status, TurnStatus::Aborted);
    let next = chat
        .send(uuid::Uuid::new_v4(), "NEXT_PROMPT")
        .await
        .unwrap();
    assert_eq!(next.status, TurnStatus::Completed);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(work.join("kar-finished").is_file(), !enabled);
    assert_eq!(
        std::fs::read_to_string(work.join("kar-env")).unwrap(),
        if enabled { "" } else { "present" }
    );
    chat.close().await.unwrap();
    let mut replay = HttpChatFeed::new(&url).unwrap();
    let tip = replay.snapshot().await.unwrap().cursor;
    for (feed, client) in [(&mut live, &mut view), (&mut replay, &mut offline)] {
        while client.cursor() != tip {
            let event = feed.recv(client.cursor()).await.unwrap();
            assert!(client.apply(event.clone()).unwrap());
            assert!(!client.apply(event).unwrap());
        }
        let results = &client.turns()[0].process_cleanups;
        if enabled {
            assert_eq!(results.len(), 1);
            assert_eq!(
                results[0].result,
                ProcessCleanupResult::Completed {
                    signalled: 1,
                    foreign: 0
                }
            );
        } else {
            assert!(results.is_empty());
        }
        assert!(client.turns()[1].process_cleanups.is_empty());
    }
    serving.abort();
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "real Grok command, native abort and two HTTP chat clients"]
async fn abort_ends_the_hosted_terminal() {
    terminal_cleanup(true).await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "real Grok command continuing after its default native abort"]
async fn default_abort_leaves_the_terminal_running() {
    terminal_cleanup(false).await;
}

#[tokio::test]
#[ignore = "requires KAR_GROK_BIN; a loopback model calls a real Rust host tool"]
async fn host_tool_is_ready_on_the_first_turn() {
    use axum::{Json, Router, extract::State, response::Response, routing::post};
    use kar::host_tools::HostTool;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    const PROMPT: &str = "KAR_HOST_TOOL_CHECK";
    const RECEIPT: &str = "receipt-from-rust-handler";
    async fn respond(State(seen): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> Response {
        let main = body["tools"].is_array() && body["messages"].to_string().contains(PROMPT);
        let previous = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool");
        if main && !previous {
            let args =
                json!({"tool_name":"fixture__rust_echo","tool_input":{"label":"from-model"}});
            return completion(
                &body,
                json!({"role":"assistant","tool_calls":[{"type":"function","id":"host-1",
                "function":{"name":"use_tool","arguments":args.to_string()}}]}),
                "tool_calls",
            );
        }
        if main
            && body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool" && m["content"].to_string().contains(RECEIPT))
        {
            seen.fetch_add(1, Ordering::Relaxed);
        }
        completion(
            &body,
            json!({"role":"assistant","content":"Fixture complete"}),
            "stop",
        )
    }
    let model = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut tool = HostTool::new(
        "rust_echo",
        json!({"type":"object","properties":{"label":{"type":"string"}},"required":["label"]})
            .as_object()
            .unwrap()
            .clone(),
        move |args, _| {
            let observed = observed.clone();
            async move {
                assert_eq!(args["label"], "from-model");
                observed.fetch_add(1, Ordering::Relaxed);
                Ok(RECEIPT.into())
            }
        },
    );
    tool.description = Some("Return a fixture receipt".into());
    tool.annotations = Some(
        json!({"readOnlyHint":true,"destructiveHint":false})
            .as_object()
            .unwrap()
            .clone(),
    );
    let host = kar::serve_mcp_tools("fixture", vec![tool], None)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut opts = local_options(&temp, listener.local_addr().unwrap());
    opts.approvals = kar::ApprovalMode::Host;
    opts.mcp_servers.extend([host.entry()]);
    let router = Router::new()
        .fallback(post(respond))
        .with_state(model.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut rt = kar::runtime("grok").unwrap();
    rt.program = std::env::var_os("KAR_GROK_BIN").unwrap().into();
    tokio::time::timeout(std::time::Duration::from_secs(40), async {
        let session = rt.open(opts).await.unwrap();
        let mut records = session.subscribe(None).unwrap();
        let turn = session.prompt(PROMPT).await.unwrap().report();
        tokio::pin!(turn);
        let report = loop {
            tokio::select! {
                r = &mut turn => break r.unwrap(),
                record = records.next() => {
                    let record = record.unwrap();
                    if record.native["method"] == "session/request_permission" {
                        let req = session.snapshot().pending_requests.into_iter().find(|r|r.id==record.native["id"]).unwrap();
                        session.decide(&req,kar::PermissionDecision::AllowOnce).await.unwrap();
                    }
                }
            }
        };
        assert_eq!(report.outcome, kar::Outcome::Completed, "{report:?}");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "first turn did not invoke the host handler"
        );
        assert_eq!(
            model.load(Ordering::Relaxed),
            1,
            "handler output did not reach the model"
        );
        let kar::McpServer::HostedHttp { headers, .. } = &host.server else {
            panic!()
        };
        let secret = headers["Authorization"].strip_prefix("Bearer ").unwrap();
        assert!(
            !serde_json::to_string(&session.records().iter().map(|r| &**r).collect::<Vec<_>>())
                .unwrap()
                .contains(secret)
        );
        session.close().await.unwrap();
    })
    .await
    .unwrap();
    host.close().await;
    server.abort();
}

#[cfg(target_os = "macos")]
#[path = "support/isolated_placement.rs"]
mod isolated_placement;

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires a real Grok ACP CLI, isolated home and loopback model"]
async fn native_process_placement_keeps_two_workspaces_independent() {
    use axum::{Json, Router, routing::post};
    async fn model(Json(body): Json<Value>) -> axum::response::Response {
        completion(
            &body,
            json!({"role":"assistant","content":"complete"}),
            "stop",
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/v1/chat/completions", post(model));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let opts = local_options(&root, addr);
    let mut rt = kar::runtime("grok").unwrap();
    rt.program = std::env::var_os("KAR_GROK_BIN")
        .expect("set KAR_GROK_BIN")
        .into();
    isolated_placement::verify(rt, opts).await;
    server.abort();
}
