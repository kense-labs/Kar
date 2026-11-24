use super::*;
use kar::{
    McpServer, McpStartupStatus,
    host_tools::{HostTool, HostToolResult},
    serve_mcp_tools,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
};

#[tokio::test]
#[ignore = "requires KAR_CODEX_BIN; one held HTTP MCP server, one failing stdio MCP server and a local Responses model"]
async fn per_server_startup_precedes_the_first_model_request() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("codex");
    let work = tmp.path().join("work");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&work).unwrap();
    let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = model_listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = captured.clone();
    let model = tokio::spawn(async move {
        while let Ok((client, _)) = model_listener.accept().await {
            let seen = seen.clone();
            let socket = client.into_std().unwrap();
            socket.set_nonblocking(false).unwrap();
            tokio::task::spawn_blocking(move || {
                respond_observed(socket, None, None, None, false, |path, body| {
                    if path.ends_with("/responses") {
                        seen.lock().unwrap().push(body.clone());
                    }
                })
            });
        }
    });
    std::fs::write(home.join("config.toml"),format!("model = \"gpt-5.1\"\nmodel_provider = \"fixture\"\n[model_providers.fixture]\nname = \"local\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nenv_key = \"KAR_FIXTURE_KEY\"\nwire_api = \"responses\"\n")).unwrap();
    let tool = HostTool::new(
        "kar_probe_alpha",
        json!({"type":"object","properties":{}})
            .as_object()
            .unwrap()
            .clone(),
        |_, _| async {
            Ok(HostToolResult {
                content: vec![],
                is_error: false,
            })
        },
    );
    let held = serve_mcp_tools("held", vec![tool], None).await.unwrap();
    let McpServer::HostedHttp { url, headers } = held.server.clone() else {
        panic!("expected local HTTP host")
    };
    let upstream = url::Url::parse(&url).unwrap();
    let target = upstream.socket_addrs(|| None).unwrap()[0];
    assert!(target.ip().is_loopback());
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut proxy_url = upstream;
    proxy_url
        .set_port(Some(proxy.local_addr().unwrap().port()))
        .unwrap();
    let (release, gate) = watch::channel(false);
    let connected = Arc::new(AtomicBool::new(false));
    let started = connected.clone();
    let relay = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        while let Ok((mut inbound, _)) = proxy.accept().await {
            let mut gate = gate.clone();
            started.store(true, Ordering::SeqCst);
            peers.spawn(async move {
                if gate.wait_for(|open| *open).await.is_ok()
                    && let Ok(mut outbound) = TcpStream::connect(target).await
                {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    let mut opts = SessionOptions::new(&work);
    opts.env.insert("CODEX_HOME".into(), home.into());
    opts.env
        .insert("KAR_FIXTURE_KEY".into(), "loopback-only".into());
    opts.mcp_servers.extend([
        (
            "held".into(),
            McpServer::Http {
                url: proxy_url.to_string(),
                headers,
            },
        ),
        (
            "broken".into(),
            McpServer::Stdio {
                command: "/usr/bin/false".into(),
                args: vec![],
                env: Default::default(),
            },
        ),
    ]);
    let rt = kar::runtime("codex").unwrap();
    let installed = rt
        .probe_installation(&kar::InstallationOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        installed,
        kar::InstallationSnapshot::Available { .. }
    ));
    let session = rt.open(opts).await.unwrap();
    let wait = |ready| {
        let s = &session;
        async move {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let report = s.snapshot().mcp_startup;
                    if report.get("broken") == Some(&McpStartupStatus::Failed)
                        && report.get("held") == Some(&ready)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    };
    wait(McpStartupStatus::Pending).await;
    assert!(connected.load(Ordering::SeqCst));
    assert!(captured.lock().unwrap().is_empty());
    release.send_replace(true);
    wait(McpStartupStatus::Ready).await;
    assert_eq!(
        session
            .prompt("Say ok.")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    assert!(
        captured.lock().unwrap()[0]["tools"]
            .to_string()
            .contains("kar_probe_alpha")
    );
    session.close().await.unwrap();
    held.close().await;
    relay.abort();
    model.abort();
}
