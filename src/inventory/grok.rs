use super::*;
use crate::process::OwnedProcess;
use std::process::Stdio;

async fn inspect(probe: &Probe, path: &Path) -> Result<Value> {
    let mut cmd = probe.command(path);
    cmd.args(["inspect", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = OwnedProcess::spawn(cmd)?.output().await?;
    if !out.status.success() {
        return Err(invalid());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}
pub(super) async fn skills(probe: &Probe, path: &Path) -> Result<(Vec<SkillEntry>, bool)> {
    let data = inspect(probe, path).await?;
    let items = rows(&data["skills"])?
        .iter()
        .map(|r| {
            let raw = &r["source"];
            let source = raw
                .as_str()
                .map(str::to_owned)
                .or_else(|| field(raw, "type"));
            let scope = match source.as_deref() {
                Some("bundled") => Some(SkillScope::Builtin),
                Some("user") => Some(SkillScope::User),
                Some("project") => Some(SkillScope::Project),
                Some("server" | "plugin" | "configToml") => Some(SkillScope::Other),
                _ => None,
            };
            Ok(SkillEntry {
                name: name(r)?,
                description: field(r, "description"),
                source,
                scope,
                plugin: field(raw, "plugin_name"),
                path: field(raw, "path"),
                user_invocable: r["userInvocable"].as_bool(),
                ..Default::default()
            })
        })
        .collect::<Result<_>>()?;
    Ok((items, false))
}
pub(super) async fn servers(probe: &Probe, path: &Path) -> Result<(Vec<McpServerEntry>, bool)> {
    let data = inspect(probe, path).await?;
    let items = rows(&data["mcpServers"])?
        .iter()
        .map(|r| {
            let id = name(r)?;
            Ok(McpServerEntry {
                id: id.clone(),
                name: id,
                source: field(r, "source").or_else(|| field(&r["source"], "type")),
                ..Default::default()
            })
        })
        .collect::<Result<_>>()?;
    Ok((items, false))
}
