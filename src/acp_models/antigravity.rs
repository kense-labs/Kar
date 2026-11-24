//! Antigravity returns grouped model selectors plus a menu for the current selection.
use super::{Query, rows, selector, text, thought};
use crate::{Error, Result, forms::ModelEntry, installation::Probe, process::OwnedProcess};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Stdio,
};

pub(super) async fn preflight(
    probe: &Probe,
    program: &Path,
    installed: Option<String>,
) -> Result<()> {
    let version = match installed {
        Some(v) => v,
        None => {
            let mut command = probe.command(program);
            command
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let output = OwnedProcess::spawn(command)?.output().await?;
            if output.status.success() {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            } else {
                String::new()
            }
        }
    };
    if crate::installation::read_version("antigravity", &version).is_some_and(|v| {
        v.pre.is_empty() && v.build.is_empty() && v >= semver::Version::new(1, 5, 0)
    }) {
        return Ok(());
    }
    Err(Error::Unsupported(
        "Antigravity model queries require a stable agy 1.5.0 or newer".into(),
    ))
}

const UNASSOCIATED: &str =
    "Native model entry could not be associated with a unique model and effort.";
const NO_MENU: &str = "Native catalog did not expose this model's effort menu.";
const UNCONFIRMED: &str = "Native model selection did not return a confirmed effort menu.";

fn unavailable(entry: &mut ModelEntry, reason: &str) {
    entry
        .metadata
        .insert("effortUnavailableReason".into(), reason.into());
}

fn legacy(opened: &Value, mut entries: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let mut bare = HashSet::new();
    let mut levels: HashMap<String, Vec<String>> = HashMap::new();
    let mut extra = vec![];
    for model in rows(&opened["models"]["availableModels"]) {
        let Some(id) = text(&model["modelId"]) else {
            continue;
        };
        if entries.iter().any(|e| e.id == id) {
            bare.insert(id);
            continue;
        }
        let mut matches = entries.iter().filter_map(|entry| {
            let suffix = id.strip_prefix(&entry.id)?.strip_prefix('-')?;
            (!suffix.is_empty()).then_some((entry.id.as_str(), suffix))
        });
        if let Some((family, level)) = matches.next()
            && matches.next().is_none()
        {
            let levels = levels.entry(family.to_owned()).or_default();
            if !levels.iter().any(|s| s == level) {
                levels.push(level.into())
            }
        } else if !extra.iter().any(|e: &ModelEntry| e.id == id) {
            let mut entry = ModelEntry {
                id: id.into(),
                default: false,
                metadata: Map::new(),
                disabled: None,
                effort_levels: None,
            };
            if let Some(name) = text(&model["name"]).filter(|s| *s != id) {
                entry.metadata.insert("displayName".into(), name.into());
            }
            unavailable(&mut entry, UNASSOCIATED);
            extra.push(entry);
        }
    }
    let has_efforts = !levels.is_empty() || thought(opened).is_some();
    for entry in &mut entries {
        if entry.effort_levels.is_some() {
            continue;
        }
        if let Some(levels) = levels.get(entry.id.as_str()) {
            entry.effort_levels = Some(levels.clone());
        } else if has_efforts && !bare.contains(entry.id.as_str()) {
            unavailable(entry, NO_MENU);
        }
    }
    entries.extend(extra);
    entries
}

fn native_text(value: &Value) -> Option<&str> {
    value.as_str().filter(|v| !v.is_empty())
}
fn levels(state: &Value) -> Vec<&str> {
    thought(state)
        .into_iter()
        .flat_map(|o| rows(&o["options"]))
        .filter_map(|o| native_text(&o["value"]))
        .collect()
}
fn selected(state: &Value, model: &str) -> bool {
    selector(state).is_some_and(|option| option["currentValue"] == model)
}
async fn select(query: &mut Query, session: &str, key: &str, value: &str) -> Result<Value> {
    query
        .request(
            "session/set_config_option",
            json!({"sessionId":session,"configId":key,"value":value}),
        )
        .await
}

async fn restore(
    query: &mut Query,
    session: &str,
    original: &str,
    effort: Option<&str>,
) -> Result<()> {
    let failed = || {
        Error::Protocol(
            "Antigravity could not confirm restoration of the original model and effort".into(),
        )
    };
    let model = select(query, session, "model", original)
        .await
        .map_err(|_| failed())?;
    if !selected(&model, original) {
        return Err(failed());
    }
    let Some(effort) = effort else {
        return if thought(&model).is_none() {
            Ok(())
        } else {
            Err(failed())
        };
    };
    let key = thought(&model)
        .and_then(|o| native_text(&o["id"]))
        .ok_or_else(failed)?;
    if !levels(&model).contains(&effort) {
        return Err(failed());
    }
    let confirmed = select(query, session, key, effort)
        .await
        .map_err(|_| failed())?;
    if selected(&confirmed, original)
        && thought(&confirmed).is_some_and(|o| o["currentValue"] == effort)
    {
        Ok(())
    } else {
        Err(failed())
    }
}

pub(super) async fn read(
    query: &mut Query,
    opened: &Value,
    entries: Vec<ModelEntry>,
) -> Result<Vec<ModelEntry>> {
    let fallback = legacy(opened, entries);
    let Some(model) = selector(opened) else {
        return Ok(fallback);
    };
    let ids: HashSet<_> = rows(&model["options"])
        .filter_map(|r| native_text(&r["value"]))
        .collect();
    let (Some(session), Some(original)) = (
        native_text(&opened["sessionId"]),
        native_text(&model["currentValue"]),
    ) else {
        return Ok(fallback);
    };
    let thinking = thought(opened);
    let effort = thinking.and_then(|r| native_text(&r["currentValue"]));
    if !ids.contains(original)
        || thinking.is_some_and(|r| {
            native_text(&r["id"]).is_none() || !effort.is_some_and(|e| levels(opened).contains(&e))
        })
    {
        return Ok(fallback);
    }

    let mut result = Vec::with_capacity(fallback.len());
    let mut attempted = false;
    for entry in &fallback {
        let mut entry = entry.clone();
        if ids.contains(entry.id.as_str()) {
            let first = !attempted;
            attempted = true;
            entry.effort_levels = None;
            entry.metadata.remove("defaultEffort");
            entry.metadata.remove("effortUnavailableReason");
            match select(query, session, "model", &entry.id).await {
                Err(Error::ProviderRpc { code: -32601, .. }) if first => return Ok(fallback),
                Ok(state)
                    if selected(&state, &entry.id)
                        && (thought(&state).is_none() || !levels(&state).is_empty()) =>
                {
                    let menu = levels(&state);
                    if let Some(effort) =
                        effort.filter(|e| entry.id == original && menu.contains(e))
                    {
                        entry.metadata.insert("defaultEffort".into(), effort.into());
                    }
                    if !menu.is_empty() {
                        entry.effort_levels = Some(menu.into_iter().map(str::to_owned).collect());
                    }
                }
                _ => unavailable(&mut entry, UNCONFIRMED),
            }
        }
        result.push(entry);
    }
    if attempted {
        restore(query, session, original, effort).await?;
    }
    Ok(result)
}
