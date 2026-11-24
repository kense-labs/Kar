use super::*;
use crate::{
    account::{self, AuthStatus},
    process::OwnedProcess,
};
use std::process::Stdio;
mod files;
mod manifests;

pub(super) async fn skills(
    probe: &Probe,
    program: &Path,
    version: Option<&str>,
    _: Instant,
) -> Result<InventoryResult<SkillEntry>> {
    if !account::default_gemini_directory(probe) {
        return Ok(InventoryResult::unsupported(
            UnsupportedCode::ScopeUnavailable,
            "Antigravity print mode cannot select GEMINI_HOME; set HOME or USERPROFILE to select this profile",
        ));
    }
    if cfg!(any(target_os = "macos", target_os = "linux"))
        && matches!(version, Some("1.5.10" | "1.5.11"))
    {
        let home = account::profile_home(probe).ok_or_else(invalid)?;
        let items = files::discover(&probe.cwd, &home).await?;
        return Ok(InventoryResult::observed(
            &probe.cwd,
            items,
            true,
            View::Discovered,
        ));
    }
    match account::antigravity_auth_status(probe, program).await? {
        AuthStatus::LoggedIn { .. } => (),
        AuthStatus::LoggedOut { .. } => {
            return Ok(InventoryResult::unavailable(
                UnavailableCode::NotAuthenticated,
                "Sign in to Antigravity to read its skills",
            ));
        }
        _ => return Err(invalid()),
    }
    let mut cmd = probe.command(program);
    cmd.args(["--output-format", "json", "--print=/skills"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = OwnedProcess::spawn(cmd)?.output().await?;
    if !out.status.success() {
        return Err(invalid());
    }
    let frame: Value = serde_json::from_slice(&out.stdout)?;
    if frame["status"] != "SUCCESS"
        || frame["conversation_id"] != ""
        || frame["num_turns"] != 0
        || frame["command"]["name"] != "skills"
    {
        return Err(invalid());
    }
    let mut items = Vec::new();
    for row in rows(&frame["command"]["data"]["skills"])? {
        let flag = |key: &str| {
            row.get(key)
                .map(|v| v.as_bool().ok_or_else(invalid))
                .transpose()
        };
        items.push(SkillEntry {
            name: name(row)?,
            description: field(row, "description"),
            path: field(row, "path"),
            plugin: field(row, "plugin"),
            user_invocable: Some(true),
            disable_model_invocation: flag("model_invocable")?.map(|v| !v),
            scope: flag("builtin")?.filter(|v| *v).map(|_| SkillScope::Builtin),
            ..Default::default()
        });
    }
    Ok(InventoryResult::observed(
        &probe.cwd,
        items,
        false,
        View::Discovered,
    ))
}

#[cfg(test)]
mod tests;
