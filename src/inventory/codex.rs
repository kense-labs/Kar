//! Codex app-server inventories, without thread/start or a model turn.
use super::*;
use crate::account::AccountWire;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use tokio::process::Command;

pub(super) async fn connect(mut cmd: Command) -> Result<AccountWire> {
    cmd.args(["app-server", "--listen", "stdio://"]);
    let mut wire = AccountWire::open(cmd)?;
    wire.initialize_codex().await?;
    Ok(wire)
}
fn invalid() -> Error {
    Error::Protocol("Invalid Codex MCP inventory".into())
}
fn name(row: &Value) -> Result<&str> {
    row["name"].as_str().ok_or_else(invalid)
}
async fn read<T>(
    probe: &Probe,
    path: &Path,
    project: impl FnMut(&Value, &mut Vec<T>) -> Result<()>,
) -> Result<(Vec<T>, bool)> {
    let mut wire = connect(probe.command(path)).await?;
    let result = pages(&mut wire, project).await;
    wire.finish().await;
    result
}
async fn pages<T>(
    wire: &mut AccountWire,
    mut project: impl FnMut(&Value, &mut Vec<T>) -> Result<()>,
) -> Result<(Vec<T>, bool)> {
    const METHOD: &str = "mcpServerStatus/list";
    let (mut cursor, mut seen) = (Value::Null, HashSet::new());
    let (mut items, mut partial) = (Vec::new(), false);
    loop {
        let call = wire
            .codex(
                2 + seen.len() as u64,
                METHOD,
                json!({"detail":"full","limit":100,"cursor":cursor}),
            )
            .await?;
        let mut page = rpc(call, METHOD)?;
        let servers = page["data"].as_array().ok_or_else(invalid)?;
        for server in servers {
            if !server.is_object() {
                return Err(invalid());
            }
            partial |= !server["toolsError"].is_null();
            project(server, &mut items)?;
        }
        match page.get_mut("nextCursor") {
            Some(Value::Null) => return Ok((items, partial)),
            Some(next)
                if seen.len() < 100 && next.as_str().is_some_and(|s| seen.insert(s.to_owned())) =>
            {
                cursor = next.take()
            }
            _ => return Err(invalid()),
        }
    }
}
pub(super) async fn servers(probe: &Probe, path: &Path) -> Result<(Vec<McpServerEntry>, bool)> {
    read(probe, path, |v, out| {
        let id = name(v)?.to_owned();
        out.push(McpServerEntry {
            id: id.clone(),
            name: id,
            status: v["runtimeStatus"].as_str().map(str::to_owned),
            auth_status: v["authStatus"].as_str().map(str::to_owned),
            tool_count: v["tools"].as_object().map(Map::len),
            error: (!v["toolsError"].is_null()).then(|| "Native tool discovery failed".into()),
            ..Default::default()
        });
        Ok(())
    })
    .await
}
pub(super) async fn tools(probe: &Probe, path: &Path) -> Result<(Vec<ToolEntry>, bool)> {
    read(probe, path, |server, out| {
        if let Some(tools) = server["tools"].as_object() {
            for v in tools.values() {
                out.push(ToolEntry {
                    name: name(v)?.into(),
                    description: v["description"].as_str().map(str::to_owned),
                    source: Some("mcp".into()),
                    mcp_server_id: Some(name(server)?.into()),
                    input_schema: v["inputSchema"].as_object().cloned(),
                    ..Default::default()
                });
            }
        }
        Ok(())
    })
    .await
}
