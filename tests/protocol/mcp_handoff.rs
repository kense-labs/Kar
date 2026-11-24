//! Native --mcp-config lifetime across Claude reconnects and Qoder tool calls.
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use kar::{McpServer, Outcome, Protocol, Runtime, Session, SessionOptions};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::BufRead,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

fn fingerprint(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn answer(req: &Value, token: &str) -> Option<Value> {
    let result = match req["method"].as_str()? {
        "initialize" => json!({"protocolVersion":req["params"]["protocolVersion"],
            "capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"1"}}),
        "tools/list" => {
            json!({"tools":[{"name":"echo","description":"Echo with the server fingerprint",
            "inputSchema":{"type":"object","properties":{"tag":{"type":"string"}},"required":["tag"]}}]})
        }
        "tools/call" => {
            assert_eq!(req["params"]["name"], "echo");
            let tag = req["params"]["arguments"]["tag"].as_str().unwrap();
            json!({"content":[{"type":"text","text":format!("{tag}/{}", fingerprint(token))}]})
        }
        _ if req.get("id").is_none() => return None,
        _ => {
            return Some(
                json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32601,"message":"unknown method"}}),
            );
        }
    };
    Some(json!({"jsonrpc":"2.0","id":req["id"],"result":result}))
}

pub(super) fn stdio(once: bool) {
    let token = std::env::var("KAR_TEST_ECHO_TOKEN").unwrap();
    for line in std::io::stdin().lock().lines() {
        let req: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if let Some(result) = answer(&req, &token) {
            super::emit(result);
        }
        if once && req["method"] == "tools/call" {
            break;
        }
    }
}

