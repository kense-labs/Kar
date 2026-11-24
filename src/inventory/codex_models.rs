//! Read the app-server picker without creating a native thread.
use super::rpc;
use crate::{Error, Result, Runtime, SessionOptions, account::AccountWire, forms::ModelEntry};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(crate) async fn read(rt: &Runtime, options: &SessionOptions, picker: bool) -> Result<Value> {
    let cwd = options.cwd.canonicalize()?;
    let mut command = options.command(&rt.program)?;
    command
        .args(&rt.args)
        .args(&options.launch_args)
        .args(["--listen", "stdio://"]);
    let mut wire = AccountWire::open(command)?;
    let read = async {
        wire.initialize_codex().await?;
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut params = json!({});
        let mut request = 2;
        loop {
            let mut page = rpc(
                wire.codex(request, "model/list", params).await?,
                "model/list",
            )?;
            let values = page
                .get_mut("data")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| Error::Protocol("Codex model/list omitted its data array".into()))?;
            rows.append(values);
            request += 1;
            let Some(next) = text(&page["nextCursor"]) else {
                break;
            };
            if !seen.insert(next.to_owned()) {
                return Err(Error::Protocol("Codex model/list repeated a cursor".into()));
            }
            params = json!({"cursor":next});
        }
        if !picker {
            return Ok(Value::Array(rows));
        }
        let config = rpc(
            wire.codex(
                request,
                "config/read",
                json!({"cwd":cwd,"includeLayers":false}),
            )
            .await?,
            "config/read",
        )?;
        let configured = text(&config["config"]["model"]);
        let models: Vec<_> = rows
            .iter()
            .filter_map(|row| entry(row, configured))
            .collect();
        Ok(serde_json::to_value(models)?)
    };
    let result = tokio::time::timeout(options.handshake_timeout, read).await;
    match result {
        Ok(value) => {
            wire.finish().await;
            value
        }
        Err(_) => {
            wire.close().await;
            Err(Error::Timeout("Codex model listing".into()))
        }
    }
}

fn text(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn entry(row: &Value, configured: Option<&str>) -> Option<ModelEntry> {
    let id = text(&row["model"])?;
    if row["hidden"] == true {
        return None;
    }
    let mut metadata = Map::new();
    for (native, key) in [
        ("displayName", "displayName"),
        ("defaultReasoningEffort", "defaultEffort"),
        ("defaultServiceTier", "defaultServiceTier"),
    ] {
        if let Some(value) = text(&row[native]) {
            metadata.insert(key.into(), value.into());
        }
    }
    if let Some(tiers) = row["serviceTiers"].as_array() {
        let mut ids = Vec::new();
        let mut details = Vec::new();
        for tier in tiers {
            let Some(id) = text(&tier["id"]) else {
                continue;
            };
            if ids.contains(&id) {
                continue;
            }
            ids.push(id);
            let mut detail = json!({"id":id});
            if let Some(label) = text(&tier["name"]) {
                detail["displayName"] = label.into();
            }
            if let Some(description) = text(&tier["description"]) {
                detail["description"] = description.into();
            }
            details.push(detail);
        }
        metadata.insert("serviceTiers".into(), json!(ids));
        metadata.insert("serviceTierDetails".into(), Value::Array(details));
    }
    let effort_levels = row["supportedReasoningEfforts"].as_array().map(|levels| {
        levels
            .iter()
            .filter_map(|v| v["reasoningEffort"].as_str().map(str::to_owned))
            .collect()
    });
    Some(ModelEntry {
        id: id.into(),
        default: configured.map_or(row["isDefault"] == true, |c| c == id),
        effort_levels,
        disabled: None,
        metadata,
    })
}
