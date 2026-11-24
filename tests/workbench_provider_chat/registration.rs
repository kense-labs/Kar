//! A native-protocol peer calls back into the normal Kar workbench while opening and resuming.
use super::{emit, server};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use uuid::Uuid;

pub(super) fn mode() -> Option<String> {
    std::env::var("KAR_MODEL_TOOLS_FIXTURE").ok()
}
fn db() -> PathBuf {
    std::env::var_os("FIXTURE_NATIVE_DB").unwrap().into()
}
fn record(method: &str, v: Value) {
    fs::write(db().join(method.replace('/', "-")), v.to_string()).unwrap();
}
fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
fn initialize() -> Value {
    rpc(
        1,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{}}),
    )
}
fn who() -> Value {
    rpc(
        2,
        "tools/call",
        json!({"name":"orchestrator_capabilities","arguments":{}}),
    )
}
pub(super) fn acp(
    req: &Value,
    rejected: &mut bool,
    lines: &mut impl Iterator<Item = std::io::Result<String>>,
) -> bool {
    let Some(mode) = mode() else { return false };
    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
    let method = req["method"].as_str().unwrap();
    let entries = req["params"]["mcpServers"].as_array().unwrap();
    if mode == "legacy" || mode == "strict" {
        if !entries.is_empty() {
            assert!(!*rejected);
            *rejected = true;
            assert_eq!(
                entries[0]["name"],
                if mode == "legacy" {
                    "kar_workbench"
                } else {
                    "explicit"
                }
            );
            emit(
                json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32602,"message":"mcpServers unsupported by this version"}}),
            );
            return true;
        }
        assert_eq!(mode, "legacy");
        assert!(*rejected);
        record(method, json!("continued without optional server"));
        return false;
    }
    if mode == "in-band" {
        let [s] = entries.as_slice() else {
            panic!("expected a single negotiated server")
        };
        assert_eq!(s["type"], "acp");
        assert_eq!(s["name"], "kar_workbench");
        assert!(s.get("headers").is_none() && s.get("command").is_none());
        let mut call = |id, method, params| {
            emit(rpc(id, method, params));
            let r: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert_eq!(r["id"], id);
            r
        };
        assert!(
            call(199, "mcp/connect", json!({"serverId":"unknown"}))
                .get("error")
                .is_some()
        );
        let conn =
            call(200, "mcp/connect", json!({"serverId":s["serverId"]}))["result"]["connectionId"]
                .clone();
        assert!(conn.is_string());
        let init = call(
            201,
            "mcp/message",
            json!({"connectionId":conn,"method":"initialize","params":initialize()["params"]}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "Kar");
        let who = call(
            202,
            "mcp/message",
            json!({"connectionId":conn,"method":"tools/call","params":who()["params"]}),
        );
        record(
            method,
            json!({"scope":who["result"]["structuredContent"]["provider_session"]}),
        );
        assert_eq!(
            call(203, "mcp/disconnect", json!({"connectionId":conn}))["result"],
            json!({})
        );
        assert!(
            call(
                204,
                "mcp/message",
                json!({"connectionId":conn,"method":"ping"})
            )
            .get("error")
            .is_some()
        );
        return false;
    }
    assert_eq!(mode, "bridge");
    let [server] = entries.as_slice() else {
        panic!("expected one scoped stdio server")
    };
    assert_eq!(server["name"], "kar_workbench");
    assert_eq!(server["env"], json!([]));
    assert!(server.get("headers").is_none() && !server.to_string().contains("Bearer "));
    let args = server["args"].as_array().unwrap();
    assert_eq!(args[0], "workbench-mcp-bridge");
    let mut child = Command::new(server["command"].as_str().unwrap())
        .args(args.iter().map(|a| a.as_str().unwrap()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut call = |msg: Value| {
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
        serde_json::from_str::<Value>(&output.next().unwrap().unwrap()).unwrap()
    };
    assert_eq!(call(initialize())["result"]["serverInfo"]["name"], "Kar");
    let scope = call(who())["result"]["structuredContent"]["provider_session"].clone();
    record(method, json!({"scope":scope,"socket":args[1]}));
    drop(input);
    assert!(child.wait().unwrap().success());
    false
}
pub(super) fn muse(req: &Value) {
    let Some(mode) = mode() else { return };
    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
    let method = req["method"].as_str().unwrap();
    if mode == "muse-old" {
        assert!(req["params"].get("config").is_none());
        record(method, json!("continued without sessionMcp"));
        return;
    }
    assert_eq!(mode, "muse");
    let m = &req["params"]["config"]["mcpServers"]["kar_workbench"];
    assert_eq!(m["mode"], "optional");
    assert_eq!(m["transport"], "streamableHttp");
    let endpoint = m["url"].as_str().unwrap();
    let auth = m["headers"]["Authorization"].as_str().unwrap();
    assert!(auth.starts_with("Bearer ") && auth != "Bearer kar-workbench-fixture");
    let scope = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        let req = |body: Value| {
            c.post(endpoint)
                .header("Authorization", auth)
                .header("accept", "application/json, text/event-stream")
                .json(&body)
        };
        let init = req(initialize())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let sid = init.headers()["mcp-session-id"].clone();
        let value = req(who())
            .header("mcp-session-id", sid)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        value["result"]["structuredContent"]["provider_session"].clone()
    });
    record(method, json!({"scope":scope}));
}
fn receipt(db: &Path, method: &str) -> Value {
    serde_json::from_slice(&fs::read(db.join(method)).unwrap()).unwrap()
}
async fn workbench(id: &str, mode: &str, profile: bool) {
    let t = tempfile::tempdir().unwrap();
    let state = t.path().join("state");
    let root = t.path().join("root");
    let db = t.path().join("native");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&db).unwrap();
    let executable = std::env::current_exe().unwrap();
    let key = format!("KAR_{}_BIN", id.to_uppercase());
    let vars = [
        (key.as_str(), executable.as_os_str()),
        ("KAR_CHAT_TEST_PROVIDER", id.as_ref()),
        ("KAR_MODEL_TOOLS_FIXTURE", mode.as_ref()),
    ];
    let c = server::client();
    let chat = Uuid::new_v4();
    let (mut kar, url) = server::serve_with_env(&state, &root, &db, &vars).await;
    let p = server::json(
        c.post(format!("{url}/projects"))
            .json(&json!({"root":root,"name":"registration"})),
    )
    .await["id"]
        .clone();
    let project = p.as_str().unwrap();
    let runtime = if profile {
        server::json(c.put(format!("{url}/acp-registry/profiles/model-tools")).json(&json!({
            "name":"Model tools ACP","source":"local","command_path":executable,"command_args":["acp"]
        }))).await["runtime"].as_str().unwrap().to_owned()
    } else {
        id.to_owned()
    };
    server::json(
        c.post(format!("{url}/projects/{project}/chats"))
            .json(&json!({"id":chat,"runtime":runtime})),
    )
    .await;
    let created = receipt(
        &db,
        if id == "muse" {
            "session-start"
        } else {
            "session-new"
        },
    );
    if mode == "bridge" {
        // A process outside the provider tree knows the socket name and still has no access.
        let mut impostor = Command::new(env!("CARGO_BIN_EXE_kar"))
            .arg("workbench-mcp-bridge")
            .arg(created["socket"].as_str().unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = writeln!(impostor.stdin.take().unwrap(), "{}", initialize());
        assert!(impostor.wait_with_output().unwrap().stdout.is_empty());
    }
    kar.kill().await.unwrap();
    let (mut next, url) = server::serve_with_env(&state, &root, &db, &vars).await;
    let turn = server::json(
        c.post(format!("{url}/projects/{project}/chats/{chat}/messages"))
            .json(&json!({"id":Uuid::new_v4(),"text":"continue"})),
    )
    .await;
    assert_eq!(turn["status"], "completed");
    let resumed = receipt(
        &db,
        if id == "muse" {
            "session-resume"
        } else {
            "session-load"
        },
    );
    if matches!(mode, "muse" | "bridge" | "in-band") {
        for r in [&created, &resumed] {
            assert_eq!(r["scope"]["project_id"], p);
            assert_eq!(r["scope"]["thread_id"], chat.to_string());
            assert_eq!(r["scope"]["runtime"], runtime);
        }
        assert_ne!(created["scope"]["id"], resumed["scope"]["id"]);
    } else {
        assert!(created.is_string() && resumed.is_string());
    }
    next.kill().await.unwrap();
}
pub(super) async fn scenario() {
    for (id, m) in [("muse", "muse"), ("muse", "muse-old"), ("kiro", "in-band")] {
        workbench(id, m, false).await;
    }
    #[cfg(target_os = "macos")]
    for (id, m) in [("cursor", "bridge"), ("fx", "legacy")] {
        workbench(id, m, false).await;
    }
    let tmp = tempfile::tempdir().unwrap();
    workbench("cursor", "in-band", true).await;
    #[cfg(target_os = "macos")]
    for mode in ["bridge", "legacy"] {
        workbench("cursor", mode, true).await;
    }
    let mut opts = kar::SessionOptions::new(tmp.path());
    opts.env
        .insert("KAR_CHAT_TEST_PROVIDER".into(), "fx".into());
    opts.env
        .insert("KAR_MODEL_TOOLS_FIXTURE".into(), "strict".into());
    opts.unset_env.insert("KAR_WORKBENCH_TOKEN".into());
    opts.mcp_servers.insert(
        "explicit".into(),
        kar::McpServer::Stdio {
            command: "unused".into(),
            args: Vec::new(),
            env: Default::default(),
        },
    );
    let rt = kar::Runtime::new(
        "fx",
        std::env::current_exe().unwrap(),
        &["acp"],
        kar::Protocol::Acp,
    );
    let err = rt
        .open(opts)
        .await
        .err()
        .expect("explicitly requested server was silently skipped");
    assert!(err.to_string().contains("mcpServers"));
}
