use super::*;
use crate::account::AccountWire;
use crate::forms::{ListingSource, ModelEntry, ModelListing};
use std::{collections::HashSet, time::Duration};

pub(crate) async fn list(rt: &Runtime, opts: &SessionOptions) -> Result<Vec<ModelEntry>> {
    let _guard = pin_process(rt)?;
    let mut cmd = opts.command(&rt.program)?;
    cmd.args(&rt.args);
    let mut wire = AccountWire::open(cmd)?;
    let hello = wire.initialize(AccountWire::capabilities()).await?;
    let mut update = None;
    wire.send(json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":opts.cwd,"mcpServers":[]}})).await?;
    let mut opened = loop {
        let mut frame = wire.next().await?;
        if frame["id"] == 2 && frame.get("method").is_none() && frame["jsonrpc"] == "2.0" {
            break result(&mut frame, "session/new")?;
        }
        observe(&mut wire, frame, &mut update).await?;
    };
    let id = opened["sessionId"]
        .as_str()
        .ok_or_else(|| Error::Protocol("ACP model discovery has no session ID".into()))?
        .to_owned();
    let apply = |opened: &mut Value, update: &Option<(String, Value)>| {
        if let Some((session, opts)) = update
            && *session == id
        {
            opened["configOptions"] = opts.clone();
        }
    };
    apply(&mut opened, &update);
    if entries(&opened).is_empty() {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let f = wire.next().await?;
                observe(&mut wire, f, &mut update).await?;
                apply(&mut opened, &update);
                if !entries(&opened).is_empty() {
                    return Ok::<_, Error>(());
                }
            }
        })
        .await
        .unwrap_or(Ok(()))?;
    }
    let caps = &hello["agentCapabilities"]["sessionCapabilities"];
    if let Some(method) = ["delete", "close"]
        .into_iter()
        .find(|k| caps[k].is_object() || caps[k] == true)
    {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            wire.rpc(3, &format!("session/{method}"), json!({"sessionId":id})),
        )
        .await;
    }
    Ok(entries(&opened))
}

pub(super) fn result(frame: &mut Value, method: &'static str) -> Result<Value> {
    if let Some(e) = frame.get("error").filter(|v| !v.is_null()) {
        let code = e["code"].as_i64().unwrap_or(0);
        if code == -32000 {
            return Err(crate::OpenError::RuntimeFailure {
                failure: crate::TurnFailure::new(
                    crate::FailureClass::Auth,
                    "The ACP agent requires authentication",
                ),
                code: None,
                cause: None,
            }
            .into());
        }
        return Err(Error::ProviderRpc { method, code });
    }
    if !frame["result"].is_object() {
        return Err(Error::Protocol(format!("ACP omitted {method} result")));
    }
    Ok(frame["result"].take())
}

async fn observe(
    wire: &mut AccountWire,
    mut f: Value,
    update: &mut Option<(String, Value)>,
) -> Result<()> {
    if f["jsonrpc"] != "2.0" {
        return Ok(());
    }
    if f["method"] == "session/update"
        && f["params"]["update"]["sessionUpdate"] == "config_option_update"
        && let Some(id) = f["params"]["sessionId"].as_str().map(str::to_owned)
        && f["params"]["update"]["configOptions"].is_array()
    {
        *update = Some((id, f["params"]["update"]["configOptions"].take()));
    } else if f["method"].is_string() && !f["id"].is_null() {
        wire.send(json!({"jsonrpc":"2.0","id":f["id"],"error":{"code":-32601,"message":"Unavailable during model discovery"}})).await?;
    }
    Ok(())
}

fn choices(option: &Value) -> Vec<&Value> {
    let mut rows = Vec::new();
    for choice in option["options"].as_array().into_iter().flatten() {
        if let Some(group) = choice["options"].as_array() {
            rows.extend(group);
        } else {
            rows.push(choice);
        }
    }
    rows
}
fn id(v: &Value) -> Option<&str> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.trim() == *s && s.len() <= 256)
}
fn entries(opened: &Value) -> Vec<ModelEntry> {
    let option = crate::options::acp_model_option(opened);
    let (list, key, current) = match option {
        Some(o) => (choices(o), "value", id(&o["currentValue"])),
        None => (
            opened["models"]["availableModels"]
                .as_array()
                .into_iter()
                .flatten()
                .collect(),
            "modelId",
            id(&opened["models"]["currentModelId"]),
        ),
    };
    let effort = crate::options::acp_effort_option(opened, false);
    let mut seen = HashSet::new();
    let mut models = vec![];
    for row in list {
        let Some(model) = id(&row[key]).filter(|s| seen.insert(*s)) else {
            continue;
        };
        let default = Some(model) == current;
        let mut metadata = serde_json::Map::new();
        for (native, field, max) in [
            ("name", "displayName", 160),
            ("description", "description", 1024),
        ] {
            if let Some(s) = row[native]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                metadata.insert(field.into(), s.chars().take(max).collect::<String>().into());
            }
        }
        let native = &row["_meta"];
        let mut levels = vec![];
        if native["supportsReasoningEffort"] != false
            && let Some(e) = effort
        {
            if let Some(per_model) = native["reasoningEfforts"].as_array() {
                levels.extend(
                    per_model
                        .iter()
                        .filter_map(|v| id(&v["value"]).or_else(|| id(v)))
                        .map(str::to_owned),
                );
            } else if default {
                levels.extend(
                    choices(e)
                        .into_iter()
                        .filter_map(|v| id(&v["value"]))
                        .map(str::to_owned),
                );
            }
            if default
                && let Some(s) = id(&e["currentValue"])
                && levels.iter().any(|l| l == s)
            {
                metadata.insert("defaultEffort".into(), s.into());
            }
        }
        let entry = ModelEntry {
            id: model.into(),
            default,
            metadata,
            disabled: None,
            effort_levels: (!levels.is_empty()).then_some(levels),
        };
        if models.len() < 256 {
            models.push(entry);
        } else if default {
            models[255] = entry;
            break;
        }
    }
    models
}

pub(crate) fn custom(store: &ChatStore, runtime: &str) -> Result<Vec<ModelEntry>> {
    if runtime.starts_with("acp-profile:") {
        Ok(profiles::load(store, runtime)?.custom_models)
    } else {
        Ok(vec![])
    }
}
pub(crate) fn merge(list: ModelListing, mut custom: Vec<ModelEntry>) -> ModelListing {
    if custom.is_empty() {
        return list;
    }
    for m in &mut custom {
        m.metadata.insert("isCustom".into(), true.into());
    }
    match list {
        ModelListing::Live { mut models } => {
            let mut ids: HashSet<_> = models.iter().map(|m| m.id.clone()).collect();
            models.extend(custom.into_iter().filter(|m| ids.insert(m.id.clone())));
            ModelListing::Live { models }
        }
        ModelListing::Unavailable { reason } | ModelListing::Fallback { reason, .. } => {
            ModelListing::Fallback {
                source: ListingSource::Configured,
                reason,
                models: custom,
            }
        }
    }
}
