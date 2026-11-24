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
            let host = kar::serve_mcp_tools(name, Vec::new(), None).await.unwrap();
            let McpServer::HostedHttp { url, headers } = host.server.clone() else {
                panic!("expected a local HTTP endpoint");
            };
            opts.mcp_servers
                .insert(name.into(), McpServer::Http { url, headers });
            let session = runtime
                .open(opts)
                .await
                .expect("declared HTTP MCP support must open");
            session.close().await.unwrap();
            host.close().await;
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
