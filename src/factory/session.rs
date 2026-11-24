use super::API_VERSION;
use crate::{ContextUsage, Error, Event, McpServer, Result, Session, SessionOptions};
use serde_json::{Value, json};
use std::path::Path;

pub(crate) fn request(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","factoryApiVersion":API_VERSION,"type":"request","id":id,"method":method,"params":params})
}

pub(crate) fn answer(id: &Value, method: &str, value: Option<Value>) -> Value {
    let value = value.or_else(|| match method {
        "droid.ask_user" => Some(json!({"cancelled":true,"answers":[]})),
        "droid.pick_options" => Some(json!({"cancelled":true,"selectedIds":[]})),
        _ => None,
    });
    let mut frame =
        json!({"jsonrpc":"2.0","factoryApiVersion":API_VERSION,"type":"response","id":id});
    if let Some(value) = value {
        frame["result"] = value;
    } else if method == "droid.request_permission" {
        frame["error"] =
            json!({"code":-32602,"message":"Factory did not offer a cancellation option"});
    } else {
        frame["error"] = json!({"code":-32601,"message":"Host declined the request"});
    }
    frame
}

/// A worker or another native client can settle several requests without a local reply.
pub(crate) fn resolved_requests(frame: &Value) -> Option<(&'static str, Vec<&Value>)> {
    if frame["type"] != "notification" || frame["method"] != "droid.session_notification" {
        return None;
    }
    let notice = &frame["params"]["notification"];
    match notice["type"].as_str()? {
        "permission_resolved" if notice["requestId"].is_string() => {
            Some(("droid.request_permission", vec![&notice["requestId"]]))
        }
        "ask_user_resolved" => Some((
            "droid.ask_user",
            notice["requestIds"]
                .as_array()?
                .iter()
                .filter(|id| id.is_string())
                .collect(),
        )),
        _ => None,
    }
}

pub(super) fn redact_settings(body: &mut Value) {
    if let Some(instructions) = body.get_mut("systemPrompt") {
        *instructions = "[redacted]".into();
    }
    for servers in ["mcpServers", "servers"] {
        for server in body
            .get_mut(servers)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            for key in ["headers", "env"] {
                if let Some(values) = server.get_mut(key) {
                    *values = "[redacted]".into();
                }
            }
        }
    }
}

pub(crate) fn redact_frame(frame: &mut Value) {
    for pointer in [
        "/params",
        "/params/notification",
        "/params/notification/settings",
        "/result",
        "/result/settings",
    ] {
        if let Some(body) = frame.pointer_mut(pointer) {
            redact_settings(body);
        }
    }
}

pub(crate) fn context(result: &Value) -> Option<ContextUsage> {
    Some(ContextUsage::counts(
        Some(result["used"].as_u64()?),
        Some(result["limit"].as_u64()?),
        false,
    ))
}

fn effort_refusal(error: Error, options: &SessionOptions, effort_only: bool) -> Error {
    if let Some(wanted) = &options.settings.effort
        && let Error::Refused(raw) = &error
        && let Ok(native) = serde_json::from_str::<Value>(raw)
        && native["code"].as_i64() == Some(-32602)
        && (effort_only
            || native["message"]
                .as_str()
                .is_some_and(|msg| msg.contains("reasoningEffort")))
    {
        // Some releases omit the parameter name on an effort-only update.
        let mut message = format!("Factory refused effort {wanted:?}: {raw}");
        crate::transport::redact_text(&mut message, &crate::options::session_secrets(options));
        return Error::Refused(message);
    }
    error
}

fn confirm_settings(options: &SessionOptions, settings: &Value) -> Result<()> {
    options.confirm_model("Factory", settings["modelId"].as_str())?;
    let reported = settings["reasoningEffort"].as_str();
    if let Some(wanted) = &options.settings.effort
        && reported != Some(wanted)
    {
        let mut message = format!(
            "Factory did not apply effort {wanted:?}, reported {}",
            reported.unwrap_or("no level")
        );
        crate::transport::redact_text(&mut message, &crate::options::session_secrets(options));
        return Err(Error::Refused(message));
    }
    Ok(())
}

