#[path = "support/declaration.rs"]
mod declaration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use kar::{
    McpServer,
    host_tools::{HostTool, HostToolContent, HostToolResult, ServedMcpTools},
    serve_mcp_tools,
};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

struct Peer {
    client: Client,
    url: String,
    authorization: String,
}
impl Peer {
    fn new(host: &ServedMcpTools) -> Self {
        let McpServer::HostedHttp { url, headers } = host.server.clone() else {
            panic!()
        };
        Self {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap(),
            url,
            authorization: headers["Authorization"].clone(),
        }
    }
    fn post(&self, id: &str, body: Value) -> RequestBuilder {
        self.client
            .post(&self.url)
            .header("authorization", &self.authorization)
            .header("mcp-session-id", id)
            .json(&body)
    }
    async fn rpc(&self, session: &str, body: Value) -> Value {
        self.post(session, body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn open(&self) -> String {
        let res = self
            .post(
                "ignored",
                json!({"id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
            )
            .send()
            .await
            .unwrap();
        let id = res.headers()["mcp-session-id"].to_str().unwrap().to_owned();
        assert_ne!(id, "ignored");
        let result = &res.json::<Value>().await.unwrap()["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(result["instructions"], "fixture instructions");
        id
    }
    async fn get_stream(&self, session: &str) -> reqwest::Response {
        let r = self
            .client
            .get(&self.url)
            .header("authorization", &self.authorization)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", session)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["mcp-session-id"], session);
        r
    }
}
fn tool(name: &str) -> HostTool {
    let mut tool = HostTool::new(
        name,
        json!({"type":"object","properties":{"text":{"type":"string"}}})
            .as_object()
            .unwrap()
            .clone(),
        |args, _| async move {
            match args.get("text").and_then(Value::as_str) {
                None => Err("expected text".into()),
                Some(s) => Ok(HostToolResult {
                    content: vec![
                        HostToolContent::Text { text: s.into() },
                        HostToolContent::Image {
                            data: "AA==".into(),
                            mime_type: "image/png".into(),
                        },
                    ],
                    is_error: false,
                }),
            }
        },
    );
    tool.description = Some("echo".into());
    tool.annotations = Some(json!({"readOnlyHint":true}).as_object().unwrap().clone());
    tool
}
fn call(id: Value, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

#[tokio::test]
async fn authorization_validation_batches_and_typed_results() {
    for name in ["__proto__", "bad.name", ""] {
        assert!(serve_mcp_tools(name, vec![], None).await.is_err());
    }
    for name in ["a!".to_owned(), "x".repeat(65)] {
        assert!(
            serve_mcp_tools("app", vec![tool(&name)], None)
                .await
                .is_err()
        );
    }
    let host = serve_mcp_tools(
        "fixture",
        vec![tool("echo")],
        Some("fixture instructions".into()),
    )
    .await
    .unwrap();
    let peer = Peer::new(&host);
    assert!(matches!(
        serde_json::from_value::<McpServer>(serde_json::to_value(&host.server).unwrap()).unwrap(),
        McpServer::HostedHttp { .. }
    ));
    assert_eq!(
        peer.client.get(&peer.url).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        peer.client
            .get(&peer.url)
            .header("origin", "http://example.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let get = peer
        .client
        .get(&peer.url)
        .header("origin", "http://localhost:8080")
        .header("authorization", &peer.authorization)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 405);
    assert_eq!(get.headers()["allow"], "GET, POST");
    for (s, code) in [
        ("{", -32700),
        ("[]", -32600),
        ("[{},false]", -32600),
        ("null", -32600),
    ] {
        let res = peer
            .client
            .post(&peer.url)
            .header("authorization", &peer.authorization)
            .body(s)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], code);
    }
    let too_big = peer
        .client
        .post(&peer.url)
        .header("authorization", &peer.authorization)
        .body(vec![b' '; 4 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(too_big.status(), 413);
    assert_eq!(too_big.headers()["connection"], "close");
    let id = peer.open().await;
    let replies = peer
        .rpc(
            &id,
            json!([
                {"id":"list","method":"tools/list"},{"method":"notifications/initialized"},
                call(json!(2),"echo",json!({"text":"hello"})), call(json!(3),"echo",json!(null)),
                {"id":4,"method":"unknown"}, call(json!(5),"there",json!({})),{"id":7,"result":{}}
            ]),
        )
        .await;
    assert_eq!(replies.as_array().unwrap().len(), 5);
    let t = &replies[0]["result"]["tools"][0];
    assert_eq!(t["name"], "echo");
    assert_eq!(t["inputSchema"]["properties"]["text"]["type"], "string");
    assert_eq!(t["annotations"]["readOnlyHint"], true);
    assert_eq!(replies[1]["result"]["content"][0]["text"], "hello");
    assert_eq!(replies[1]["result"]["content"][1]["mimeType"], "image/png");
    assert_eq!(replies[2]["result"]["isError"], true);
    assert_eq!(replies[3]["error"]["code"], -32601);
    assert_eq!(replies[4]["error"]["code"], -32602);
    assert_eq!(
        peer.post(&id, json!({"method":"notifications/initialized"}))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    host.close().await;
}

#[tokio::test]
async fn two_sessions_cancel_independently_and_receive_every_list_change() {
    let (started, mut calls) = mpsc::unbounded_channel();
    let slow = HostTool::new("wait", Map::new(), move |_, signal| {
        let started = started.clone();
        async move {
            let (release, done) = tokio::sync::oneshot::channel();
            started.send(release).unwrap();
            tokio::select! {
                _ = signal.cancelled() => Ok("cancelled".into()),
                _ = done => Ok("original handler".into()),
            }
        }
    });
    let host = serve_mcp_tools("app", vec![slow], Some("fixture instructions".into()))
        .await
        .unwrap();
    let peer = Peer::new(&host);
    let (a, b) = tokio::join!(peer.open(), peer.open());
    assert_ne!(a, b);
    let (ra, rb) = tokio::join!(peer.get_stream(&a), peer.get_stream(&b));
    let mut sa = ra.bytes_stream().eventsource();
    let mut sb = rb.bytes_stream().eventsource();
    let run = |session: &str| {
        let req = peer.post(session, call(1.into(), "wait", json!({})));
        tokio::spawn(async move { req.send().await.unwrap().json::<Value>().await.unwrap() })
    };
    let fa = run(&a);
    let hold_a = calls.recv().await.unwrap();
    let fb = run(&b);
    let release_b = calls.recv().await.unwrap();
    assert!(host.set_tools(vec![tool("echo"), tool("echo")]).is_err());
    assert_eq!(
        peer.rpc(&a, json!({"id":2,"method":"tools/list"})).await["result"]["tools"][0]["name"],
        "wait"
    );
    host.set_tools(vec![tool("echo")]).unwrap();
    host.set_tools(vec![]).unwrap();
    for _ in 0..2 {
        let (x, y) = tokio::join!(sa.next(), sb.next());
        for event in [x, y] {
            let event = event.unwrap().unwrap();
            assert_eq!(event.event, "message");
            assert_eq!(
                serde_json::from_str::<Value>(&event.data).unwrap()["method"],
                "notifications/tools/list_changed"
            );
        }
    }
    assert_eq!(
        peer.rpc(&a, call(2.into(), "wait", json!({}))).await["error"]["code"],
        -32602
    );
    let cancel = |id: Value| {
        peer.post(
            &a,
            json!({"method":"notifications/cancelled","params":{"requestId":id}}),
        )
    };
    assert_eq!(cancel("1".into()).send().await.unwrap().status(), 202);
    assert_eq!(
        cancel(1.into()).send().await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        fa.await.unwrap()["result"]["content"][0]["text"],
        "cancelled"
    );
    release_b.send(()).unwrap();
    assert_eq!(
        fb.await.unwrap()["result"]["content"][0]["text"],
        "original handler"
    );
    drop(hold_a);
    host.close().await;
    assert!(sa.next().await.is_none());
    assert!(sb.next().await.is_none());
}

#[tokio::test]
async fn socket_disconnect_and_server_close_signal_handlers() {
    use tokio::{io::AsyncWriteExt, net::TcpStream};
    let (notice, mut events) = mpsc::unbounded_channel();
    let wait = HostTool::new("wait", Map::new(), move |_, signal| {
        let notice = notice.clone();
        async move {
            notice.send("started").unwrap();
            signal.cancelled().await;
            notice.send("stopped").unwrap();
            Ok("stopped".into())
        }
    });
    let host = serve_mcp_tools("app", vec![wait], None).await.unwrap();
    let peer = Peer::new(&host);
    let mut host = Some(host);
    for closing in [false, true] {
        let addr = peer
            .url
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/mcp")
            .unwrap();
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let body = call(1.into(), "wait", json!({})).to_string();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {}\r\nContent-Length: {}\r\n\r\n{body}",
            peer.authorization,
            body.len()
        );
        socket.write_all(req.as_bytes()).await.unwrap();
        assert_eq!(events.recv().await, Some("started"));
        if closing {
            host.take().unwrap().close().await;
        } else {
            drop(socket);
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap(),
            Some("stopped")
        );
        if closing {
            break;
        }
    }
}

#[tokio::test]
#[ignore = "requires Codex CLI with thread-scoped MCP startup notifications"]
async fn codex_reports_the_required_host_service_ready() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex-home");
    std::fs::create_dir(&home).unwrap();
    // A same-name disabled user entry must not disable the host's session-scoped override.
    std::fs::write(
        home.join("config.toml"),
        r#"
model = "fixture"
model_provider = "localfixture"
[model_providers.localfixture]
name = "localfixture"
base_url = "http://127.0.0.1:1/v1"
wire_api = "responses"
[mcp_servers.fixture]
url = "http://127.0.0.1:1/mcp"
enabled = false
"#,
    )
    .unwrap();
    let host = serve_mcp_tools("fixture", vec![tool("echo")], None)
        .await
        .unwrap();
    let mut opts = kar::SessionOptions::new(temp.path());
    opts.env.insert("CODEX_HOME".into(), home.into());
    opts.mcp_servers.extend([host.entry()]);
    let rt = kar::runtime("codex").unwrap();
    let declared = declaration::for_installation(&rt, &opts).await;
    let session = rt.open(opts).await.unwrap();
    let mut records = session.subscribe(None).unwrap();
    tokio::time::timeout(Duration::from_secs(12),async {
        while let Some(r)=records.next().await {
            if r.session_id.as_deref()==Some(session.id()) && r.events.iter().any(|e| matches!(e,
                kar::Event::McpServerStartup {server,status:kar::McpStartupStatus::Ready} if server=="fixture")) {
                return;
            }
        }
        panic!("native session closed before its host service was ready");
    }).await.expect("missing thread-specific MCP readiness");
    assert_eq!(
        session.snapshot().mcp_startup.get("fixture"),
        Some(&kar::McpStartupStatus::Ready)
    );
    let caps = session.capabilities();
    declaration::check(&declared, caps);
    assert_eq!(
        caps.mcp_transports,
        Some([kar::McpTransport::Stdio, kar::McpTransport::Http].as_slice())
    );
    assert_eq!(
        serde_json::to_value(caps.mcp_startup).unwrap(),
        json!({"kind":"per_server","first_model_request":"may_start_pending"})
    );
    let token = Peer::new(&host).authorization;
    for r in session.records() {
        assert!(
            !r.native
                .to_string()
                .contains(token.strip_prefix("Bearer ").unwrap())
        );
    }
    session.close().await.unwrap();
    host.close().await;
}
