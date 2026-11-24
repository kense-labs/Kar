//! Local release server and native Rust protocol fixture. No vendor endpoint is contacted.
#[path = "support/codex_chat_peer.rs"]
mod codex_chat_peer;
#[path = "support/workbench_server.rs"]
#[allow(dead_code)]
mod server;
use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    routing::get,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use std::{
    fs,
    io::{BufRead, Write},
    path::Path as FsPath,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[derive(Clone)]
struct Files {
    codex: Bytes,
    agy: Bytes,
    hold: Arc<AtomicBool>,
}
async fn release(State(files): State<Files>, Path(id): Path<String>) -> Bytes {
    let wait = files.hold.load(Ordering::SeqCst);
    if wait {
        tokio::time::sleep(Duration::from_millis(600)).await
    }
    if id == "codex" {
        files.codex
    } else {
        files.agy
    }
}
fn archive(driver: &str, exe: &[u8]) -> Vec<u8> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(enc);
    let mut add = |name: &str, mode: u32, body: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_mode(mode);
        h.set_size(body.len() as u64);
        h.set_cksum();
        tar.append_data(&mut h, name, body).unwrap();
    };
    if driver == "codex" {
        add("bin/codex", 0o755, exe);
        add("bin/codex-code-mode-host", 0o755, b"fixture helper");
        add("codex-path/rg", 0o755, b"fixture helper");
        add("codex-package.json",0o644,br#"{"layoutVersion":1,"version":"0.166.0","target":"fixture-macos","entrypoint":"bin/codex"}"#)
    } else {
        add("antigravity", 0o755, exe)
    }
    tar.into_inner().unwrap().finish().unwrap()
}
fn acp() {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let msg: Value = serde_json::from_str(&line).unwrap();
        if msg["method"] == "initialize" {
            let out = json!({"jsonrpc":"2.0","id":msg["id"],"result":{"protocolVersion":1,"agentInfo":{"name":"Antigravity ACP Server","version":"1.2.3"},"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"resume":{}},"auth":{"logout":{}}},"authMethods":[{"id":"oauth-personal","name":"fixture"}]}});
            println!("{out}");
            std::io::stdout().flush().unwrap();
        }
    }
}
async fn wait(host: &reqwest::Client, url: &str, want: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let v = server::json(host.get(url)).await;
            if v["phase"] == want {
                return v;
            }
            assert_ne!(v["phase"], "failed", "{v}");
            tokio::time::sleep(Duration::from_millis(70)).await
        }
    })
    .await
    .unwrap()
}
async fn scenario() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("work");
    let native = home.path().join("native");
    let state = home.path().join("state");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&native).unwrap();
    let exe = fs::read(std::env::current_exe().unwrap()).unwrap();
    let codex = Bytes::from(archive("codex", &exe));
    let agy = Bytes::from(archive("antigravity", &exe));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", l.local_addr().unwrap());
    let path = home.path().join("catalog.json");
    let mut catalog = json!({"codex":{"version":"0.166.0","target":"fixture-macos","url":format!("{upstream}/codex"),"sha256":format!("{:x}",Sha256::digest(&codex)),"bytes":codex.len()},"antigravity":{"version":"1.2.3","target":"fixture-macos","url":format!("{upstream}/antigravity"),"sha512":format!("{:x}",Sha512::digest(&agy)),"bytes":agy.len()}});
    fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let hold = Arc::new(AtomicBool::new(true));
    let app = Router::new()
        .route("/{id}", get(release))
        .with_state(Files {
            codex,
            agy,
            hold: hold.clone(),
        });
    let http = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let env = [
        ("KAR_MANAGED_PROVIDER_CATALOG", path.as_os_str()),
        ("KAR_CODEX_BIN", std::ffi::OsStr::new("")),
        ("KAR_ANTIGRAVITY_BIN", std::ffi::OsStr::new("")),
    ];
    let (mut proc, base) = server::serve_with_env(&state, &root, &native, &env).await;
    let host = server::client();
    let id = format!("{base}/providers/codex/install");
    assert!(
        host.post(format!("{id}/start"))
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    server::json(
        host.put(format!("{base}/providers/codex/setup"))
            .json(&json!({"setupMode":"managed"})),
    )
    .await;
    let sub = || async {
        host.get(format!("{id}/events"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes_stream()
            .eventsource()
    };
    let (mut a, mut b) = tokio::join!(sub(), sub());
    for e in [
        a.next().await.unwrap().unwrap(),
        b.next().await.unwrap().unwrap(),
    ] {
        let v: Value = serde_json::from_str(&e.data).unwrap();
        assert_eq!(v["phase"], "idle")
    }
    let start = || server::json(host.post(format!("{id}/start")));
    let (j1, j2) = tokio::join!(start(), start());
    assert_eq!(j1["operationId"], j2["operationId"]);
    for e in [
        a.next().await.unwrap().unwrap(),
        b.next().await.unwrap().unwrap(),
    ] {
        let v: Value = serde_json::from_str(&e.data).unwrap();
        assert_eq!(v["operationId"], j1["operationId"])
    }
    server::json(
        host.post(format!("{id}/cancel"))
            .json(&json!({"operationId":j1["operationId"]})),
    )
    .await;
    wait(&host, &id, "cancelled").await;
    hold.store(false, Ordering::SeqCst);
    assert_ne!(start().await["operationId"], j1["operationId"]);
    let done = wait(&host, &id, "succeeded").await;
    assert_eq!(done["downloadedBytes"], done["totalBytes"]);
    assert_eq!(done["installedVersion"], "0.166.0");
    assert_eq!(done["source"], "managed");
    let command = std::path::PathBuf::from(done["executablePath"].as_str().unwrap());
    assert!(command.is_file());
    let project = server::json(
        host.post(format!("{base}/projects"))
            .json(&json!({"root":root,"name":"install"})),
    )
    .await;
    let p = project["id"].as_str().unwrap();
    let chat = server::json(
        host.post(format!("{base}/projects/{p}/chats"))
            .json(&json!({"runtime":"codex"})),
    )
    .await;
    assert!(chat["info"]["id"].is_string());
    assert!(
        host.post(format!("{id}/remove"))
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    proc.kill().await.unwrap();
    let (mut proc, base) = server::serve_with_env(&state, &root, &native, &env).await;
    let id = format!("{base}/providers/codex/install");
    let current = server::json(host.get(&id)).await;
    assert_eq!(current["executablePath"], done["executablePath"]);
    let releases = command
        .ancestors()
        .find(|p| p.file_name() == Some(std::ffi::OsStr::new("releases")))
        .unwrap();
    let foreign = releases.join("outside-kars-ownership");
    fs::write(&foreign, b"external").unwrap();
    let removed = server::json(host.post(format!("{id}/remove"))).await;
    assert_eq!(removed["canRemove"], false);
    assert!(foreign.is_file());
    assert!(!command.exists());
    catalog["codex"]["sha256"] = "0".repeat(64).into();
    fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    server::json(host.post(format!("{id}/start"))).await;
    let rejected = wait(&host, &id, "failed").await;
    assert!(rejected["message"].as_str().unwrap().contains("checksum"));
    let id = format!("{base}/providers/antigravity/install");
    server::json(host.post(format!("{id}/start"))).await;
    let ag = wait(&host, &id, "succeeded").await;
    assert_eq!(ag["installedVersion"], "1.2.3");
    proc.kill().await.unwrap();
    let external = std::env::current_exe().unwrap();
    let env = [
        ("KAR_MANAGED_PROVIDER_CATALOG", path.as_os_str()),
        ("KAR_ANTIGRAVITY_BIN", external.as_os_str()),
    ];
    let (mut proc, base) = server::serve_with_env(&state, &root, &native, &env).await;
    assert!(
        host.post(format!("{base}/providers/antigravity/install/remove"))
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    assert!(external.is_file());
    assert!(FsPath::new(ag["executablePath"].as_str().unwrap()).exists());
    let code = reqwest::Client::new()
        .post(format!("{base}/providers/antigravity/install/start"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::FORBIDDEN);
    proc.kill().await.unwrap();
    http.abort();
}
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => println!("codex-cli 0.166.0"),
        Some("app-server") => codex_chat_peer::agent(),
        Some("acp") => acp(),
        _ => tokio::runtime::Runtime::new().unwrap().block_on(scenario()),
    }
}
