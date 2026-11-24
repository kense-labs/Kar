//! Native instruction and tool-selection channels. No workspace configuration is rewritten.
use super::{McpServer, SessionSettings, env_path};
use crate::{Error, Protocol, Result, Runtime, SessionOptions};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use tokio::process::Command;

pub(crate) fn reserved_server(name: &str) -> bool {
    matches!(
        name,
        "__proto__"
            | "__defineGetter__"
            | "__defineSetter__"
            | "__lookupGetter__"
            | "__lookupSetter__"
    )
}

pub(crate) fn agent_dir(opts: &SessionOptions, cwd: &Path) -> Result<PathBuf> {
    let dir = env_path(opts, "OAR_PI_AGENT_DIR")
        .or_else(|| env_path(opts, "PI_CODING_AGENT_DIR"))
        .or_else(|| opts.session_home().map(|p| p.join(".pi/agent")))
        .ok_or_else(|| Error::Protocol("Pi agent directory is unknown".into()))?;
    Ok(cwd.join(dir))
}

fn prompt_file(
    cmd: &mut Command,
    flag: &str,
    text: &str,
    files: &mut Vec<NamedTempFile>,
) -> Result<()> {
    let mut file = NamedTempFile::new()?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    cmd.arg(flag).arg(file.path());
    files.push(file);
    Ok(())
}

pub(crate) fn launch(
    cmd: &mut Command,
    rt: &Runtime,
    opts: &SessionOptions,
    cwd: &Path,
) -> Result<Vec<NamedTempFile>> {
    let s = &opts.settings;
    let mut files = Vec::new();
    let base = s.system_prompt.as_deref();
    let append = s.append_system_prompt.as_deref();
    let deny = &s.disallowed_tools;
    match rt.protocol {
        Protocol::Claude => {
            if let Some(text) = base {
                prompt_file(cmd, "--system-prompt-file", text, &mut files)?;
            }
            if let Some(text) = append {
                prompt_file(cmd, "--append-system-prompt-file", text, &mut files)?;
            }
            if !deny.is_empty() {
                cmd.arg("--disallowed-tools").args(deny);
            }
        }
        Protocol::Qwen => {
            if let Some(text) = base {
                cmd.arg(format!("--system-prompt={text}"));
            }
            if let Some(text) = append {
                cmd.arg(format!("--append-system-prompt={text}"));
            }
            if !deny.is_empty() {
                cmd.arg("--exclude-tools").args(deny);
            }
        }
        Protocol::Pi => {
            if let Some(trusted) = s.pi_project_trusted {
                cmd.arg(if trusted { "--approve" } else { "--no-approve" });
            }
            if let Some(text) = base {
                prompt_file(cmd, "--system-prompt", text, &mut files)?;
            }
            if let Some(text) = append.filter(|text| !text.is_empty()) {
                let project = cwd.join(".pi/APPEND_SYSTEM.md");
                let source = if project.try_exists()? {
                    // Native Pi extensions may override the on-disk trust decision. Bind both
                    // the native startup and our append selection to the same explicit choice.
                    match s.pi_project_trusted {
                        Some(true) => project,
                        Some(false) => agent_dir(opts, cwd)?.join("APPEND_SYSTEM.md"),
                        None => return Err(Error::Unsupported("combining a host append with a project APPEND_SYSTEM.md requires pi_project_trusted to select the same source as Pi".into())),
                    }
                } else {
                    agent_dir(opts, cwd)?.join("APPEND_SYSTEM.md")
                };
                let prior = match std::fs::read_to_string(source) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e.into()),
                };
                let effective = joined(prior.trim_start_matches('\u{feff}'), text);
                prompt_file(cmd, "--append-system-prompt", &effective, &mut files)?;
            }
            if !deny.is_empty() {
                cmd.arg("--exclude-tools").arg(deny.join(","));
            }
        }
        Protocol::Acp if rt.id == "codebuddy" => {
            if let Some(text) = base {
                prompt_file(cmd, "--system-prompt-file", text, &mut files)?;
            }
            if !deny.is_empty() {
                cmd.arg("--disallowedTools").arg(deny.join(","));
            }
        }
        _ => {}
    }
    Ok(files)
}
fn joined(first: &str, last: &str) -> String {
    match (first.is_empty(), last.is_empty()) {
        (true, _) => last.to_owned(),
        (_, true) => first.to_owned(),
        _ => format!("{first}\n\n{last}"),
    }
}

