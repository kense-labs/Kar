use super::*;
use crate::account::AccountWire;
use serde_json::json;
pub(super) mod models;
mod skills;
async fn request(wire: &mut AccountWire, args: Value) -> Result<Value> {
    let mut reply = wire.control_reply(args).await?;
    match reply["subtype"].as_str() {
        Some("success") => Ok(reply["response"].take()),
        Some("error")
            if reply["error"].as_str().is_some_and(|s| {
                let msg = s.to_ascii_lowercase();
                ["unsupported", "not supported", "unknown control"]
                    .iter()
                    .any(|p| msg.contains(p))
            }) =>
        {
            Err(Error::Unsupported("Claude inventory control".into()))
        }
        _ => Err(invalid()),
    }
}
async fn read<T>(
    probe: &Probe,
    path: &Path,
    query: impl AsyncFnOnce(&mut AccountWire, Value) -> Result<(Vec<T>, bool)>,
) -> Result<(Vec<T>, bool)> {
    let mut cmd = probe.command(path);
    crate::options::claude_environment(&mut cmd, &Default::default());
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
    ]);
    let mut wire = AccountWire::open(cmd)?;
    let data = async {
        let initial = request(&mut wire, json!({"subtype":"initialize"})).await?;
        query(&mut wire, initial).await
    }
    .await;
    wire.finish().await;
    data
}
pub(super) async fn commands(probe: &Probe, path: &Path) -> Result<(Vec<CommandEntry>, bool)> {
    read(probe, path, async |_, v| {
        let entries = rows(&v["commands"])?
            .iter()
            .map(|r| {
                let aliases = r
                    .get("aliases")
                    .map(|v| serde_json::from_value(v.clone()))
                    .transpose()?;
                Ok(CommandEntry {
                    name: name(r)?,
                    description: field(r, "description"),
                    source: (r["builtin"] == true).then(|| "builtin".into()),
                    argument_hint: field(r, "argumentHint").filter(|s| !s.is_empty()),
                    aliases,
                })
            })
            .collect::<Result<_>>()?;
        Ok((entries, false))
    })
    .await
}
pub(super) async fn skills(probe: &Probe, path: &Path) -> Result<(Vec<SkillEntry>, bool)> {
    read(probe, path, async |wire, init| {
        let data = request(
            wire,
            json!({"subtype":"get_context_usage","detail":"summary"}),
        )
        .await?;
        skills::catalog(&data, &init, &probe.cwd).await
    })
    .await
}
async fn mcp(wire: &mut AccountWire) -> Result<(Vec<Value>, bool)> {
    for attempt in 0..=10 {
        let mut reply = request(wire, json!({"subtype":"mcp_status"})).await?;
        let servers = reply["mcpServers"].as_array_mut().ok_or_else(invalid)?;
        if attempt == 10 || !servers.iter().any(|v| v["status"] == "pending") {
            let partial = servers
                .iter()
                .any(|v| matches!(v["status"].as_str(), Some("pending" | "failed")));
            return Ok((std::mem::take(servers), partial));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    unreachable!()
}
pub(super) async fn servers(probe: &Probe, path: &Path) -> Result<(Vec<McpServerEntry>, bool)> {
    read(probe, path, async |wire, _| {
        let (servers, partial) = mcp(wire).await?;
        let entries = servers
            .iter()
            .map(|r| {
                let id = name(r)?;
                Ok(McpServerEntry {
                    id: id.clone(),
                    name: id,
                    status: field(r, "status"),
                    source: field(r, "scope"),
                    tool_count: r["tools"].as_array().map(Vec::len),
                    error: (!r["error"].is_null()).then(|| "Native MCP connection failed".into()),
                    ..Default::default()
                })
            })
            .collect::<Result<_>>()?;
        Ok((entries, partial))
    })
    .await
}
pub(super) async fn tools(probe: &Probe, path: &Path) -> Result<(Vec<ToolEntry>, bool)> {
    read(probe, path, async |wire, _| {
        let (servers, partial) = mcp(wire).await?;
        let mut items = vec![];
        for server in servers {
            let Some(tools) = server.get("tools") else {
                continue;
            };
            for r in rows(tools)? {
                items.push(ToolEntry {
                    name: name(r)?,
                    mcp_server_id: Some(name(&server)?),
                    source: Some("mcp".into()),
                    description: field(r, "description"),
                    ..Default::default()
                });
            }
        }
        Ok((items, partial))
    })
    .await
}
