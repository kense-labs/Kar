//! Exercise the authenticated Kar listener and the credential handed to a native Codex protocol peer.
#[path = "workbench_mcp/issues.rs"]
mod issues;
#[path = "workbench_mcp/projects.rs"]
mod projects;
#[path = "workbench_mcp/worktree.rs"]
mod worktree;
#[path = "workbench_mcp/preview_tools.rs"]
mod preview_tools;
#[path = "workbench_mcp/workflow.rs"]
mod workflow;
#[allow(dead_code)]
#[path = "support/codex_chat_peer.rs"]
mod peer;
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod server;
#[allow(dead_code)]
#[path="support/workbench_devices.rs"]
mod devices;
use server as workbench_server;
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use uuid::Uuid;

fn main() {
    if matches!(std::env::args().nth(1).as_deref(),Some("simctl"|"devices"|"-s"|"-version"|"version"|"-list-avds"|"--find")){
        devices::peer(&std::env::args().skip(1).collect::<Vec<_>>());
        return;
    }
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("codex 0.143.0");
            return;
        }
        Some("app-server") => {
            let target: SocketAddr = std::env::var("KAR_TEST_MCP_OBSERVER")
                .unwrap()
                .parse()
                .unwrap();
            assert!(target.ip().is_loopback());
            let observer = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            peer::agent_with_observer(|v| {
                if matches!(v["method"].as_str(), Some("thread/start" | "thread/resume")) {
                    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
                    let config = &v["params"]["config"]["mcp_servers"]["kar_workbench"];
                    let token = config["http_headers"]["Authorization"].as_str().unwrap();
                    observer.send_to(token.as_bytes(), target).unwrap();
                }
            });
            return;
        }
        _ => {}
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(180), scenario())
            .await
            .unwrap();
    })
}
fn rpc(c: &Client, url: &str, auth: &str, method: &str, params: Value) -> RequestBuilder {
    c.post(format!("{url}/mcp"))
        .header("Authorization", auth)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
}
async fn open(c: &Client, url: &str, auth: &str) -> String {
    let r=rpc(c,url,auth,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"kar-test","version":"1"}}))
        .send().await.unwrap().error_for_status().unwrap();
    let sid = r.headers()["mcp-session-id"].to_str().unwrap().to_owned();
    assert_eq!(
        r.json::<Value>().await.unwrap()["result"]["protocolVersion"],
        "2025-06-18"
    );
    sid
}
async fn scenario() {
    let t = tempfile::tempdir().unwrap();
    let authorized = t.path().canonicalize().unwrap().join("projects");
    let root = authorized.join("work");
    let checkouts = authorized.join("checkouts");
    let state = t.path().join("state");
    let db = t.path().join("provider");
    for dir in [&root, &db, &checkouts] {
        fs::create_dir_all(dir).unwrap();
    }
    let config = t.path().join("forge.toml");
    let writes = issues::fixture(&root, &config).await;
    let observer = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = observer.local_addr().unwrap().to_string();
    let device_bin=std::env::current_exe().unwrap();
    let (mut kar, url) = server::serve_with_env(
        &state,
        &authorized,
        &db,
        &[
            ("KAR_TEST_MCP_OBSERVER", address.as_ref()),
            ("KAR_DEVICE_TEST_DIR",db.as_os_str()),
            ("KAR_DEVICE_XCRUN_BIN",device_bin.as_os_str()),
            ("KAR_DEVICE_ADB_BIN",device_bin.as_os_str()),
            ("KAR_DEVICE_EMULATOR_BIN",device_bin.as_os_str()),
            ("KAR_FORGE_CONFIG", config.as_os_str()),
            ("KAR_MCP_TEST_FORGE", "local-only".as_ref()),
        ],
    )
    .await;
    let c = Client::new();
    let owner = "Bearer kar-workbench-fixture";
    let a = server::client();
    assert_eq!(
        rpc(&c, &url, "", "ping", json!({}))
            .header("origin", "http://localhost")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rpc(&c, &url, "", "ping", json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let owner_sid = open(&c, &url, owner).await;
    let proj = server::json(
        a.post(format!("{url}/projects"))
            .json(&json!({"root":root,"name":"one"})),
    )
    .await;
    let p = proj["id"].as_str().unwrap();
    let chat = Uuid::new_v4();
    server::json(
        a.post(format!("{url}/projects/{p}/chats"))
            .json(&json!({"id":chat,"runtime":"codex"})),
    )
    .await;
    let mut buf = [0u8; 192];
    let count = observer.recv(&mut buf).await.unwrap();
    let provider = std::str::from_utf8(&buf[..count]).unwrap();
    assert_ne!(provider, owner);
    let provider_sid = open(&c, &url, provider).await;
    let call = |method: &str, params: Value| {
        rpc(&c, &url, provider, method, params)
            .header("mcp-session-id", &provider_sid)
            .header("kar-tool-exposure", "deferred")
    };
    assert_eq!(
        rpc(&c, &url, owner, "tools/list", json!({}))
            .header("mcp-session-id", &provider_sid)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let tools = server::json(call("tools/list", json!({}))).await;
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 2);
    let stream = || {
        c.get(format!("{url}/mcp"))
            .header("Authorization", provider)
            .header("mcp-session-id", &provider_sid)
            .header("accept", "text/event-stream")
            .send()
    };
    let mut stream1 = stream().await.unwrap().error_for_status().unwrap();
    let mut stream2 = stream().await.unwrap().error_for_status().unwrap();
    let found = server::json(call(
        "tools/call",
        json!({"name":"tool_search","arguments":{"query":"kar_project_read"}}),
    ))
    .await;
    assert_eq!(
        found["result"]["structuredContent"]["tools"][0]["name"],
        "kar_project_read"
    );
    for s in [&mut stream1, &mut stream2] {
        let mut data = Vec::new();
        while !String::from_utf8_lossy(&data).contains("notifications/tools/list_changed") {
            data.extend_from_slice(&s.chunk().await.unwrap().unwrap());
        }
    }
    drop((stream1, stream2));
    let listed = server::json(call("tools/list", json!({}))).await;
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 3);
    let read = server::json(call(
        "tools/call",
        json!({"name":"kar_project_read","arguments":{"project_id":p}}),
    ))
    .await;
    assert_eq!(read["result"]["structuredContent"]["name"], "one");
    let foreign = server::json(call(
        "tools/call",
        json!({"name":"kar_project_read","arguments":{"project_id":Uuid::new_v4()}}),
    ))
    .await;
    assert_eq!(foreign["result"]["isError"], true);
    let comment = issues::exercise(&url, &a, p, chat, provider, &root, &writes, call).await;
    projects::exercise(&url, &a, p, chat, &root, call).await;
    preview_tools::controls(&url, &a, p, chat, call).await;
    preview_tools::devices(&url,&a,p,chat,call).await;
    preview_tools::html(&url,&a,p,chat,&root,call).await;
    workflow::exercise(&url, &a, p, chat, call).await;
    let stateless = |r: RequestBuilder| {
        r.header("mcp-method", "server/discover")
            .header("mcp-protocol-version", "2026-07-28")
    };
    let discover=server::json(stateless(rpc(&c,&url,owner,"server/discover",json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}})))).await;
    assert_eq!(discover["result"]["resultType"], "complete");
    assert_eq!(
        stateless(rpc(&c, &url, owner, "tools/list", json!({})))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let code = server::json(a.post(format!("{url}/access/pairings"))).await;
    let paired = server::json(
        c.post(format!("{url}/access/pair"))
            .json(&json!({"credential":code["credential"],"label":"mcp"})),
    )
    .await;
    let device = format!("Bearer {}", paired["token"].as_str().unwrap());
    let device_sid = open(&c, &url, &device).await;
    a.delete(format!(
        "{url}/access/devices/{}",
        paired["id"].as_str().unwrap()
    ))
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert_eq!(
        rpc(&c, &url, &device, "ping", json!({}))
            .header("mcp-session-id", device_sid)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    c.delete(format!("{url}/mcp"))
        .header("Authorization", owner)
        .header("mcp-session-id", &owner_sid)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        rpc(&c, &url, owner, "ping", json!({}))
            .header("mcp-session-id", owner_sid)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    server::json(
        a.put(format!("{url}/projects/{p}/chats/{chat}/archive"))
            .json(&json!({"archived":true})),
    )
    .await;
    assert_eq!(
        call("tools/list", json!({})).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let journal = fs::read(state.join(format!("{chat}.karchat"))).unwrap();
    assert!(
        !journal
            .windows(provider.len())
            .any(|w| w == provider.as_bytes())
    );
    let moved=worktree::exercise(&url,&a,&observer,p,&checkouts,&db).await;
    let interrupted=worktree::Interrupted::prepare(&a,&url,p,&observer,&checkouts,&db).await;
    kar.kill().await.unwrap();
    let (mut restarted, url) = server::serve_with_env(&state,&authorized,&db,&[("KAR_TEST_MCP_OBSERVER",address.as_ref())]).await;
    let policy = server::json(a.get(format!(
        "{url}/projects/{p}/chats/{chat}/model-tools/permissions"
    )))
    .await;
    assert_eq!(policy["mode"], "execute");
    assert_eq!(policy["issue_writes"], false);
    let receipt =
        server::json(a.get(format!("{url}/projects/{p}/forge/operations/{comment}"))).await;
    assert_eq!(receipt["result"]["Ok"]["comment_id"], 31);
    assert_eq!(writes.lock().unwrap().len(), 4);
    moved.check(&a,&url,p).await;
    interrupted.check(&a,&url,p,&db).await;
    restarted.kill().await.unwrap();
}
