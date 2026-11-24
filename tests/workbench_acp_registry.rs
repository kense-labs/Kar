//! Actual Kar HTTP/persistence and archive handling, with Rust registry, ACP and package-manager peers.
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod server;
use axum::{Json, Router, routing::get};
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

fn peer() {
    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
    assert_eq!(
        std::env::var("KAR_FIXTURE_DISTRIBUTION_ENV").as_deref(),
        Ok("peer")
    );
    let db = PathBuf::from(std::env::var_os("FIXTURE_NATIVE_DB").unwrap())
        .join(std::env::var("KAR_FIXTURE_KIND").unwrap());
    let native = "prepared-session";
    for line in std::io::stdin().lock().lines() {
        let req: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let p = &req["params"];
        let result = match req["method"].as_str().unwrap() {
            "initialize" => json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true}}),
            "session/new" => {
                assert!(!db.exists());
                fs::write(&db, native).unwrap();
                json!({"sessionId":native})
            }
            "session/load" => {
                assert_eq!(p["sessionId"], fs::read_to_string(&db).unwrap());
                json!({"sessionId":native})
            }
            "session/prompt" => {
                assert_eq!(p["sessionId"], native);
                let text = p["prompt"][0]["text"].as_str().unwrap();
                println!(
                    "{}",
                    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":native,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("received {text}")}}}})
                );
                json!({"stopReason":"end_turn"})
            }
            method => panic!("unexpected ACP method: {method}"),
        };
        println!(
            "{}",
            json!({"jsonrpc":"2.0","id":req["id"],"result":result})
        );
        std::io::stdout().flush().unwrap();
    }
}
fn manager(kind: &str, args: &[String]) {
    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
    let prefix = PathBuf::from(
        std::env::var_os(if kind == "npm" {
            "npm_config_prefix"
        } else {
            "UV_TOOL_DIR"
        })
        .unwrap(),
    );
    assert!(prefix.starts_with(std::env::var_os("KAR_FIXTURE_INSTALL_ROOT").unwrap()));
    if kind == "uv" && args == ["tool", "list"] {
        if prefix.join("bin/demo").is_file() {
            println!("demo v1.2.3");
        }
        return;
    }
    let bin = prefix.join("bin");
    fs::create_dir_all(&bin).unwrap();
    if kind == "npm" {
        assert_eq!(args, ["install", "--global", "@fixture/demo@1.2.3"]);
        let pkg = prefix.join("lib/node_modules/@fixture/demo");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("package.json"),
            r#"{"name":"@fixture/demo","version":"1.2.3","bin":{"demo":"agent.js"}}"#,
        )
        .unwrap();
    } else {
        assert_eq!(args, ["tool", "install", "--force", "demo==1.2.3"]);
        assert_eq!(std::env::var_os("UV_TOOL_BIN_DIR").unwrap(), bin);
    }
    fs::hard_link(std::env::current_exe().unwrap(), bin.join("demo")).unwrap();
}
fn archive() -> Vec<u8> {
    let mut writer = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    writer
        .append_path_with_name(std::env::current_exe().unwrap(), "libexec/agent")
        .unwrap();
    writer.into_inner().unwrap().finish().unwrap()
}
fn small_archives() -> [(&'static str, &'static str, Vec<u8>); 3] {
    let data = b"fixture executable content";
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("libexec/agent", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(data).unwrap();
    let zip = zip.finish().unwrap().into_inner();
    let compressed = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
    let mut tar = tar::Builder::new(compressed);
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o500);
    header.set_size(data.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "libexec/agent", &data[..])
        .unwrap();
    let bz = tar.into_inner().unwrap().finish().unwrap();
    [
        ("small-zip", "sample.zip", zip),
        ("small-bzip2", "sample.tbz2", bz),
        ("small-raw", "sample.bin", data.to_vec()),
    ]
}
async fn invoke(client: &Client, base: &str, id: &str, kind: &str) -> Value {
    server::json(
        client
            .post(format!("{base}/acp-registry/prepare"))
            .json(&json!({"agent_id":id,"distribution":kind})),
    )
    .await
}
async fn exercise() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("root");
    let state = t.path().canonicalize().unwrap().join("state");
    let db = t.path().join("native");
    let bin = t.path().join("bin");
    for d in [&root, &db, &bin] {
        fs::create_dir_all(d).unwrap();
    }
    let me = std::env::current_exe().unwrap();
    for name in ["npm", "uv"] {
        fs::hard_link(&me, bin.join(name)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let catalog_url = format!("http://{}/index", socket.local_addr().unwrap());
    let archive_url = catalog_url.replace("/index", "/agent.tgz");
    let data = archive();
    let sha = format!("{:x}", Sha256::digest(&data));
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let platform = format!("{os}-{}", std::env::consts::ARCH);
    let vars = json!({"KAR_FIXTURE_DISTRIBUTION_ENV":"peer"});
    let mut dist = json!({"binary":{(platform.as_str()):{"archive":archive_url,"sha256":sha,"cmd":"./libexec/agent","args":["acp-fixture"],"env":vars}},
        "npx":{"package":"@fixture/demo@1.2.3","args":["acp-fixture"],"env":vars},"uvx":{"package":"demo@1.2.3","args":["acp-fixture"],"env":vars}});
    for (kind, slot) in [
        ("binary", vec!["binary", platform.as_str()]),
        ("npx", vec!["npx"]),
        ("uvx", vec!["uvx"]),
    ] {
        let mut target = &mut dist;
        for key in slot {
            target = &mut target[key];
        }
        target["env"]["KAR_FIXTURE_KIND"] = kind.into();
    }
    let agent = json!({"id":"demo","name":"Demo Agent","version":"1.2.3","description":"fixture","distribution":dist});
    let mut wrong = agent.clone();
    wrong["id"] = "bad-hash".into();
    wrong["distribution"]["binary"][&platform]["sha256"] = "00".repeat(32).into();
    let mut other = agent.clone();
    other["id"] = "unused".into();
    let mut unpinned = agent.clone();
    unpinned["id"] = "unpinned".into();
    unpinned["distribution"]["npx"]["package"] = "other@latest".into();
    let mut agents = vec![other, wrong, agent.clone(), unpinned];
    let payload = axum::body::Bytes::from(data);
    let mut app = Router::new().route("/agent.tgz", get(move || async move { payload }));
    for (id, name, bytes) in small_archives() {
        let mut sample = agent.clone();
        sample["id"] = id.into();
        let bin = &mut sample["distribution"]["binary"][&platform];
        bin["archive"] = catalog_url.replace("/index", &format!("/{name}")).into();
        bin["sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
        agents.push(sample);
        let bytes = axum::body::Bytes::from(bytes);
        app = app.route(&format!("/{name}"), get(move || async move { bytes }));
    }
    let catalog = json!({"version":"1.0.0","agents":agents});
    let published = std::sync::Arc::new(std::sync::Mutex::new(catalog));
    let index = published.clone();
    let app = app.route(
        "/index",
        get(move || {
            let index = index.clone();
            async move { Json(index.lock().unwrap().clone()) }
        }),
    );
    let peer = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    let options = [
        ("KAR_ACP_REGISTRY_URL", OsStr::new(&catalog_url)),
        ("KAR_FIXTURE_INSTALL_ROOT", state.as_os_str()),
        ("PATH", path.as_os_str()),
    ];
    let (mut kar, base) = server::serve_with_env(&state, &root, &db, &options).await;
    let client = server::client();
    let search =
        server::json(client.get(format!("{base}/acp-registry/search?query=demo&limit=1"))).await;
    assert_eq!(search["agents"][0]["id"], "demo");
    assert_eq!(search["agents"][0]["integrity"], "sha256");
    let list = server::json(client.get(format!("{base}/acp-registry/search"))).await;
    assert_eq!(list["agents"].as_array().unwrap().len(), 6);
    for agent in ["small-zip", "small-bzip2", "small-raw"] {
        let receipt = invoke(&client, &base, agent, "binary").await;
        let handle = receipt["runtime"].as_str().unwrap();
        let dir = state
            .join("workbench/acp-registry/instances")
            .join(agent)
            .join("1.2.3/binary/payload/libexec/agent");
        assert_eq!(fs::read(dir).unwrap(), b"fixture executable content");
        assert_eq!(
            server::json(client.delete(format!("{base}/acp-registry/instances/{handle}"))).await["removed"],
            true
        );
    }
    assert!(
        !client
            .post(format!("{base}/acp-registry/prepare"))
            .json(&json!({"agent_id":"bad-hash"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    // Package writes, discovery and uninstall stay inside the state even when the global manager is on PATH.
    for kind in ["npx", "uvx"] {
        let install = invoke(&client, &base, "demo", kind).await;
        assert_eq!(install, invoke(&client, &base, "demo", kind).await);
        let id = install["runtime"].as_str().unwrap();
        assert_eq!(
            server::json(client.delete(format!("{base}/acp-registry/agents/demo/managed-binary")))
                .await["removed"],
            false
        );
        let prepared = server::json(client.get(format!("{base}/acp-registry/instances"))).await;
        assert_eq!(prepared["instances"][0]["runtime"], id);
        assert_eq!(
            server::json(client.delete(format!("{base}/acp-registry/instances/{id}"))).await["removed"],
            true
        );
    }
    let unused = invoke(&client, &base, "unused", "auto").await;
    assert_eq!(
        server::json(client.delete(format!("{base}/acp-registry/agents/unused/managed-binary")))
            .await["removed"],
        false
    );
    let handle = unused["runtime"].as_str().unwrap();
    assert_eq!(
        server::json(client.delete(format!("{base}/acp-registry/instances/{handle}"))).await["removed"],
        true
    );
    let instance = invoke(&client, &base, "demo", "auto").await;
    let runtime = instance["runtime"].as_str().unwrap();
    assert_eq!(runtime, "acp:demo:1.2.3:binary");
    let project = server::json(
        client
            .post(format!("{base}/projects"))
            .json(&json!({"root":root,"name":"registry"})),
    )
    .await;
    let project = project["id"].as_str().unwrap();
    let mut endpoints = Vec::new();
    for kind in ["binary", "npx", "uvx"] {
        let inst = invoke(&client, &base, "demo", kind).await;
        let handle = inst["runtime"].as_str().unwrap();
        let chat = Uuid::new_v4();
        let endpoint = format!("/projects/{project}/chats/{chat}/messages");
        let created = server::json(
            client
                .post(format!("{base}/projects/{project}/chats"))
                .json(&json!({"id":chat,"runtime":handle})),
        )
        .await;
        assert!(created.to_string().contains("prepared-session"));
        assert!(
            server::json(
                client
                    .post(format!("{base}{endpoint}"))
                    .json(&json!({"id":Uuid::new_v4(),"text":"first"}))
            )
            .await
            .to_string()
            .contains("received first")
        );
        assert_eq!(
            server::json(client.delete(format!("{base}/acp-registry/instances/{handle}"))).await["removed"],
            false
        );
        endpoints.push((endpoint, chat, handle.to_string(), kind));
    }
    // After a publisher advances the index, only the saved release identifies the original bytes.
    let published_versions = published.lock().unwrap()["agents"]
        .as_array()
        .unwrap()
        .clone();
    published.lock().unwrap()["agents"] = published_versions
        .into_iter()
        .map(|mut a| {
            if a["id"] == "demo" {
                a["version"] = "2.0.0".into();
            }
            a
        })
        .collect::<Vec<_>>()
        .into();
    let newer = invoke(&client, &base, "demo", "binary").await;
    assert_eq!(newer["version"], "2.0.0");
    let new_handle = newer["runtime"].as_str().unwrap();
    assert_eq!(
        server::json(client.delete(format!("{base}/acp-registry/instances/{new_handle}"))).await["removed"],
        true
    );
    for (endpoint, chat, handle, kind) in &endpoints {
        let prefix = format!("{base}/projects/{project}/chats/{chat}");
        if *kind == "binary" {
            server::json(client.post(format!("{prefix}/force-stop")).json(&json!({}))).await;
        } else {
            server::json(
                client
                    .put(format!("{prefix}/archive"))
                    .json(&json!({"archived":true})),
            )
            .await;
        }
        let path = if *kind == "binary" {
            format!("{base}/acp-registry/agents/demo/managed-binary")
        } else {
            format!("{base}/acp-registry/instances/{handle}")
        };
        assert_eq!(server::json(client.delete(path)).await["removed"], true);
        let snapshot = server::json(client.get(format!("{prefix}/snapshot"))).await;
        assert_eq!(snapshot["history"]["chat"]["id"], chat.to_string());
        assert_eq!(snapshot["history"]["chat"]["native_id"], "prepared-session");
        assert_eq!(snapshot["history"]["chat"]["runtime"], handle.as_str());
        assert_eq!(snapshot["history"]["turns"].as_array().unwrap().len(), 1);
        server::json(
            client
                .put(format!("{prefix}/archive"))
                .json(&json!({"archived":false})),
        )
        .await;
        let missing = client
            .post(format!("{base}{endpoint}"))
            .json(&json!({"id":Uuid::new_v4(),"text":"unavailable"}))
            .send()
            .await
            .unwrap();
        assert!(!missing.status().is_success());
        let restored =
            server::json(client.post(format!("{base}/acp-registry/instances/{handle}/reinstall")))
                .await;
        assert_eq!(restored["runtime"], handle.as_str());
        assert_eq!(restored["version"], "1.2.3");
    }
    kar.kill().await.unwrap();
    kar.wait().await.unwrap();
    peer.abort();
    let _ = peer.await;
    let (mut kar, base) = server::serve_with_env(&state, &root, &db, &options).await;
    for (endpoint, chat, handle, _) in endpoints {
        assert!(
            server::json(
                client
                    .post(format!("{base}{endpoint}"))
                    .json(&json!({"id":Uuid::new_v4(),"text":"second"}))
            )
            .await
            .to_string()
            .contains("received second")
        );
        let snapshot =
            server::json(client.get(format!("{base}/projects/{project}/chats/{chat}/snapshot")))
                .await;
        assert_eq!(snapshot["history"]["chat"]["runtime"], handle);
        assert_eq!(snapshot["history"]["chat"]["native_id"], "prepared-session");
        assert_eq!(snapshot["history"]["turns"].as_array().unwrap().len(), 2);
    }
    assert_eq!(
        server::json(client.delete(format!("{base}/acp-registry/agents/demo/managed-binary")))
            .await["removed"],
        false
    );
    kar.kill().await.unwrap();
    kar.wait().await.unwrap();
}
async fn official(agent: &str) {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("workspace");
    let state = d.path().join("state");
    fs::create_dir(&root).unwrap();
    let options = [(
        "KAR_ACP_REGISTRY_URL",
        OsStr::new("https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json"),
    )];
    let (mut host, url) = server::serve_with_env(&state, &root, d.path(), &options).await;
    let client = server::client();
    let listed = server::json(
        client
            .get(format!("{url}/acp-registry/search"))
            .query(&[("query", agent)]),
    )
    .await;
    assert!(
        listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == agent),
        "agent is not usable on this host"
    );
    let kind = std::env::var("KAR_ACP_LIVE_DISTRIBUTION").unwrap_or_else(|_| "auto".into());
    let installed = invoke(&client, &url, agent, &kind).await;
    let runtime = installed["runtime"].as_str().unwrap();
    let project = server::json(
        client
            .post(format!("{url}/projects"))
            .json(&json!({"root":root,"name":"official ACP"})),
    )
    .await;
    let project = project["id"].as_str().unwrap();
    let chat = Uuid::new_v4();
    let path = format!("/projects/{project}/chats/{chat}/messages");
    let created = server::json(
        client
            .post(format!("{url}/projects/{project}/chats"))
            .json(&json!({"id":chat,"runtime":runtime})),
    )
    .await;
    let nonce = Uuid::new_v4().to_string();
    let send = |url: &str, text: String| {
        client
            .post(format!("{url}{path}"))
            .json(&json!({"id":Uuid::new_v4(),"text":text}))
    };
    let contains = |turn: &Value| {
        turn["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["role"] == "assistant")
            .any(|i| i["text"].as_str().is_some_and(|s| s.contains(&nonce)))
    };
    assert!(contains(
        &server::json(send(
            &url,
            format!("Remember {nonce} and reply with exactly that value.")
        ))
        .await
    ));
    host.kill().await.unwrap();
    host.wait().await.unwrap();
    let (mut host, url) = server::serve_with_env(&state, &root, d.path(), &options).await;
    assert!(contains(
        &server::json(send(
            &url,
            "Reply with just the value I asked you to remember.".into()
        ))
        .await
    ));
    println!(
        "{}",
        json!({"runtime":installed,"chat":chat,"native_session":created["info"]["native_id"],"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"recalled_after_restart":true})
    );
    host.kill().await.unwrap();
    host.wait().await.unwrap();
}
fn main() {
    let mut args = std::env::args();
    let argv = args.next().unwrap();
    let args: Vec<_> = args.collect();
    let program = Path::new(&argv).file_name().unwrap().to_str().unwrap();
    if matches!(program, "npm" | "uv") {
        manager(program, &args);
        return;
    }
    if args == ["acp-fixture"] {
        peer();
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    if args == ["official"] {
        rt.block_on(official(
            &std::env::var("KAR_ACP_LIVE_AGENT").expect("select an official registry agent"),
        ));
    } else {
        rt.block_on(exercise());
    }
}
