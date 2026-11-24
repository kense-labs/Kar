//! Qoder's worker and per-session daemon transports share the stream-JSON conversation.
use crate::{Error, Result, Runtime, Session, SessionOptions};
use semver::Version;
use serde_json::{Value, json};
use std::{io::Write, path::Path, process::Stdio};
use tempfile::NamedTempFile;
use tokio::process::Command;

pub(crate) const PRIVATE: &str = "kar-qoder-";
pub(crate) fn input(session: Option<&str>, text: &str) -> Value {
    json!({"type":"user","uuid":uuid::Uuid::new_v4(),"session_id":session,"priority":"next",
        "message":{"role":"user","content":[{"type":"text","text":text}]}})
}
pub(crate) struct Launch {
    version: Version,
    daemon_args: Option<Vec<String>>,
}
fn env(opts: &SessionOptions, key: &str) -> Option<String> {
    opts.env_value(key)
        .and_then(|v| v.into_string().ok())
        .filter(|v| !v.is_empty())
}
fn auth(opts: &SessionOptions) -> Result<Value> {
    let access = env(opts, "QODER_PERSONAL_ACCESS_TOKEN");
    let service = env(opts, "QODER_SERVICE_ACCOUNT_KEY");
    let job = env(opts, "QODER_JOB_TOKEN");
    match (access, service, job) {
        (Some(_), Some(_), _) | (_, Some(_), Some(_)) => Err(Error::Refused(
            "conflicting Qoder authentication methods".into(),
        )),
        (Some(token), None, _) => Ok(json!({"type":"accessToken","accessToken":token})),
        (None, Some(key), None) => Ok(json!({"type":"serviceAccount","serviceAccountKey":key})),
        (None, None, Some(_)) => Err(Error::Unsupported(
            "Qoder job-token authentication requires a host callback".into(),
        )),
        _ => Ok(json!({"type":"qodercli"})),
    }
}
pub(crate) fn credentials(opts: &SessionOptions) -> impl Iterator<Item = String> + '_ {
    [
        "QODER_PERSONAL_ACCESS_TOKEN",
        "QODER_SERVICE_ACCOUNT_KEY",
        "QODER_JOB_TOKEN",
        "QODER_API_KEY",
    ]
    .into_iter()
    .filter_map(|key| env(opts, key))
}
fn version(raw: &str) -> Result<Version> {
    let value = raw
        .split_whitespace()
        .last()
        .and_then(|v| Version::parse(v.trim_start_matches('v')).ok())
        .filter(|v| v.pre.is_empty() && v.build.is_empty() && v >= &Version::new(1, 1, 78));
    value.ok_or_else(|| {
        Error::Unsupported("Qoder requires a stable CLI version 1.1.78 or newer".into())
    })
}
pub(crate) async fn prepare(
    rt: &Runtime,
    opts: &SessionOptions,
    cwd: &Path,
    id: &str,
    process: &mut Command,
    files: &mut Vec<NamedTempFile>,
    mcp: Option<&Path>,
) -> Result<Launch> {
    let output = tokio::time::timeout(
        opts.handshake_timeout,
        opts.command(&rt.program)?
            .arg("--version")
            .current_dir(cwd)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| Error::Timeout("Qoder version query".into()))??;
    if !output.status.success() {
        return Err(Error::Refused("Qoder version query failed".into()));
    }
    let version = version(std::str::from_utf8(&output.stdout).unwrap_or_default())?;
    let mut args: Vec<String> = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--replay-user-messages",
        "--permission-mode",
        "default",
        "--permission-prompt-tool",
        "stdio",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(format!(
        "--{}={id}",
        if opts.resume.is_some() {
            "resume"
        } else {
            "session-id"
        }
    ));
    for (flag, value) in [
        ("--model", opts.model.as_ref()),
        ("--reasoning-effort", opts.settings.effort.as_ref()),
    ] {
        if let Some(value) = value {
            args.extend([flag.into(), value.clone()]);
        }
    }
    let path = |p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Unsupported("Qoder requires UTF-8 paths".into()))
    };
    for (flag, text) in [
        ("--system-prompt-file", &opts.settings.system_prompt),
        (
            "--append-system-prompt-file",
            &opts.settings.append_system_prompt,
        ),
    ] {
        if let Some(text) = text {
            let mut file = NamedTempFile::new()?;
            file.write_all(text.as_bytes())?;
            file.flush()?;
            args.extend([flag.into(), path(file.path())?]);
            files.push(file);
        }
    }
    for arg in &rt.args {
        let arg = arg
            .to_str()
            .ok_or_else(|| Error::Unsupported("Qoder requires UTF-8 arguments".into()))?;
        validate_arg(arg)?;
        args.push(arg.into());
    }
    if let Some(mcp) = mcp {
        args.extend(["--mcp-config".into(), path(mcp)?]);
    }
    let dirs = opts.settings.directories(cwd)?;
    if !dirs.is_empty() {
        args.push("--add-dir".into());
        args.extend(dirs.iter().map(|d| path(d)).collect::<Result<Vec<_>>>()?);
    }
    if !opts.settings.disallowed_tools.is_empty() {
        args.extend([
            "--disallowed-tools".into(),
            opts.settings.disallowed_tools.join(","),
        ]);
    }
    process.env("QODER_EXPOSE_TOKEN_USAGE", "1");
    let daemon_args = if version >= Version::new(1, 1, 81) {
        process.arg("--sdk").env("QODER_AGENT_SDK_DAEMON", "1");
        Some(args)
    } else {
        process.args(args);
        None
    };
    Ok(Launch {
        version,
        daemon_args,
    })
}
fn validate_arg(arg: &str) -> Result<()> {
    let name = arg.split('=').next().unwrap_or(arg);
    let mut flag = String::new();
    if name.starts_with("--") {
        for ch in name.chars() {
            if ch.is_ascii_uppercase() {
                flag.push('-');
            }
            flag.push(ch.to_ascii_lowercase());
        }
    } else if name.starts_with('-') {
        flag.extend(name.chars().take(2));
    }
    if matches!(
        flag.as_str(),
        "--" | "-p"
            | "-f"
            | "-o"
            | "-r"
            | "-c"
            | "-m"
            | "-n"
            | "--print"
            | "--input-format"
            | "--output-format"
            | "--include-partial-messages"
            | "--no-include-partial-messages"
            | "--replay-user-messages"
            | "--no-replay-user-messages"
            | "--session-id"
            | "--resume"
            | "--continue"
            | "--sdk"
            | "--acp"
            | "--fork-session"
            | "--remote"
            | "--remote-session"
            | "--permission-mode"
            | "--permission-prompt-tool"
            | "--dangerously-skip-permissions"
            | "--model"
            | "--reasoning-effort"
            | "--system-prompt"
            | "--system-prompt-file"
            | "--append-system-prompt"
            | "--append-system-prompt-file"
            | "--name"
    ) {
        return Err(Error::Unsupported(format!(
            "Qoder launch arguments cannot override {flag}"
        )));
    }
    Ok(())
}
pub(crate) fn private_method(method: &str) -> bool {
    matches!(
        method,
        "daemon.initialize" | "daemon.open_session" | "initialize" | "get_settings" | "get_models"
    )
}
pub(crate) fn remember(value: &Value, secrets: &mut Vec<String>) {
    match value {
        Value::Array(a) => {
            for v in a {
                remember(v, secrets);
            }
        }
        Value::Object(obj) => {
            for (k, v) in obj {
                let k = k.to_ascii_lowercase();
                if let Some(s) = v.as_str().filter(|s| !s.is_empty())
                    && ["token", "secret", "password", "apikey", "api_key"]
                        .iter()
                        .any(|p| k.contains(p))
                {
                    if !secrets.iter().any(|t| t == s) {
                        secrets.push(s.into());
                    }
                } else {
                    remember(v, secrets);
                }
            }
        }
        _ => {}
    }
}
pub(crate) fn private_reply(method: &str, v: &Value, session: Option<&str>) -> Result<Value> {
    if v["response"]["subtype"] != "success" {
        return Err(Error::Refused(format!("Qoder refused {method}")));
    }
    let body = &v["response"]["response"];
    match method {
        "daemon.initialize" => {
            let ok = body["daemon_protocol_version"] == 2
                && body["capabilities"].as_array().is_some_and(|c| {
                    ["multi_session_v2", "daemon_queries_v1"]
                        .iter()
                        .all(|expected| c.iter().any(|v| v == expected))
                });
            if !ok {
                return Err(Error::Protocol(
                    "Qoder did not confirm daemon protocol v2 and session routing".into(),
                ));
            }
            Ok(Value::Null)
        }
        "daemon.open_session" if v["session_id"].as_str() != session || session.is_none() => Err(
            Error::Protocol("Qoder did not confirm the requested session".into()),
        ),
        "get_settings" => {
            Ok(json!({"effort":body.pointer("/applied/effort").and_then(Value::as_str)}))
        }
        "get_models" => {
            let rows = body["models"]
                .as_array()
                .ok_or_else(|| Error::Protocol("invalid Qoder model catalog".into()))?;
            Ok(Value::Array(
                rows.iter()
                    .filter_map(|row| {
                        let id = row["value"].as_str().filter(|s| !s.is_empty())?;
                        let mut clean = json!({"value":id});
                        for field in ["displayName", "modelId", "defaultEffort"] {
                            if let Some(s) = row[field].as_str() {
                                clean[field] = s.into();
                            }
                        }
                        for field in ["isDefault", "isEnabled"] {
                            if let Some(b) = row[field].as_bool() {
                                clean[field] = b.into();
                            }
                        }
                        if let Some(a) = row["efforts"].as_array() {
                            clean["efforts"] = json!(
                                a.iter()
                                    .filter_map(|e| {
                                        e.as_str()
                                            .or_else(|| {
                                                e.get("effort").or_else(|| e.get("id"))?.as_str()
                                            })
                                            .filter(|s| !s.is_empty())
                                    })
                                    .collect::<Vec<_>>()
                            );
                        }
                        if let Some(s) = row
                            .pointer("/serverModel/display_name")
                            .and_then(Value::as_str)
                        {
                            clean["serverModel"] = json!({"display_name":s});
                        }
                        Some(clean)
                    })
                    .collect(),
            ))
        }
        _ => Ok(Value::Null),
    }
}
fn resolved<'a>(models: &'a Value, selector: &str, version: &Version) -> Option<&'a str> {
    let rows = models.as_array()?;
    let row = rows.iter().find(|r| r["value"] == selector)?;
    if version < &Version::new(1, 1, 82) {
        return row
            .pointer("/serverModel/display_name")?
            .as_str()
            .filter(|s| !s.is_empty());
    }
    let name = row["displayName"].as_str().filter(|n| !n.is_empty())?;
    (row["modelId"] == selector
        && rows.iter().filter(|r| r["displayName"] == name).count() == 1
        && rows.iter().filter(|r| r["value"] == selector).count() == 1)
        .then_some(name)
}
impl Launch {
    pub async fn open(
        self,
        session: &Session,
        opts: &SessionOptions,
        id: &str,
        cwd: &str,
    ) -> Result<Value> {
        if let Some(args) = self.daemon_args {
            session.rpc("daemon.initialize",json!({"daemon_protocol_version":2,"sdk_version":"kar","capabilities":["multi_session_v2","daemon_queries_v1"],"auth":auth(opts)?})).await?;
            session
                .rpc("daemon.open_session", json!({"worker_args":args,"cwd":cwd}))
                .await?;
        }
        let initialized = session.rpc("initialize", json!({})).await?;
        let reported = initialized["model"].as_str();
        if let Some(model) = &opts.model
            && (self.version >= Version::new(1, 1, 82) || reported != Some(model))
        {
            let refusal =
                |reason: &str, catalog: Option<&Value>, failure: Option<(&str, Error)>| {
                    let mut secrets = crate::options::session_secrets(opts);
                    secrets.extend(credentials(opts));
                    crate::OpenError::ModelConfirmation {
                        message: format!("Qoder could not confirm model {model:?}: {reason}"),
                        requested: model.clone(),
                        reported: reported.map(str::to_owned),
                        catalog_selector_ids: catalog.and_then(Value::as_array).map(|rows| {
                            rows.iter()
                                .filter_map(|r| r["value"].as_str().map(str::to_owned))
                                .collect()
                        }),
                        cause: failure.map(|(strategy, e)| crate::NativeCause {
                            method: "get_models".into(),
                            native: json!({"fetchStrategy":strategy,"message":e.to_string()}),
                        }),
                    }
                    .redacted(&secrets)
                };
            let mut catalog = session
                .rpc("get_models", json!({"fetchStrategy":"cache"}))
                .await
                .map_err(|e| refusal("cache lookup failed", None, Some(("cache", e))))?;
            if resolved(&catalog, model, &self.version).is_none() {
                catalog = session
                    .rpc("get_models", json!({"fetchStrategy":"live"}))
                    .await
                    .map_err(|e| {
                        refusal("live lookup failed", Some(&catalog), Some(("live", e)))
                    })?;
            }
            let native = resolved(&catalog, model, &self.version).ok_or_else(|| {
                refusal("catalog did not resolve the selector", Some(&catalog), None)
            })?;
            if Some(native) != reported {
                return Err(refusal(
                    "initialized with a different model",
                    Some(&catalog),
                    None,
                ));
            }
        }
        let confirmed = session.rpc("get_settings", json!({})).await?;
        if let Some(effort) = &opts.settings.effort
            && confirmed["effort"] != effort.as_str()
        {
            return Err(Error::Refused(
                "Qoder did not confirm the requested effort".into(),
            ));
        }
        Ok(
            json!({"sessionId":id,"model":reported,"effort":confirmed["effort"],"version":self.version.to_string()}),
        )
    }
}

pub(crate) fn failure(v: &Value) -> crate::TurnFailure {
    let reason = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let class = if v["error_code"] == 118 || v["error_code"] == "118" {
        crate::FailureClass::Billing
    } else if v["subtype"] == "error_max_turns" {
        crate::FailureClass::LimitReached
    } else {
        crate::FailureClass::Unknown
    };
    crate::TurnFailure::new(
        class,
        if reason.is_empty() {
            v["result"]
                .as_str()
                .unwrap_or("Qoder reported a failed turn")
                .into()
        } else {
            reason
        },
    )
}
