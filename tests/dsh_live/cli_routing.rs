use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
#[ignore = "requires official DSH Web and Kimi ACP; two isolated loopback model endpoints"]
async fn cli_routes_to_the_selected_native_runtime() {
    let tb = testbed(Protocol::Dsh).await;
    let kimi = std::env::var_os("KAR_KIMI_ACP_BIN").unwrap();
    let profile = tb.dir.path().join("kimi-profile");
    std::fs::create_dir(&profile).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    std::fs::write(
        profile.join("config.toml"),
        format!(
            r#"
default_model = "fixture"
auto_session_title = false
[providers.local]
type = "openai"
base_url = "http://{addr}/v1"
api_key = "fixture-key"
[models.fixture]
provider = "local"
model = "fixture"
max_context_size = 65536
"#
        ),
    )
    .unwrap();
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let router = Router::new().route("/v1/chat/completions", axum::routing::post(
        |State(received): State<Arc<Mutex<Vec<Value>>>>, Json(body): Json<Value>| async move {
            received.lock().unwrap().push(body);
            let chunks = [(json!({"role":"assistant","content":"Kimi route confirmed"}),Value::Null),(json!({}),json!("stop"))];
            let mut sse = String::new();
            for (delta, finish) in chunks {
                let chunk = json!({"id":"route","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
                sse.push_str(&format!("data: {chunk}\n\n"));
            }
            sse.push_str("data: [DONE]\n\n");
            ([("content-type","text/event-stream")],sse)
        }
    )).with_state(received.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["mcp", "--runtimes", "kimi,dsh", "--cwd"])
        .arg(&tb.opts.cwd)
        .envs(&tb.opts.env)
        .env("KAR_DSH_BIN", &tb.runtime.program)
        .env("KAR_KIMI_BIN", kimi)
        .env("KIMI_CODE_HOME", &profile)
        .env("KIMI_CODE_MODEL_CATALOG_REFRESH_ON_START", "0")
        .env("KIMI_DISABLE_TELEMETRY", "1")
        .env_remove("KAR_AGENT_DEPTH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut call = async |id, method: &str, params: Value| {
        let packet = json!({"id":id,"jsonrpc":"2.0","method":method,"params":params});
        stdin
            .write_all(format!("{packet}\n").as_bytes())
            .await
            .unwrap();
        let line = tokio::time::timeout(Duration::from_secs(45), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id);
        assert!(
            reply.get("error").is_none() && reply["result"]["isError"] != true,
            "{reply}"
        );
        reply["result"].clone()
    };
    call(1, "initialize", json!({"protocolVersion":"2025-11-25"})).await;
    let offered = call(2, "tools/call", json!({"name":"runtimes","arguments":{}})).await;
    let rows = offered["structuredContent"]["runtimes"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["installation"]["kind"], "available", "{row}");
    }
    for (i, (runtime, task, model, expected)) in [
        (
            "kimi",
            "KAR_ROUTE_ONLY_KIMI",
            "fixture",
            "Kimi route confirmed",
        ),
        (
            "dsh",
            "DSH_PROMPT_ROUTE_ONLY_DSH",
            "deepseek-official/deepseek-flash",
            "scoped prompt reply",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let result = call(
            3 + i,
            "tools/call",
            json!({"name":"run","arguments":{"runtime":runtime,"task":task,"model":model}}),
        )
        .await;
        assert_eq!(result["structuredContent"]["outcome"], "completed");
        assert_eq!(result["structuredContent"]["text"], expected);
    }
    let agents = call(5, "tools/call", json!({"name":"list","arguments":{}})).await;
    let agents = agents["structuredContent"]["agents"].as_array().unwrap();
    let dsh = agents.iter().find(|a| a["runtime"] == "dsh").unwrap()["session_id"]
        .as_str()
        .unwrap();
    assert!(
        tb.data
            .prompt_sessions
            .lock()
            .unwrap()
            .iter()
            .any(|(id, _)| id == dsh)
    );
    for (seen, own, other) in [
        (
            &*received.lock().unwrap(),
            "KAR_ROUTE_ONLY_KIMI",
            "DSH_PROMPT_ROUTE_ONLY_DSH",
        ),
        (
            &*tb.data.requests.lock().unwrap(),
            "DSH_PROMPT_ROUTE_ONLY_DSH",
            "KAR_ROUTE_ONLY_KIMI",
        ),
    ] {
        assert!(seen.iter().any(|b| b["messages"].to_string().contains(own)));
        assert!(
            seen.iter()
                .all(|b| !b["messages"].to_string().contains(other))
        );
    }
    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    server.abort();
}
