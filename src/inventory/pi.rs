use super::*;
use crate::account::AccountWire;

fn command(probe: &Probe, path: &Path) -> tokio::process::Command {
    rpc_command(probe, probe.command(path))
}

fn rpc_command(probe: &Probe, mut cmd: tokio::process::Command) -> tokio::process::Command {
    cmd.args(["--mode", "rpc", "--no-session"]);
    if let Some(dir) = probe.get("OAR_PI_AGENT_DIR").filter(|p| !p.is_empty()) {
        cmd.env("PI_CODING_AGENT_DIR", probe.cwd.join(dir));
    }
    cmd
}

async fn native(probe: &Probe, path: &Path) -> Result<Vec<Value>> {
    let mut wire = AccountWire::open(command(probe, path))?;
    let reply = wire.pi("get_commands").await;
    wire.finish().await;
    let mut reply = reply?;
    match reply["commands"].take() {
        Value::Array(commands)
            if commands
                .iter()
                .all(|r| r["name"].is_string() && r["source"].is_string()) =>
        {
            Ok(commands)
        }
        _ => Err(invalid()),
    }
}

pub(super) async fn commands(probe: &Probe, path: &Path) -> Result<(Vec<CommandEntry>, bool)> {
    let commands = native(probe, path)
        .await?
        .iter()
        .filter(|r| r["source"] != "skill")
        .map(|r| {
            Ok(CommandEntry {
                name: name(r)?,
                source: field(r, "source"),
                description: field(r, "description"),
                aliases: None,
                argument_hint: None,
            })
        })
        .collect::<Result<_>>()?;
    Ok((commands, false))
}

pub(super) async fn skills(probe: &Probe, path: &Path) -> Result<(Vec<SkillEntry>, bool)> {
    let mut items = Vec::new();
    for command in native(probe, path).await? {
        if command["source"] != "skill" {
            continue;
        }
        let info = &command["sourceInfo"];
        let name = command["name"].as_str().ok_or_else(invalid)?;
        let source = field(info, "source").ok_or_else(invalid)?;
        items.push(SkillEntry {
            name: name.strip_prefix("skill:").unwrap_or(name).to_owned(),
            description: field(&command, "description"),
            path: Some(field(info, "path").ok_or_else(invalid)?),
            scope: match info["scope"].as_str().ok_or_else(invalid)? {
                "user" => Some(SkillScope::User),
                "project" => Some(SkillScope::Project),
                "temporary" => Some(SkillScope::Other),
                _ => None,
            },
            plugin: (info["origin"] == "package").then(|| source.clone()),
            source: Some(source),
            user_invocable: Some(true),
            ..Default::default()
        });
    }
    Ok((items, false))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub(super) mod registry;
