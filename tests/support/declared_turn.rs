//! Installed-provider checks for the fixed public session capability contract.
use kar::{
    Error, McpServer, McpTransport, Runtime, Session, SessionCapabilityDeclaration, SessionOptions,
};

pub async fn prepare(runtime: &Runtime, opts: &SessionOptions) -> SessionCapabilityDeclaration {
    let declared = super::declaration::for_installation(runtime, opts).await;
    if let Some(transports) = declared.mcp_transports {
        let mut opts = opts.clone();
        let name = "kar_cap_http";
        if transports.contains(&McpTransport::Http) {
            // Transport support and support for secret HTTP headers are independent.
            // This probe checks the transport alone, as in the fixed public case.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let router = axum::Router::new().route("/mcp", axum::routing::post(http_echo));
            let mut peers = tokio::task::JoinSet::new();
            peers.spawn(async move { axum::serve(listener, router).await.unwrap() });
            opts.mcp_servers.insert(
                name.into(),
                McpServer::Http {
                    url,
                    headers: Default::default(),
                },
            );
            let session = runtime
                .open(opts)
                .await
                .expect("declared HTTP MCP support must open");
            session.close().await.unwrap();
            peers.shutdown().await;
        } else {
            opts.mcp_servers.insert(
                name.into(),
                McpServer::Http {
                    url: "http://127.0.0.1:1/mcp".into(),
                    headers: Default::default(),
                },
            );
            let refused = runtime
                .open(opts)
                .await
                .err()
                .expect("undeclared HTTP MCP was accepted");
            assert!(
                matches!(&refused, Error::Unsupported(reason) if reason.to_ascii_lowercase().contains("mcp")),
                "{}: {refused}",
                runtime.id
            );
        }
    }
    declared
}

async fn http_echo(axum::Json(packet): axum::Json<serde_json::Value>) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    use serde_json::json;
    let Some(id) = packet.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match packet["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":packet["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"kar-cap-http","version":"1"}})
        }
        Some("tools/list") => {
            json!({"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]})
        }
        Some("tools/call") => {
            json!({"content":[{"type":"text","text":packet["params"]["arguments"]["text"]}]})
        }
        Some("ping") => json!({}),
        _ => return axum::Json(
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}}),
        )
        .into_response(),
    };
    axum::Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

pub async fn completed(declared: &SessionCapabilityDeclaration, session: &Session) {
    if declared.native_session_file == Some(true) {
        let file = session
            .native_session_file()
            .await
            .unwrap()
            .expect("declared native session file");
        assert!(file.is_absolute(), "native session file {file:?}");
        assert!(file.is_file(), "missing native session file {file:?}");
    }
    if let Some(fields) = declared
        .context_usage
        .filter(|fields| fields.tokens || fields.context_window)
    {
        let readback = session
            .context_usage_readback()
            .value
            .expect("declared context after a completed root turn");
        if fields.tokens {
            assert!(readback.tokens.is_some(), "{readback:?}");
        }
        if fields.context_window {
            assert!(
                readback.context_window.is_some_and(|window| window > 0),
                "{readback:?}"
            );
        }
    }
}
