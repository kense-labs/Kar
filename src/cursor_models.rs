//! Cursor ACP exposes base model IDs and their parameters separately from legacy variant IDs.
use crate::{Error, Protocol, Result, Runtime, SessionOptions};
use serde_json::{Value, json};
use std::collections::HashSet;

pub(crate) const LIST: &str = "cursor/list_available_models";

pub(crate) fn capabilities(parameterized: bool) -> Value {
    json!({"_meta":{"subagents":true,"parameterizedModelPicker":parameterized}})
}

pub(crate) fn effort(info: &Value) -> Option<&Value> {
    let options = info["configOptions"].as_array()?;
    ["effort", "reasoning", "reasoning_effort", "thinking"]
        .iter()
        .find_map(|id| {
            options
                .iter()
                .find(|option| option["id"] == *id && option["type"] == "select")
        })
}

pub(crate) fn fast(info: &Value) -> Option<&Value> {
    info["configOptions"].as_array()?.iter().find(|option| {
        option["id"] == "fast" && matches!(option["type"].as_str(), Some("select" | "boolean"))
    })
}

pub(crate) fn fast_flag(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| match value.as_str()? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    })
}

pub(crate) fn fast_supports(option: &Value, wanted: bool) -> bool {
    if option["type"] == "boolean" {
        return true;
    }
    match option["options"].as_array() {
        Some(items) => items
            .iter()
            .any(|item| fast_flag(&item["value"]) == Some(wanted)),
        None => true,
    }
}

pub(crate) fn project(result: &Value) -> Result<Value> {
    let invalid =
        || Error::Protocol("Cursor returned an invalid parameterized model catalog".into());
    let models = result["models"].as_array().ok_or_else(invalid)?;
    let mut ids = HashSet::new();
    let mut entries = Vec::with_capacity(models.len());
    for model in models {
        let id = model["value"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        if !ids.insert(id) || !model["configOptions"].is_array() {
            return Err(invalid());
        }
        let mut entry = model.clone();
        entry["id"] = id.into();
        entry["modelId"] = id.into();
        if let Some(label) = model["name"].as_str().filter(|s| !s.is_empty() && *s != id) {
            entry["displayName"] = label.into();
        }
        if let Some(choice) = effort(model) {
            let levels: Vec<&str> = choice["options"]
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(|item| item["value"].as_str().ok_or_else(invalid))
                .collect::<Result<_>>()?;
            if let Some(value) = choice["currentValue"]
                .as_str()
                .filter(|s| levels.contains(s))
            {
                entry["defaultEffort"] = value.into();
            }
            entry["effortLevels"] = json!(levels);
        }
        if let Some(choice) = fast(model) {
            let supports_fast = if choice["type"] == "boolean" {
                true
            } else {
                let items = choice["options"].as_array().ok_or_else(invalid)?;
                let mut seen_true = false;
                for item in items {
                    if fast_flag(&item["value"]).is_none() {
                        return Err(invalid());
                    }
                    seen_true |= fast_flag(&item["value"]) == Some(true);
                }
                seen_true
            };
            if supports_fast {
                entry["serviceTiers"] = json!(["fast"]);
                if fast_flag(&choice["currentValue"]) == Some(true) {
                    entry["defaultServiceTier"] = "fast".into();
                }
            }
        } else if let Some(param) = model["parameters"]
            .as_array()
            .and_then(|params| params.iter().find(|p| p["id"] == "fast"))
        {
            let supports_fast = param["values"]
                .as_array()
                .is_some_and(|vals| vals.iter().any(|v| fast_flag(&v["value"]) == Some(true)));
            if supports_fast {
                entry["serviceTiers"] = json!(["fast"]);
                let default_fast = model["variants"]
                    .as_array()
                    .and_then(|vars| vars.iter().find(|v| v["isDefault"] == true))
                    .and_then(|v| v["params"].as_array())
                    .is_some_and(|params| {
                        params
                            .iter()
                            .any(|p| p["id"] == "fast" && fast_flag(&p["value"]) == Some(true))
                    });
                if default_fast {
                    entry["defaultServiceTier"] = "fast".into();
                }
            }
        }
        entries.push(entry);
    }
    Ok(Value::Array(entries))
}

/// Read the vendor extension before opening any native session. None means an older ACP peer.
pub(crate) async fn catalog(rt: &Runtime, opts: &SessionOptions) -> Result<Option<Value>> {
    let mut cmd = opts.command(&rt.program)?;
    cmd.args(&rt.args).current_dir(&opts.cwd);
    let mut wire = crate::account::AccountWire::open(cmd)?;
    let secrets = crate::options::session_secrets(opts);
    let read = async {
        let hello = json!({"protocolVersion":1,"clientInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")},
            "clientCapabilities":capabilities(true)});
        let mut initialized = wire.rpc(1, "initialize", hello).await?;
        crate::transport::redact(&mut initialized, &secrets);
        if crate::wire::result(Protocol::Acp, &initialized)?["protocolVersion"] != 1 {
            return Err(Error::Protocol(
                "Cursor did not negotiate ACP version 1".into(),
            ));
        }
        let mut frame = wire.rpc(2, LIST, json!({})).await?;
        crate::transport::redact(&mut frame, &secrets);
        if frame["error"]["code"] == -32601 {
            return Ok(None);
        }
        if let Some(error) = crate::opening::OpenStep::Open.error(
            Protocol::Acp,
            &crate::wire::failure::AcpFailures::Cursor,
            LIST,
            &frame["error"],
        ) {
            return Err(error.redacted(&secrets));
        }
        project(&crate::wire::result(Protocol::Acp, &frame)?).map(Some)
    };
    let reply = tokio::time::timeout(opts.handshake_timeout, read).await;
    wire.close().await;
    reply.map_err(|_| Error::Timeout("Cursor model listing".into()))?
}

