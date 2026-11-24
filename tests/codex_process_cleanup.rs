#![cfg(unix)]
use axum::{Json, Router, extract::State, routing::post};
use kar::{
    McpServer, ProcessCleanupResult, SessionOptions,
    chat::{ChatReplica, ChatStore, HttpChatFeed, TurnStatus, event_router},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

struct Model {
    work: PathBuf,
    foreground: bool,
}
async fn model(
    State(model): State<Arc<Model>>,
    Json(body): Json<Value>,
) -> ([(&'static str, &'static str); 1], String) {
    let input = body["input"].as_array().unwrap();
    let last = input
        .iter()
        .rev()
        .find(|item| item["role"] == "user")
        .unwrap()
        .to_string();
    let marker = ["earlier", "current"]
        .into_iter()
        .find(|marker| last.contains(&format!("START_{marker}")));
    let mut output = json!({"type":"message","id":"msg_fixture","role":"assistant","status":"completed","content":[{"type":"output_text","text":"session still running","annotations":[]}]});
    if let Some(marker) = marker {
        let call = format!("call_{marker}");
        if !input
            .iter()
            .any(|item| item["call_id"] == call && item["type"] == "function_call_output")
        {
            // The native read is started before the prompt but intentionally does not delay it.
            tokio::time::sleep(Duration::from_millis(600)).await;
            let yield_ms = if model.foreground && marker == "current" {
                300_000
            } else {
                250
            };
            let args =
                json!({"cmd":format!("exec ./fixture {marker} ."), "yield_time_ms":yield_ms});
            let mut tool = json!({"type":"function_call","id":format!("fc_{marker}"),"call_id":call,"name":"exec_command","arguments":args.to_string(),"status":"completed"});
            if let Some(namespace) = body["tools"].as_array().unwrap().iter().find(|t| {
                t["tools"]
                    .as_array()
                    .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "exec_command"))
            }) {
                tool["namespace"] = namespace["name"].clone();
            }
            output = tool;
        } else if marker == "current" {
            std::fs::write(model.work.join("model-waiting"), "").unwrap();
            for _ in 0..800 {
                if !model.work.is_dir() || model.work.join("release").is_file() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    let response_id = Uuid::new_v4().to_string();
    let response = json!({"id":response_id,"object":"response","status":"completed","model":"gpt-5.1","output":[output],"usage":{"input_tokens":2,"output_tokens":2,"total_tokens":4}});
    let events = [
        json!({"type":"response.created","response":{"id":response_id,"status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":output}),
        json!({"type":"response.output_item.done","output_index":0,"item":output}),
        json!({"type":"response.completed","response":response}),
    ];
    let body = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ([("Content-Type", "text/event-stream")], body)
}

async fn marker(work: &Path, name: &str) -> i32 {
    for _ in 0..400 {
        if let Ok(s) = std::fs::read_to_string(work.join(name))
            && let Ok(pid) = s.parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("missing {name} process marker");
}
fn running(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}
#[tokio::test]
#[ignore = "real Codex app-server and exec_command with a loopback model and isolated MCP server"]
async fn abort_spares_earlier_commands_and_mcp_workers() {
    scenario(true, false).await;
}

#[tokio::test]
#[ignore = "real Codex interrupts a foreground tool with a separately grouped child"]
async fn abort_ends_the_foreground_tools_child() {
    scenario(true, true).await;
}

#[tokio::test]
#[ignore = "real Codex native interrupt without opt-in cleanup"]
async fn native_interrupt_preserves_the_default() {
    scenario(false, false).await;
}

async fn scenario(enabled: bool, foreground: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let home = tmp.path().join("codex");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&home).unwrap();
    let work = work.canonicalize().unwrap();
    let helper = work.join("fixture");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_process_tree.rs");
    assert!(
        std::process::Command::new("rustc")
            .arg("--edition=2024")
            .arg(source)
            .arg("-o")
            .arg(&helper)
            .status()
            .unwrap()
            .success()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Model {
        work: work.clone(),
        foreground,
    });
    let app = Router::new()
        .route("/v1/responses", post(model))
        .with_state(state);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = format!(
        "model = \"gpt-5.1\"\nmodel_provider = \"fixture\"\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n[model_providers.fixture]\nname = \"fixture\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nenv_key = \"KAR_FIXTURE_KEY\"\nwire_api = \"responses\"\n"
    );
    std::fs::write(home.join("config.toml"), config).unwrap();
    let mut opts = SessionOptions::new(&work);
    opts.env.insert("CODEX_HOME".into(), home.into_os_string());
    opts.env.insert("KAR_FIXTURE_KEY".into(), "loopback".into());
    opts.settings.end_turn_processes_on_abort = enabled;
    opts.mcp_servers.insert(
        "fixture".into(),
        McpServer::Stdio {
            command: helper.to_str().unwrap().into(),
            args: vec![
                "mcp".into(),
                work.to_str().unwrap().into(),
                env!("CARGO_BIN_EXE_kar").into(),
            ],
            env: BTreeMap::new(),
        },
    );
    let store = ChatStore::open(tmp.path().join("chats")).unwrap();
    let chat = store.create("codex", None, opts).await.unwrap();
    let id = chat.info().id;
    let mcp = marker(&work, "mcp").await;
    let earlier = chat.send(Uuid::new_v4(), "START_earlier").await.unwrap();
    assert_eq!(earlier.status, TurnStatus::Completed, "{earlier:?}");
    let older_pid = marker(&work, "earlier").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = event_router(store.clone(), id);
    let service = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut live_feed = HttpChatFeed::new(&url).unwrap();
    let mut first = ChatReplica::from_snapshot(live_feed.snapshot().await.unwrap());
    let mut second =
        ChatReplica::from_snapshot(HttpChatFeed::new(&url).unwrap().snapshot().await.unwrap());
    let turn = Uuid::new_v4();
    let send = chat.send(turn, "START_current");
    let abort = async {
        let current = marker(&work, "current").await;
        std::fs::write(work.join("fork-now"), "").unwrap();
        let child = marker(&work, "current-child").await;
        let earlier_child = marker(&work, "earlier-child").await;
        let mcp_child = marker(&work, "mcp-child").await;
        if !foreground {
            for _ in 0..400 {
                if work.join("model-waiting").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(work.join("model-waiting").exists());
        }
        chat.abort().await.unwrap();
        (current, child, earlier_child, mcp_child)
    };
    let (sent, (current, child, earlier_child, mcp_child)) =
        tokio::time::timeout(Duration::from_secs(35), async { tokio::join!(send, abort) })
            .await
            .unwrap();
    assert_eq!(sent.unwrap().status, TurnStatus::Aborted);
    std::fs::write(work.join("release"), "").unwrap();
    let cleanup = if enabled {
        let cleanup = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                first
                    .apply(live_feed.recv(first.cursor()).await.unwrap())
                    .unwrap();
                if let Some(result) = first
                    .turns()
                    .last()
                    .and_then(|t| t.process_cleanups.last().cloned())
                {
                    break result;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(cleanup.result, ProcessCleanupResult::Completed { signalled, foreign:0 } if signalled >= 1),
            "{cleanup:?}"
        );
        for _ in 0..150 {
            if !running(current) && !running(child) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !running(current) && !running(child),
            "current {current} or child {child} survived"
        );
        Some(cleanup)
    } else {
        assert!(
            running(child),
            "native control case lost the detached child"
        );
        None
    };
    for pid in [older_pid, earlier_child, mcp, mcp_child] {
        assert!(running(pid), "older process {pid} was lost");
    }
    assert_eq!(
        chat.send(Uuid::new_v4(), "NEXT_PROMPT")
            .await
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    chat.close().await.unwrap();
    let mut feed = HttpChatFeed::new(&url).unwrap();
    let tip = store.feed_snapshot(id).unwrap().cursor;
    while second.cursor() != tip {
        let event = feed.recv(second.cursor()).await.unwrap();
        assert!(second.apply(event.clone()).unwrap());
        assert!(!second.apply(event).unwrap());
    }
    assert_eq!(
        second.turns()[1].process_cleanups,
        cleanup.into_iter().collect::<Vec<_>>()
    );
    assert_eq!(
        store
            .history(id)
            .unwrap()
            .chat
            .settings
            .end_turn_processes_on_abort,
        enabled
    );
    assert!(
        second.turns()[0].process_cleanups.is_empty()
            && second.turns()[2].process_cleanups.is_empty()
    );
    server.abort();
    service.abort();
}
