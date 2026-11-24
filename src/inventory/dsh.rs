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
            let running = owner.insert(launch(probe, path, json!({"cwd":probe.cwd}))?);
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
fn launch(probe: &Probe, executable: &Path, scope: Value) -> Result<Running> {
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
            {"insert":[{"id":"kar-rust-skills","name":uri.as_str(),"config":scope}]}
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
                    "DSH does not expose the requested inventory method".into(),
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

pub(crate) async fn model_choices(
    rt: &Runtime,
    opts: &crate::SessionOptions,
) -> Result<Vec<crate::forms::ModelEntry>> {
    let runtime = rt.selected(opts).await?;
    let version = crate::dsh::release(&runtime, opts, None).await?;
    if version < semver::Version::parse("0.2.1-rc.5").unwrap() {
        return Err(Error::Unsupported(
            "DSH model listing requires 0.2.1-rc.5 or later".into(),
        ));
    }
    let probe = Probe::for_session(opts)?;
    let mut lock = Some(crate::dsh::profile::lock(&probe, "acp").await?);
    let mut run = launch(
        &probe,
        &runtime.program,
        json!({"cwd":probe.cwd,"catalog":"models"}),
    )?;
    let snapshot = read(&mut run.process, &mut lock).await;
    run.process.stop().await;
    project_models(&snapshot?)
}

fn project_models(catalog: &Value) -> Result<Vec<crate::forms::ModelEntry>> {
    use crate::{forms::ModelEntry, providers::ProviderErrorKind};
    let mut entries = vec![];
    let providers = rows(&catalog["providers"])?;
    let default = &catalog["default"];
    for p in providers {
        let Some(provider) = p["id"].as_str() else {
            continue;
        };
        for model in p["models"].as_array().into_iter().flatten() {
            let Some(id) = model["id"].as_str() else {
                continue;
            };
            let mut metadata = serde_json::Map::new();
            let mut source = json!({"id":provider});
            if let Some(name) = p["name"].as_str() {
                source["displayName"] = name.into();
            }
            metadata.insert("provider".into(), source);
            if let Some(name) = model["name"].as_str() {
                metadata.insert("displayName".into(), name.into());
            }
            let reasoning = &model["reasoning"];
            let efforts = reasoning.as_object().map(|_| {
                reasoning["efforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v["id"].as_str().map(str::to_owned))
                    .collect()
            });
            if let Some(level) = reasoning["defaultEffort"].as_str() {
                metadata.insert("defaultEffort".into(), level.into());
            }
            entries.push(ModelEntry {
                id: format!("{provider}/{id}"),
                default: default["provider"] == provider && default["model"] == id,
                effort_levels: efforts,
                disabled: None,
                metadata,
            });
        }
    }
    if entries.is_empty() {
        if default["provider"].as_str().is_some_and(|id| {
            !id.is_empty()
                && providers.iter().any(|p| {
                    p["id"] == id
                        && !p["failure"].is_string()
                        && p["models"].as_array().is_none_or(Vec::is_empty)
                })
        }) {
            return Err(ProviderErrorKind::AuthenticationFailed
                .error("DSH's default provider advertises no credentialed models"));
        }
        if providers.iter().any(|p| p["failure"].is_string()) {
            return Err(Error::Unsupported(
                "DSH could not list any provider's models".into(),
            ));
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod model_tests {
    use super::*;
    #[test]
    fn picker_distinguishes_failed_unconfigured_and_empty_default() {
        let source = json!({"default":{"provider":"deep","model":"a"},"providers":[{"id":"deep","name":"Deep","models":[{"id":"a","name":"A","reasoning":{"efforts":[{"id":"high"}],"defaultEffort":"high"}}]}]});
        let row = &project_models(&source).unwrap()[0];
        assert!(row.default);
        assert_eq!(row.id, "deep/a");
        assert_eq!(row.effort_levels.as_ref().unwrap(), &["high"]);
        assert_eq!(row.metadata["provider"]["displayName"], "Deep");
        let cases = [
            json!({"default":{"provider":"p"},"providers":[{"id":"p","models":[]}]}),
            json!({"default":{"provider":"p"},"providers":[{"id":"p","failure":"failed"}]}),
            json!({"default":{"provider":"other"},"providers":[{"id":"p","models":[]}]}),
        ];
        assert!(
            matches!(project_models(&cases[0]),Err(Error::Provider(error)) if error.kind==crate::providers::ProviderErrorKind::AuthenticationFailed)
        );
        assert!(matches!(
            project_models(&cases[1]),
            Err(Error::Unsupported(_))
        ));
        assert!(project_models(&cases[2]).unwrap().is_empty());
    }
}