pub(crate) async fn use_parameters(rt: &Runtime, opts: &SessionOptions) -> Result<bool> {
    if opts.settings.effort.is_some() || opts.settings.service_tier.is_some() {
        return Ok(true);
    }
    let Some(id) = &opts.model else {
        return Ok(opts.resume.is_none());
    };
    // A saved variant ID still needs the old picker. The two modes can only be chosen at initialize.
    Ok(catalog(rt, opts)
        .await?
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|rows| rows.iter().any(|model| model["id"] == id.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_service_tiers_and_defaults_from_cursor_fast_parameter() {
        let raw = json!({
            "models": [
                {
                    "value": "claude-sonnet-4-6",
                    "name": "Claude Sonnet 4.6",
                    "configOptions": [
                        {
                            "id": "effort",
                            "type": "select",
                            "currentValue": "medium",
                            "options": [{"value": "low"}, {"value": "medium"}, {"value": "high"}]
                        },
                        {
                            "id": "fast",
                            "type": "select",
                            "currentValue": "true",
                            "options": [{"value": "false"}, {"value": "true"}]
                        }
                    ]
                },
                {
                    "value": "gpt-5",
                    "name": "GPT-5",
                    "configOptions": [
                        {
                            "id": "fast",
                            "type": "select",
                            "currentValue": "false",
                            "options": [{"value": "false"}, {"value": "true"}]
                        }
                    ]
                },
                {
                    "value": "plain-model",
                    "configOptions": []
                }
            ]
        });
        let projected = project(&raw).unwrap();
        let rows = projected.as_array().unwrap();
        assert_eq!(rows[0]["serviceTiers"], json!(["fast"]));
        assert_eq!(rows[0]["defaultServiceTier"], "fast");
        assert_eq!(rows[0]["effortLevels"], json!(["low", "medium", "high"]));
        assert_eq!(rows[1]["serviceTiers"], json!(["fast"]));
        assert!(rows[1].get("defaultServiceTier").is_none());
        assert!(rows[2].get("serviceTiers").is_none());
    }
}
