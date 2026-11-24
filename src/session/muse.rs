use super::*;
use crate::{McpServer, McpTransport, opening::OpenStep};

impl Session {
    pub(super) async fn open_muse(
        &mut self,
        opts: &SessionOptions,
        cwd: &str,
    ) -> Result<(String, Value)> {
        let init = self
            .open_rpc(
                "initialize",
                json!({
                    "clientInfo":{"name":"kar","title":"Kar","version":env!("CARGO_PKG_VERSION")},
                    "capabilities":{"requestedCapabilities":["sessionMcp"]}
                }),
                OpenStep::Open,
            )
            .await?;
        if init.pointer("/schema/version").and_then(Value::as_u64) != Some(1)
            || init
                .pointer("/schema/fingerprint")
                .and_then(Value::as_str)
                .is_none()
        {
            return Err(Error::Protocol(
                "Muse did not negotiate an MSP v1 schema fingerprint".into(),
            ));
        }
        self.call(|tx| Command::Send(json!({"jsonrpc":"2.0","method":"initialized"}), tx))
            .await?;
        let supports_mcp = init["grantedCapabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|s| s == "sessionMcp"));
        self.mcp.transports = Some(if supports_mcp {
            &[McpTransport::Http]
        } else {
            &[]
        });
        let id = opts
            .resume
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        let mut params =
            json!({"sessionId":id,"commandId":Uuid::now_v7(),"permissionProfile":":ask-me"});
        let (method, step) = if opts.resume.is_some() {
            params["excludeItems"] = true.into();
            ("session/resume", OpenStep::Target(id.clone()))
        } else {
            params["workspaceRoot"] = cwd.into();
            params["providerId"] = "meta".into();
            params["approvalMode"] = "promptUnmatched".into();
            if let Some(m) = &opts.model {
                params["modelId"] = m.as_str().into();
            }
            ("session/start", OpenStep::Open)
        };
        let workbench = supports_mcp
            .then_some(opts.workbench_registration.as_ref())
            .flatten()
            .filter(|_| {
                !opts
                    .mcp_servers
                    .contains_key(crate::WorkbenchRegistration::NAME)
            });
        if !opts.mcp_servers.is_empty() || workbench.is_some() {
            if !supports_mcp {
                return Err(Error::Unsupported("Muse did not grant sessionMcp".into()));
            }
            let mut servers = serde_json::Map::new();
            for (key, server, mode) in opts
                .mcp_servers
                .iter()
                .map(|(k, s)| (k.as_str(), s, "required"))
                .chain(workbench.map(|r| (crate::WorkbenchRegistration::NAME, &r.http, "optional")))
            {
                let (McpServer::Http { url, headers } | McpServer::HostedHttp { url, headers }) =
                    server
                else {
                    return Err(Error::Unsupported(
                        "Muse session MCP needs a streamable HTTP server".into(),
                    ));
                };
                servers.insert(
                    key.into(),
                    json!({"mode":mode,"transport":"streamableHttp","url":url,"headers":headers}),
                );
            }
            params["config"] = json!({"mcpServers":servers});
        }
        let mut result = self.open_rpc(method, params, step).await?;
        if result.pointer("/session/sessionId").and_then(Value::as_str) != Some(&id) {
            return Err(Error::Protocol(
                "Muse returned a different session identity".into(),
            ));
        }
        if result
            .pointer("/history/noneReason")
            .is_some_and(|v| v == "projectionUnavailable")
        {
            return Err(Error::Refused(
                "Muse has no live projection for the saved session".into(),
            ));
        }
        if opts.resume.is_some() {
            self.open_rpc(
                "session/setApprovalMode",
                json!({"sessionId":id,"commandId":Uuid::now_v7(),"mode":"promptUnmatched"}),
                OpenStep::Target(id.clone()),
            )
            .await?;
        }
        if let Some(wanted) = &opts.model
            && result.pointer("/session/modelId").and_then(Value::as_str) != Some(wanted)
        {
            if opts.resume.is_none() {
                return Err(Error::Refused(
                    "Muse did not confirm requested model".into(),
                ));
            }
            self.open_rpc(
                "session/setModel",
                json!({"sessionId":id,"commandId":Uuid::now_v7(),
                "model":{"modelId":wanted,"providerId":"meta"}}),
                OpenStep::Model,
            )
            .await?;
            result["session"]["modelId"] = wanted.as_str().into();
        }
        Ok((id, result))
    }
}