pub(crate) fn validate(s: &SessionSettings, p: Protocol, runtime: &str) -> Result<()> {
    use Protocol::*;
    let grok = p == Acp && runtime == "grok";
    let buddy = p == Acp && runtime == "codebuddy";
    if (grok || buddy)
        && s.system_prompt
            .as_ref()
            .is_some_and(|v| v.trim().is_empty())
        || p == Pi && s.system_prompt.as_deref() == Some("")
    {
        return Err(Error::Unsupported(
            "native runtime treats an empty replacement as omitted".into(),
        ));
    }
    for name in &s.disallowed_tools {
        if name.trim().is_empty()
            || name.contains('\0')
            || name.starts_with('-')
            || (matches!(p, Pi | Qoder) || buddy) && name.contains(',')
        {
            return Err(Error::Refused(
                "tool exclusion is not a single native tool name or rule".into(),
            ));
        }
        if p == Codex && !name.starts_with("mcp__") {
            return Err(Error::Unsupported(
                "Codex can only disable qualified MCP tools".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn redact_open(protocol: Protocol, packet: &mut Value) {
    let original_method = packet["method"].as_str();
    let map = match protocol {
        Protocol::Codex
            if matches!(
                original_method,
                Some("thread/start" | "thread/fork" | "thread/resume")
            ) =>
        {
            if let Some(args) = packet["params"].as_object_mut() {
                for key in ["baseInstructions", "developerInstructions"] {
                    if let Some(value) = args.get_mut(key) {
                        *value = "[redacted]".into();
                    }
                }
            }
            packet.pointer_mut("/params/config/mcp_servers")
        }
        Protocol::Qwen
            if packet
                .pointer("/request/subtype")
                .is_some_and(|v| v == "initialize") =>
        {
            packet.pointer_mut("/request/mcpServers")
        }
        Protocol::Acp
            if matches!(
                original_method,
                Some("session/new" | "session/load" | "session/fork")
            ) =>
        {
            if let Some(servers) = packet
                .pointer_mut("/params/mcpServers")
                .and_then(Value::as_array_mut)
            {
                for server in servers {
                    for field in ["env", "headers"] {
                        for pair in server
                            .get_mut(field)
                            .and_then(Value::as_array_mut)
                            .into_iter()
                            .flatten()
                        {
                            if let Some(value) = pair.get_mut("value") {
                                *value = "[redacted]".into();
                            }
                        }
                    }
                }
            }
            None
        }
        _ => None,
    };
    for server in map
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|map| map.values_mut())
    {
        for field in ["env", "headers", "http_headers"] {
            if let Some(values) = server.get_mut(field).and_then(Value::as_object_mut) {
                for value in values.values_mut() {
                    *value = "[redacted]".into();
                }
            }
        }
    }
}
// Only this projection of config/read is passed back to the opener, never into a record.
pub(crate) fn codex_config(body: &Value) -> Result<Value> {
    let config = body
        .get("config")
        .filter(|v| v.is_object())
        .ok_or_else(|| Error::Protocol("missing effective Codex configuration".into()))?;
    let mut disabled = Map::new();
    if let Some(servers) = config.get("mcp_servers").filter(|v| !v.is_null()) {
        for (name, options) in servers
            .as_object()
            .ok_or_else(|| Error::Protocol("invalid Codex mcp_servers".into()))?
        {
            disabled.insert(name.clone(), options["disabled_tools"].clone());
        }
    }
    Ok(
        json!({"goals":config.pointer("/features/goals") == Some(&Value::Bool(true)), "developer_instructions":config["developer_instructions"], "disabled":disabled}),
    )
}

pub(crate) fn codex_open(
    params: &mut Value,
    settings: &SessionSettings,
    servers: &BTreeMap<String, McpServer>,
    config: &Value,
) -> Result<()> {
    if let Some(base) = &settings.system_prompt {
        params["baseInstructions"] = base.clone().into();
    }
    if let Some(append) = &settings.append_system_prompt {
        let prior = match &config["developer_instructions"] {
            Value::Null => "",
            Value::String(v) => v,
            _ => {
                return Err(Error::Protocol(
                    "invalid Codex developer_instructions".into(),
                ));
            }
        };
        params["developerInstructions"] = joined(prior, append).into();
    }
    let configured = config["disabled"].as_object();
    let names: BTreeSet<_> = servers
        .keys()
        .chain(configured.into_iter().flat_map(|m| m.keys()))
        .collect();
    for deny in &settings.disallowed_tools {
        let mut matches = names.iter().filter(|s| {
            !s.is_empty()
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && deny.starts_with(&format!("mcp__{s}__"))
        });
        let server = matches
            .next()
            .filter(|s| matches.next().is_none() && !reserved_server(s))
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "cannot resolve Codex MCP tool {deny:?} to one configured server"
                ))
            })?;
        let tool = deny
            .strip_prefix(&format!("mcp__{server}__"))
            .expect("matched server prefix");
        if tool.is_empty() {
            return Err(Error::Refused("missing Codex MCP tool name".into()));
        }
        let slot = &mut params["config"]["mcp_servers"][server.as_str()]["disabled_tools"];
        if slot.is_null() {
            let old = &config["disabled"][server.as_str()];
            if !old.is_null()
                && !old
                    .as_array()
                    .is_some_and(|arr| arr.iter().all(Value::is_string))
            {
                return Err(Error::Protocol(format!(
                    "invalid existing disabled_tools for server {server:?}"
                )));
            }
            *slot = old.as_array().cloned().unwrap_or_default().into();
        }
        let list = slot
            .as_array_mut()
            .expect("constructed disabled_tools array");
        if !list.iter().any(|v| v == tool) {
            list.push(tool.into());
        }
    }
    Ok(())
}