pub(crate) async fn open(
    session: &Session,
    options: &SessionOptions,
    cwd: &Path,
) -> Result<(String, Value)> {
    let (method, mut params) = if let Some(id) = &options.resume {
        if options.settings.system_prompt.is_some()
            || options.settings.append_system_prompt.is_some()
        {
            return Err(Error::Unsupported(
                "Factory cannot set system instructions on resume".into(),
            ));
        }
        (
            "droid.load_session",
            json!({"sessionId":id,"messageLimit":1}),
        )
    } else {
        let mut params = json!({"machineId":"default","cwd":cwd,"sessionOriginHint":"sdk"});
        for (key, value) in [
            ("modelId", &options.model),
            ("reasoningEffort", &options.settings.effort),
        ] {
            if let Some(value) = value {
                params[key] = value.clone().into();
            }
        }
        match (
            &options.settings.system_prompt,
            &options.settings.append_system_prompt,
        ) {
            (Some(base), Some(append)) => {
                params["systemPrompt"] = format!("{base}\n\n{append}").into()
            }
            (Some(base), None) => params["systemPrompt"] = base.clone().into(),
            (None, Some(append)) => {
                params["systemPrompt"] = json!({"type":"preset","preset":"droid","append":append})
            }
            _ => {}
        }
        ("droid.initialize_session", params)
    };
    if !options.mcp_servers.is_empty() {
        params["mcpServers"] = options
            .mcp_servers
            .iter()
            .map(|(name, server)| match server {
                McpServer::Stdio { command, args, env } => {
                    json!({"name":name,"command":command,"args":args,"env":env})
                }
                McpServer::Http { .. } | McpServer::HostedHttp { .. } => server.acp(name),
            })
            .collect::<Vec<_>>()
            .into();
    }
    let step = options
        .resume
        .as_ref()
        .map(|id| crate::opening::OpenStep::Target(id.clone()))
        .unwrap_or(crate::opening::OpenStep::Open);
    let mut info = session
        .open_rpc(method, params, step)
        .await
        .map_err(|e| effort_refusal(e, options, false))?;
    let id = if let Some(expected) = &options.resume {
        let actual = info["cwd"]
            .as_str()
            .ok_or_else(|| Error::Protocol("Factory load omitted the workspace".into()))?;
        if info["sessionId"].as_str().is_some_and(|id| id != expected) {
            return Err(Error::Refused("Factory resumed a different session".into()));
        }
        if !crate::opening::same_directory(actual, cwd) {
            return Err(crate::OpenError::cwd(
                expected,
                cwd,
                crate::ResumeCwdReason::OtherCwd {
                    saved_cwd: actual.into(),
                },
            )
            .redacted(&crate::options::session_secrets(options)));
        }
        if info["isAgentLoopInProgress"] == true {
            return Err(Error::Busy);
        }
        expected.clone()
    } else {
        info["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::Protocol("Factory did not return a session ID".into()))?
            .into()
    };
    let mut changes = json!({});
    for (key, wanted) in [
        ("modelId", &options.model),
        ("reasoningEffort", &options.settings.effort),
    ] {
        if let Some(wanted) = wanted
            && info["settings"][key].as_str() != Some(wanted)
        {
            changes[key] = wanted.clone().into();
        }
    }
    if changes != json!({}) && options.resume.is_some() {
        let cursor = session.records().last().map(|r| r.cursor());
        let mut events = session.subscribe(cursor.as_ref())?;
        let effort_only =
            changes.get("modelId").is_none() && changes.get("reasoningEffort").is_some();
        session
            .rpc("droid.update_session_settings", changes)
            .await
            .map_err(|e| effort_refusal(e, options, effort_only))?;
        info["settings"] = tokio::time::timeout(options.handshake_timeout, async {
            let mut requested = None;
            while let Some(record) = events.next().await {
                if record.direction == crate::Direction::ToRuntime
                    && record.native["method"] == "droid.update_session_settings"
                {
                    requested = record.native["id"].as_str().map(str::to_owned);
                }
                let Some(rpc_id) = requested.as_deref() else {
                    continue;
                };
                let n = &record.native["params"]["notification"];
                if record.session_id.as_ref().is_none_or(|root| root == &id)
                    && n["type"] == "settings_updated"
                    && n["requestId"].as_str().is_none_or(|r| r == rpc_id)
                {
                    let settings = &n["settings"];
                    let confirmed = confirm_settings(options, settings);
                    if confirmed.is_ok() || n["requestId"] == rpc_id {
                        confirmed?;
                        return Ok(settings.clone());
                    }
                }
            }
            Err(Error::Closed(
                "Factory exited during settings confirmation".into(),
            ))
        })
        .await
        .map_err(|_| Error::Timeout("confirming Factory settings".into()))??;
    }
    confirm_settings(options, &info["settings"])?;
    Ok((id, info))
}

pub(crate) fn settings(body: &Value) -> Vec<Event> {
    let mut events = Vec::new();
    if let Some(model) = body["modelId"].as_str() {
        events.push(Event::Model {
            model: model.into(),
        });
    }
    if let Some(effort) = body["reasoningEffort"].as_str() {
        events.push(Event::Effort {
            effort: effort.into(),
        });
    }
    events
}
