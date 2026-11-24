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
    if opts.settings.effort.is_some() {
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
