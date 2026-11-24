//! Pi RPC, a local scripted model, and two real MCP transports.
#[cfg(unix)]
#[path = "support/mcp_process.rs"]
mod mcp_process;
#[path = "support/sse.rs"]
mod sse;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use kar::{
    McpServer, SessionOptions,
    chat::{ChatReplica, ChatSession, ChatStore, ChatTurn, Item, TurnStatus, event_router},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use uuid::Uuid;

#[derive(Clone, Default)]
struct Peer {
    requests: Arc<Mutex<Vec<Value>>>,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
async fn model(State(peer): State<Peer>, Json(body): Json<Value>) -> Response {
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let call = if last["role"] == "user" {
        match last["content"]
            .as_str()
            .or_else(|| last["content"][0]["text"].as_str())
        {
            Some("call stdio") => Some(("mcp__local_agent__list", "{}", "stdio")),
            Some("call http") => {
                Some(("mcp__remote__echo", r#"{"text":"literal $value"}"#, "http"))
            }
            _ => None,
        }
    } else {
        None
    };
    peer.requests.lock().unwrap().push(body);
    let (delta, stop) = match call {
        Some((name, args, id)) => (
            json!({"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args}}]}),
            "tool_calls",
        ),
        None => (json!({"role":"assistant","content":"finished"}), "stop"),
    };
    let mut wire = String::new();
    for (delta, finish) in [(delta, Value::Null), (json!({}), json!(stop))] {
        let frame = json!({"id":"reply","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        wire.push_str(&format!("data: {frame}\n\n"));
    }
    wire.push_str("data: [DONE]\n\n");
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        wire,
    )
        .into_response()
}
async fn mcp(State(peer): State<Peer>, Json(req): Json<Value>) -> Response {
    let data = match req["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": req["params"]["protocolVersion"],
            "serverInfo": {"name":"loopback","version":"1"},
            "capabilities": {"tools": {}}
        }),
        Some("notifications/initialized") => return StatusCode::ACCEPTED.into_response(),
        Some("tools/list") => json!({"tools": [{
            "name":"echo", "description":"Return the input",
            "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
        }]}),
        Some("tools/call") => {
            assert_eq!(req["params"]["name"], "echo");
            assert_eq!(req["params"]["arguments"]["text"], "literal $value");
            peer.calls.fetch_add(1, Ordering::SeqCst);
            peer.entered.notify_one();
            peer.release.notified().await;
            json!({"content":[{"type":"text","text":"MCP returned literal $value"}]})
        }
        _ => return Json(json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32601,"message":"unknown method"}})).into_response(),
    };
    Json(json!({"jsonrpc":"2.0","id":req["id"],"result":data})).into_response()
}
fn names(peer: &Peer) -> BTreeSet<String> {
    let calls = peer.requests.lock().unwrap();
    calls.last().unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .filter(|n| n.starts_with("mcp__"))
        .map(String::from)
        .collect()
}
fn stdio(cwd: &str) -> McpServer {
    McpServer::Stdio {
        command: env!("CARGO_BIN_EXE_kar").into(),
        args: vec!["mcp".into(), "--cwd".into(), cwd.into()],
        env: Default::default(),
    }
}

#[cfg(unix)]
async fn check_stdio_environment(
    store: &ChatStore,
    chat: Uuid,
    opts: &SessionOptions,
    marker: &str,
    receipt: &Path,
) {
    assert!(
        std::env::var_os("HOME").is_some(),
        "this check removes an inherited HOME"
    );
    let McpServer::Stdio { command, args, .. } = stdio(marker) else {
        unreachable!()
    };
    for remove in [false, true] {
        let mut shell_args: Vec<String> = [
            "-c",
            r#"printf '%s' "${HOME-unset}" > "$1"; shift; exec "$@""#,
            "env-check",
        ]
        .map(str::to_owned)
        .into();
        shell_args.push(receipt.to_str().unwrap().into());
        shell_args.push(command.clone());
        shell_args.extend(args.clone());
        let server = McpServer::Stdio {
            command: "/bin/sh".into(),
            args: shell_args,
            env: Default::default(),
        };
        let resumed = store
            .resume_with(chat, |launch| {
                launch.env.clone_from(&opts.env);
                if remove {
                    launch.remove_env("HOME").unwrap();
                }
                launch.mcp_servers.insert("local-agent".into(), server);
            })
            .await
            .unwrap();
        let turn = resumed.send(Uuid::new_v4(), "call stdio").await.unwrap();
        assert_eq!(turn.status, TurnStatus::Completed);
        assert!(
            serde_json::to_string(&turn.items)
                .unwrap()
                .contains("unread")
        );
        let expected = if remove {
            "unset"
        } else {
            opts.env[std::ffi::OsStr::new("HOME")].to_str().unwrap()
        };
        assert_eq!(fs::read_to_string(receipt).unwrap(), expected);
        fs::remove_file(receipt).unwrap();
        resumed.close().await.unwrap();
    }
}

async fn through_tool_start(client: &mut sse::SseClient, replica: &mut ChatReplica) {
    loop {
        assert!(replica.apply(client.next().await).unwrap());
        if replica.turns().last().is_some_and(|turn| turn.status==TurnStatus::Running && turn.items.iter().any(|item| matches!(
            &item.item, Item::Tool {activity,..} if activity.name.as_deref()==Some("mcp__remote__echo") && !activity.finished
        ))) {return}
    }
}
async fn observe_http_call(store: &ChatStore, chat: &ChatSession, peer: &Peer) -> ChatTurn {
    let id = chat.info().id;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = event_router(store.clone(), id);
    let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut a = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let start = a.cursor().to_string();
    let mut left = sse::SseClient::open(&url, &start, None).await;
    let mut right = sse::SseClient::open(&url, &start, None).await;
    let inspect = async {
        tokio::join!(
            through_tool_start(&mut left, &mut a),
            through_tool_start(&mut right, &mut b)
        );
        tokio::time::timeout(std::time::Duration::from_secs(8), peer.entered.notified())
            .await
            .expect("Pi called the HTTP server");
        drop(right);
        peer.release.notify_one();
    };
    let (turn, ()) = tokio::join!(chat.send(Uuid::new_v4(), "call http"), inspect);
    let mut right = sse::SseClient::open(&url, &start, Some(&b.cursor().to_string())).await;
    let end = store.feed_snapshot(id).unwrap();
    for (client, replica) in [(&mut left, &mut a), (&mut right, &mut b)] {
        while replica.cursor() != end.cursor {
            assert!(replica.apply(client.next().await).unwrap());
        }
        assert_eq!(replica.turns(), end.history.turns);
    }
    http.abort();
    turn.unwrap()
}

#[tokio::test]
#[ignore = "requires npm Pi or standalone Pi with matching OAR_PI_SDK_DIR, and Node with native addon support"]
async fn native_stdio_http_isolation_and_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let work = base.join("workspace");
    let agent = base.join("agent");
    let marker = format!("workspace $HOME {}", Uuid::new_v4());
    fs::create_dir_all(work.join(&marker)).unwrap();
    fs::create_dir(&agent).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let data = Peer::default();
    let router = Router::new()
        .route("/v1/chat/completions", post(model))
        .route("/mcp", post(mcp))
        .with_state(data.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    fs::write(agent.join("models.json"),json!({"providers":{"fixture":{"baseUrl":format!("{url}/v1"),"api":"openai-completions","apiKey":"local","models":[{"id":"fixture","name":"Fixture","input":["text"],"contextWindow":64000,"maxTokens":512}]}}}).to_string()).unwrap();
    let ambient=json!({"mcpServers":{"local-agent":{"type":"http","url":format!("{url}/mcp"),"exposure":"direct"},"ambient":{"type":"http","url":format!("{url}/mcp"),"exposure":"direct"}}}).to_string();
    fs::write(agent.join("mcp.json"), &ambient).unwrap();
    fs::create_dir_all(work.join(".pi")).unwrap();
    fs::write(work.join(".pi/mcp.json"), &ambient).unwrap();
    let mut opts = SessionOptions::new(&work);
    for k in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
    ] {
        opts.env.insert(k.into(), base.join(k).into());
    }
    opts.env
        .insert("OAR_PI_AGENT_DIR".into(), agent.clone().into());
    opts.env.insert("PI_OFFLINE".into(), "1".into());
    opts.model = Some("fixture/fixture".into());
    opts.settings.pi_project_trusted = Some(true);
    let store = ChatStore::open(base.join("chats")).unwrap();
    let bin = std::env::var_os("OAR_PI_BIN").unwrap();
    let plain = opts.clone();
    opts.mcp_servers
        .insert("local-agent".into(), stdio(&marker));
    let remote = McpServer::Http {
        url: format!("{url}/mcp"),
        headers: Default::default(),
    };
    opts.mcp_servers.insert("remote".into(), remote.clone());
    let chat = store
        .create("pi", Some(Path::new(&bin)), opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    assert!(chat.capabilities().session_mcp);
    assert_eq!(
        chat.capabilities().mcp_transports,
        Some([kar::McpTransport::Stdio, kar::McpTransport::Http].as_slice())
    );
    for (prompt, answer) in [
        ("call stdio", "unread"),
        ("call http", "MCP returned literal $value"),
    ] {
        let turn = if prompt == "call http" {
            observe_http_call(&store, &chat, &data).await
        } else {
            chat.send(Uuid::new_v4(), prompt).await.unwrap()
        };
        assert_eq!(turn.status, TurnStatus::Completed, "{:?}", turn.items);
        let names = names(&data);
        assert!(
            names.contains("mcp__local_agent__list") && names.contains("mcp__remote__echo"),
            "{names:?}"
        );
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("mcp__ambient") || n == "mcp__local_agent__echo")
        );
        let requests = data.requests.lock().unwrap();
        let last = requests.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert!(
            last["role"] == "tool" && last["content"].to_string().contains(answer),
            "{last}"
        );
        assert!(serde_json::to_string(&turn.items).unwrap().contains(answer));
    }
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
    let other = store
        .create("pi", Some(Path::new(&bin)), plain)
        .await
        .unwrap();
    assert!(other.capabilities().session_mcp);
    assert!(other.capabilities().mcp_transports.is_none());
    other
        .send(Uuid::new_v4(), "concurrent plain chat")
        .await
        .unwrap();
    assert!(names(&data).is_empty());
    other.close().await.unwrap();
    #[cfg(unix)]
    assert!(mcp_process::tracked_mcp_running(&marker).await);
    chat.close().await.unwrap();
    #[cfg(unix)]
    mcp_process::assert_mcp_stopped(&marker).await;
    let store = ChatStore::open(base.join("chats")).unwrap();
    let resumed = store
        .resume_with(id, |o| {
            o.env = opts.env.clone();
            o.mcp_servers.insert("replacement".into(), remote);
        })
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    resumed
        .send(Uuid::new_v4(), "continue with replacement")
        .await
        .unwrap();
    assert_eq!(
        names(&data),
        BTreeSet::from(["mcp__replacement__echo".into()])
    );
    assert!(
        data.requests.lock().unwrap().last().unwrap()["messages"]
            .to_string()
            .contains("MCP returned literal $value")
    );
    resumed.close().await.unwrap();
    let omitted = store
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    omitted
        .send(Uuid::new_v4(), "continue without servers")
        .await
        .unwrap();
    assert!(names(&data).is_empty());
    omitted.close().await.unwrap();
    #[cfg(unix)]
    check_stdio_environment(&store, id, &opts, &marker, &base.join("stdio-home")).await;
    assert_eq!(fs::read_to_string(agent.join("mcp.json")).unwrap(), ambient);
    let mut filtered = opts.clone();
    filtered.settings.model_requests = Some(kar::ModelRequests::Latest);
    filtered.settings.disallowed_tools = vec!["mcp__remote__*".into()];
    let limited = store
        .create("pi", Some(Path::new(&bin)), filtered)
        .await
        .unwrap();
    limited
        .send(Uuid::new_v4(), "check filtered tools")
        .await
        .unwrap();
    let observed = limited
        .last_model_request()
        .await
        .unwrap()
        .expect("captured Pi request")
        .body
        .unwrap();
    assert_eq!(
        observed["tools"],
        data.requests.lock().unwrap().last().unwrap()["tools"]
    );
    let offered = names(&data);
    assert!(offered.contains("mcp__local_agent__list"));
    assert!(!offered.contains("mcp__remote__echo"));
    limited.close().await.unwrap();
    let mut malformed = opts.clone();
    malformed.mcp_servers.clear();
    malformed.mcp_servers.insert(
        "invalid".into(),
        McpServer::Http {
            url: "not-a-url".into(),
            headers: Default::default(),
        },
    );
    let count = data.requests.lock().unwrap().len();
    let err = store
        .create("pi", Some(Path::new(&bin)), malformed)
        .await
        .err()
        .expect("native rejection");
    assert!(matches!(err, kar::Error::Refused(_)), "{err}");
    assert_eq!(data.requests.lock().unwrap().len(), count);
    assert_eq!(
        fs::read_to_string(work.join(".pi/mcp.json")).unwrap(),
        ambient
    );
    // A Pi user's extension already owns this name when our session_start runs.
    fs::create_dir_all(agent.join("extensions")).unwrap();
    fs::write(agent.join("extensions/preexisting.ts"), r#"export default pi => pi.registerMcpServer("remote", {type:"http",url:"http://127.0.0.1:9",enabled:false});"#).unwrap();
    let err = store
        .create("pi", Some(Path::new(&bin)), opts.clone())
        .await
        .err()
        .expect("registration conflict");
    assert!(matches!(err, kar::Error::Refused(_)), "{err}");
    opts.env.insert("NODE_OPTIONS".into(), "--no-addons".into());
    let err = store
        .create("pi", Some(Path::new(&bin)), opts)
        .await
        .err()
        .expect("Node disabled native modules");
    assert!(matches!(err, kar::Error::Unsupported(_)), "{err}");
    assert_eq!(data.requests.lock().unwrap().len(), count);
    server.abort();
}