#[derive(Clone)]
struct Peer {
    tokens: [String; 2],
    retire_after_call: bool,
    retired: Arc<Mutex<HashSet<String>>>,
    expired_requests: Arc<std::sync::atomic::AtomicUsize>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn http(
    State(state): State<Peer>,
    Path(slot): Path<usize>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    let Some(token) = state.tokens.get(slot) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let session = headers.get("mcp-session-id").and_then(|v| v.to_str().ok());
    if session.is_some_and(|s| state.retired.lock().unwrap().contains(s)) {
        state
            .expired_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(reply) = answer(&req, token) else {
        return StatusCode::ACCEPTED.into_response();
    };
    let mut response = Json(reply).into_response();
    if req["method"] == "initialize" {
        response.headers_mut().insert(
            "mcp-session-id",
            Uuid::new_v4().to_string().parse().unwrap(),
        );
    } else if state.retire_after_call && req["method"] == "tools/call" {
        state
            .retired
            .lock()
            .unwrap()
            .insert(session.unwrap().to_owned());
    }
    response
}

async fn model(State(state): State<Peer>, Json(req): Json<Value>) -> Response {
    let mut tool = None;
    'messages: for msg in req["messages"].as_array().into_iter().flatten().rev() {
        if msg["role"] != "user" {
            continue;
        }
        for part in msg["content"].as_array().into_iter().flatten().rev() {
            if part["type"] == "tool_result" {
                break 'messages;
            }
            if let Some(instruction) = part["text"]
                .as_str()
                .and_then(|t| t.trim().strip_prefix("invoke "))
                && let Some((name, tag)) = instruction.split_once(' ')
                && ["local", "remote", "other"].contains(&name)
            {
                tool = Some((name.to_owned(), tag.to_owned()));
                break 'messages;
            }
        }
    }
    state.requests.lock().unwrap().push(req);
    let id = Uuid::new_v4().simple().to_string();
    let (block, delta, reason) = match tool {
        Some((name, tag)) => (
            json!({"type":"tool_use","id":format!("toolu_{id}"),"name":format!("mcp__{name}__echo"),"input":{}}),
            json!({"type":"input_json_delta","partial_json":json!({"tag":tag}).to_string()}),
            "tool_use",
        ),
        None => (
            json!({"type":"text","text":""}),
            json!({"type":"text_delta","text":"done"}),
            "end_turn",
        ),
    };
    let frames = [
        json!({"type":"message_start","message":{"id":format!("msg_{id}"),"type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_delta","index":0,"delta":delta}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ];
    let data = frames
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect::<String>();
    ([("content-type", "text/event-stream")], data).into_response()
}

async fn invoke(session: &Session, peer: &Peer, name: &str, token: &str) {
    let tag = Uuid::new_v4();
    let start = peer.requests.lock().unwrap().len();
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        session
            .prompt(format!("invoke {name} {tag}"))
            .await?
            .wait()
            .await
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, Outcome::Completed);
    let value = format!("{tag}/{}", fingerprint(token));
    let requests = peer.requests.lock().unwrap();
    let found = requests[start..].iter().any(|req| {
        req["messages"].as_array().is_some_and(|messages| {
            messages
                .last()
                .and_then(|m| m["content"].as_array())
                .is_some_and(|parts| {
                    parts.iter().any(|p| {
                        p["type"] == "tool_result" && p["content"].to_string().contains(&value)
                    })
                })
        })
    });
    assert!(
        found,
        "missing authenticated {name} result among {} model requests",
        requests.len() - start
    );
}

#[cfg(unix)]
fn check_args(session: &Session, tokens: &[String]) {
    let ps = std::process::Command::new("ps")
        .args(["-axww", "-o", "command="])
        .output()
        .unwrap();
    assert!(ps.status.success());
    let text = String::from_utf8(ps.stdout).unwrap();
    assert!(tokens.iter().all(|s| !text.contains(s)));
    let line = text
        .lines()
        .find(|l| l.contains(session.id()) && l.contains("--mcp-config "))
        .unwrap();
    let path = line
        .split_once("--mcp-config ")
        .unwrap()
        .1
        .split_whitespace()
        .next()
        .unwrap();
    assert!(!std::path::Path::new(path).parent().unwrap().exists());
}

pub(super) async fn claude() {
    let runtime = Runtime::new(
        "claude",
        std::env::var_os("KAR_CLAUDE_BIN").unwrap(),
        &[
            "--strict-mcp-config",
            "--allowedTools",
            "mcp__local__echo,mcp__remote__echo,mcp__other__echo",
        ],
        Protocol::Claude,
    );
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let tokens: [String; 3] = std::array::from_fn(|_| format!("Bearer kar-{}", Uuid::new_v4()));
    let state = Peer {
        tokens: [tokens[0].clone(), tokens[1].clone()],
        retire_after_call: true,
        retired: Default::default(),
        expired_requests: Default::default(),
        requests: Default::default(),
    };
    let router = Router::new()
        .route("/v1/messages", post(model))
        .route("/mcp/{slot}", post(http))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut options = SessionOptions::new(dir.path());
    for (name, value) in [
        ("HOME", dir.path().to_str().unwrap()),
        ("CLAUDE_CONFIG_DIR", dir.path().to_str().unwrap()),
        ("ANTHROPIC_BASE_URL", &url),
        ("ANTHROPIC_API_KEY", "fixture"),
        ("ANTHROPIC_AUTH_TOKEN", ""),
        ("ENABLE_TOOL_SEARCH", "false"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ] {
        options.env.insert(name.into(), value.into());
    }
    let mut sibling = options.clone();
    let remote = |slot: usize| McpServer::Http {
        url: format!("{url}/mcp/{slot}"),
        headers: [("Authorization".into(), tokens[slot].clone())].into(),
    };
    options.mcp_servers.insert(
        "local".into(),
        McpServer::Stdio {
            command: std::env::current_exe().unwrap().to_str().unwrap().into(),
            args: vec!["stdio-echo-once".into()],
            env: [("KAR_TEST_ECHO_TOKEN".into(), tokens[2].clone())].into(),
        },
    );
    options.mcp_servers.insert("remote".into(), remote(0));
    sibling.mcp_servers.insert("other".into(), remote(1));
    let (a, b) = tokio::try_join!(runtime.open(options), runtime.open(sibling)).unwrap();
    #[cfg(unix)]
    for session in [&a, &b] {
        check_args(session, &tokens);
    }
    for (session, name, slot) in [
        (&a, "local", 2),
        (&a, "remote", 0),
        (&b, "other", 1),
        (&a, "local", 2),
        (&a, "remote", 0),
    ] {
        invoke(session, &state, name, &tokens[slot]).await;
    }
    assert_eq!(state.retired.lock().unwrap().len(), 3);
    assert!(
        state
            .expired_requests
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
    for session in [a, b] {
        session.close().await.unwrap();
        for r in session.records() {
            assert!(
                tokens
                    .iter()
                    .all(|t| !serde_json::to_string(&*r).unwrap().contains(t)),
                "credential in Kar records"
            );
        }
    }
    for req in state.requests.lock().unwrap().iter() {
        assert!(
            tokens.iter().all(|t| !req.to_string().contains(t)),
            "credential in model traffic"
        );
    }
    super::private_inputs::verify_private_inputs(dir.path(), &tokens);
    server.abort();
    println!(
        "Claude MCP startup, stdio restart, HTTP reinitialization and sibling isolation passed"
    );
}

pub(super) async fn qoder() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        json!({"security":{"auth":{"useExternal":true}}}).to_string(),
    )
    .unwrap();
    let replay = dir.path().join("replies.jsonl");
    let calls = [
        ("local", "stdio-first"),
        ("remote", "http-first"),
        ("local", "stdio-second"),
        ("remote", "http-second"),
    ];
    let rows = calls.iter().map(|(name,tag)| json!({"functionCall":{"name":format!("mcp__{name}__echo"),"args":{"tag":tag}}}))
        .chain([json!({"text":"done"})]).map(|part| json!({"method":"generateContentStream","response":[{"candidates":[{"content":{"parts":[part]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2}}]}).to_string())
        .collect::<Vec<_>>().join("\n");
    std::fs::write(&replay, format!("{rows}\n")).unwrap();
    let work = dir.path().join("project");
    std::fs::create_dir(&work).unwrap();
    let runtime = Runtime::new(
        "qoder",
        std::env::var_os("KAR_QODER_WORKER_BIN").unwrap(),
        &[
            "--settings",
            settings.to_str().unwrap(),
            "--fake-responses",
            replay.to_str().unwrap(),
        ],
        Protocol::Qoder,
    );
    let tokens: [String; 2] = std::array::from_fn(|_| format!("Bearer kar-{}", Uuid::new_v4()));
    let state = Peer {
        tokens: tokens.clone(),
        retire_after_call: false,
        retired: Default::default(),
        expired_requests: Default::default(),
        requests: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp/0", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/mcp/{slot}", post(http))
        .with_state(state);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut options = SessionOptions::new(&work);
    for key in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME"] {
        options.env.insert(key.into(), dir.path().into());
    }
    for key in [
        "QODER_PERSONAL_ACCESS_TOKEN",
        "QODER_API_KEY",
        "QODER_SERVICE_ACCOUNT_KEY",
        "QODER_JOB_TOKEN",
        "QODER_CONFIG_DIR",
        "QODER_CLI_HOME",
        "QODER_AGENT_SDK_DAEMON",
    ] {
        options.remove_env(key).unwrap();
    }
    options
        .env
        .insert("QODER_FAKE_STREAM_DELAY_MS".into(), "1".into());
    options.approvals = kar::ApprovalMode::Host;
    options.mcp_servers.insert(
        "local".into(),
        McpServer::Stdio {
            command: std::env::current_exe().unwrap().to_str().unwrap().into(),
            args: vec!["stdio-echo".into()],
            env: [("KAR_TEST_ECHO_TOKEN".into(), tokens[1].clone())].into(),
        },
    );
    options.mcp_servers.insert(
        "remote".into(),
        McpServer::Http {
            url,
            headers: [("Authorization".into(), tokens[0].clone())].into(),
        },
    );
    let session = runtime.open(options).await.unwrap();
    #[cfg(unix)]
    check_args(&session, &tokens);
    let mut records = session.subscribe(None).unwrap();
    let turn = session.prompt("call echo").await.unwrap();
    let answer = async {
        while let Some(_record) = records.next().await {
            for request in session.snapshot().pending_requests {
                session
                    .decide(&request, kar::PermissionDecision::AllowOnce)
                    .await
                    .unwrap();
            }
        }
    };
    let report = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::select! {
            r = turn.report() => r.unwrap(),
            _ = answer => panic!("session closed before the turn ended"),
        }
    })
    .await
    .unwrap();
    assert_eq!(report.outcome, Outcome::Completed);
    session.close().await.unwrap();
    let frames = session
        .records()
        .iter()
        .map(|r| serde_json::to_string(&**r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for (name, tag) in calls {
        assert!(
            frames.contains(&format!(
                "{tag}/{}",
                fingerprint(&tokens[usize::from(name == "local")])
            )),
            "missing {tag} credential receipt"
        );
    }
    assert!(
        tokens.iter().all(|t| !frames.contains(t)),
        "credential in Kar records"
    );
    super::private_inputs::verify_private_inputs(dir.path(), &tokens);
    server.abort();
    println!("Qoder MCP startup, repeated stdio and HTTP calls and credential isolation passed");
}
