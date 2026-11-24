use super::{
    InventoryOptions, InventoryResult, Probe, SkillEntry, SkillScope, View, field, invalid, rows,
};
use crate::{Error, Result, Runtime, process::OwnedProcess};
use serde_json::{Value, json};
use std::{ffi::OsString, fs::File, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const PREFIX: &str = "KAR-DSH-SKILLS ";

pub(super) async fn skills(
    runtime: &Runtime,
    opts: &InventoryOptions,
) -> Result<InventoryResult<SkillEntry>> {
    let mut owner = None;
    let result = runtime
        .query_workspace(opts, true, async |probe, path, _, _| {
            let mut lock = Some(crate::dsh::profile::lock(probe, "acp").await?);
            let running = owner.insert(launch(probe, path)?);
            let snapshot = read(&mut running.process, &mut lock).await?;
            let complete = snapshot["complete"].as_bool().ok_or_else(invalid)?;
            let items = rows(&snapshot["skills"])?
                .iter()
                .map(project)
                .collect::<Result<_>>()?;
            Ok(InventoryResult::observed(
                &probe.cwd,
                items,
                !complete,
                View::Discovered,
            ))
        })
        .await;
    // The inventory deadline may have expired inside a provider callback. Reap
    // the whole owned process before returning or deleting its loaded library.
    if let Some(mut running) = owner {
        running.process.stop().await;
    }
    result
}

fn project(skill: &Value) -> Result<SkillEntry> {
    use SkillScope::*;
    let source = field(skill, "source");
    let scope = match source.as_deref() {
        Some("bundled") => Some(Builtin),
        Some("project-dsh" | "project-agents") => Some(Project),
        Some("user-dsh" | "user-agents") => Some(User),
        Some("custom" | "runtime") => Some(Other),
        _ => None,
    };
    Ok(SkillEntry {
        name: field(skill, "name").ok_or_else(invalid)?,
        description: field(skill, "description"),
        path: field(skill, "path"),
        source,
        scope,
        user_invocable: skill["invocation"]["userInvocable"].as_bool(),
        disable_model_invocation: skill["invocation"]["modelInvocable"].as_bool().map(|v| !v),
        ..Default::default()
    })
}

struct Running {
    process: OwnedProcess,
    _directory: tempfile::TempDir,
}
fn launch(probe: &Probe, executable: &Path) -> Result<Running> {
    let directory = tempfile::tempdir()?;
    let plugin = directory.path().join("skills.node");
    std::fs::write(
        &plugin,
        include_bytes!(concat!(env!("OUT_DIR"), "/dsh-skills.node")),
    )?;
    let uri = url::Url::from_file_path(plugin).map_err(|_| invalid())?;
    let patch = directory.path().join("skills.json");
    std::fs::write(
        &patch,
        serde_json::to_vec(&json!([
            {"id":"acp","disabled":true},
            {"insert":[{"id":"kar-rust-skills","name":uri.as_str(),"config":{"cwd":probe.cwd}}]}
        ]))?,
    )?;
    let mut options = probe
        .get("NODE_OPTIONS")
        .map(OsString::from)
        .unwrap_or_default();
    if options
        .to_string_lossy()
        .split_whitespace()
        .any(|s| s == "--no-addons")
    {
        return Err(Error::Unsupported(
            "DSH's Node host has disabled native addons".into(),
        ));
    }
    options.push(" --experimental-addon-modules");
    let mut command = probe.command(executable);
    command
        .args(["--profile", "acp", "--patch"])
        .arg(patch)
        .env("NODE_OPTIONS", options)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(Running {
        process: OwnedProcess::spawn(command)?,
        _directory: directory,
    })
}
async fn read(process: &mut OwnedProcess, lock: &mut Option<File>) -> Result<Value> {
    let child = process.inner();
    let mut out = BufReader::new(
        child
            .stdout
            .take()
            .expect("DSH stdout configured")
            .take(9 * 1024 * 1024),
    )
    .lines();
    let mut err = BufReader::new(
        child
            .stderr
            .take()
            .expect("DSH stderr configured")
            .take(1024 * 1024),
    )
    .lines();
    let mut has_stderr = true;
    loop {
        let line = tokio::select! {
            s=out.next_line()=>s?.ok_or_else(invalid)?,
            s=err.next_line(), if has_stderr => {
                let Some(line)=s? else {has_stderr=false;continue};
                if addon_unavailable(&line) { return Err(Error::Unsupported("DSH cannot load the native skills plugin".into())); }
                continue
            }
        };
        if addon_unavailable(&line) {
            return Err(Error::Unsupported(
                "DSH cannot load the native skills plugin".into(),
            ));
        }
        let Some(json) = line.strip_prefix(PREFIX) else {
            continue;
        };
        if json == "loaded" {
            lock.take();
            continue;
        }
        let snapshot: Value = serde_json::from_str(json)?;
        match snapshot["error"].as_str() {
            Some("native_query_unavailable") => {
                return Err(Error::Unsupported(
                    "DSH does not expose skills.snapshot".into(),
                ));
            }
            Some(_) => return Err(invalid()),
            None => return Ok(snapshot),
        }
    }
}

fn addon_unavailable(line: &str) -> bool {
    line.contains("ERR_DLOPEN_FAILED")
        || (line.contains("skills.node")
            && (line.contains("code signature") || line.contains("Unknown file extension")))
        || line.contains("--experimental-addon-modules is not allowed")
}
