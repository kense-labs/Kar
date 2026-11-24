pub(crate) mod availability;
pub(crate) mod environment;
pub(crate) mod instructions;
pub(crate) mod mcp_capabilities;
pub(crate) mod mcp_file;
pub(crate) mod opencode;
pub(crate) mod qwen_memory;
pub(crate) mod user_configuration;
use crate::{Error, Result, SessionOptions};
pub use qwen_memory::{NativeMemory, NativeMemoryDefault};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};
pub use user_configuration::UserConfigurationCategory;

impl SessionOptions {
    pub(crate) fn confirm_model(&self, runtime: &str, reported: Option<&str>) -> Result<()> {
        if let Some(wanted) = self.model.as_deref()
            && Some(wanted) != reported
        {
            return Err(crate::OpenError::model_confirmation(
                format!(
                    "{runtime} did not confirm model {wanted:?}, reported {}",
                    reported.unwrap_or("no model")
                ),
                wanted,
                reported,
            )
            .redacted(&session_secrets(self)));
        }
        Ok(())
    }
}

/// Endpoint for a host-provided Model Context Protocol tool server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "McpConfig", into = "McpConfig")]
pub enum McpServer {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    /// An HTTP endpoint created by `serve_mcp_tools`. Some runtimes wait for it at startup.
    HostedHttp {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum McpConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
}
impl From<McpConfig> for McpServer {
    fn from(config: McpConfig) -> Self {
        match config {
            McpConfig::Stdio { command, args, env } => Self::Stdio { command, args, env },
            McpConfig::Http {
                url,
                headers,
                origin,
            } if origin.as_deref() == Some("serveMcpTools") => Self::HostedHttp { url, headers },
            McpConfig::Http { url, headers, .. } => Self::Http { url, headers },
        }
    }
}
impl From<McpServer> for McpConfig {
    fn from(config: McpServer) -> Self {
        match config {
            McpServer::Stdio { command, args, env } => Self::Stdio { command, args, env },
            McpServer::Http { url, headers } => Self::Http {
                url,
                headers,
                origin: None,
            },
            McpServer::HostedHttp { url, headers } => Self::Http {
                url,
                headers,
                origin: Some("serveMcpTools".into()),
            },
        }
    }
}
impl McpServer {
    pub(crate) fn vendor_config(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        if let Some(map) = value.as_object_mut() {
            map.remove("origin");
        }
        Ok(value)
    }
    pub(crate) fn has_credentials(&self) -> bool {
        match self {
            Self::Stdio { env, .. } => !env.is_empty(),
            Self::Http { headers, .. } | Self::HostedHttp { headers, .. } => !headers.is_empty(),
        }
    }
    pub(crate) fn headers(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            Self::Stdio { .. } => None,
            Self::Http { headers, .. } | Self::HostedHttp { headers, .. } => Some(headers),
        }
    }
    fn pairs(map: &BTreeMap<String, String>) -> Vec<Value> {
        map.iter()
            .map(|(name, value)| json!({ "name":name,"value":value }))
            .collect()
    }
    pub(crate) fn acp(&self, name: &str) -> Value {
        match self {
            Self::Stdio { command, args, env } => {
                json!({ "name":name,"command":command,"args":args,"env":Self::pairs(env) })
            }
            Self::Http { url, headers } | Self::HostedHttp { url, headers } => {
                json!({ "name":name,"type":"http","url":url,"headers":Self::pairs(headers) })
            }
        }
    }
    pub(crate) fn codex(&self) -> Value {
        match self {
            Self::Stdio { command, args, env } => {
                json!({"command":command,"args":args,"env":env,"enabled":true})
            }
            Self::Http { url, headers } => json!({"enabled":true,"url":url,"http_headers":headers}),
            Self::HostedHttp { url, headers } => {
                json!({"enabled":true,"url":url,"http_headers":headers,"required":true,"startup_timeout_sec":30})
            }
        }
    }
    pub(crate) fn qwen(&self) -> Value {
        match self {
            Self::Stdio { command, args, env } => json!({"command":command,"args":args,"env":env}),
            Self::Http { url, headers } | Self::HostedHttp { url, headers } => {
                json!({"httpUrl":url,"headers":headers})
            }
        }
    }
    pub(crate) fn secret_values(&self) -> impl Iterator<Item = &String> {
        let map = match self {
            Self::Http { headers, .. } | Self::HostedHttp { headers, .. } => headers,
            Self::Stdio { env, .. } => env,
        };
        map.values().filter(|v| !v.is_empty())
    }
}
pub(crate) fn env_path(opts: &SessionOptions, name: &str) -> Option<PathBuf> {
    opts.env_value(name)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}
/// Pi takes a file on argv, while callers resume by the native session ID.
pub(crate) fn pi_file(cwd: &Path, opts: &SessionOptions, id: &str) -> Result<PathBuf> {
    let agent_dir = instructions::agent_dir(opts, cwd)?;
    let path = cwd.to_string_lossy();
    let slug = path
        .trim_start_matches(['/', '\\'])
        .replace(['/', '\\', ':'], "-");
    let dir = agent_dir.join("sessions").join(format!("--{slug}--"));
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut count = 0usize;
    let mut found = None;
    if let Some(entries) = entries {
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.path().extension() != Some(OsStr::new("jsonl"))
            {
                continue;
            }
            let mut header = String::new();
            BufReader::new(std::fs::File::open(entry.path())?.take(64 * 1024))
                .read_line(&mut header)?;
            if let Ok(h) = serde_json::from_str::<Value>(&header)
                && h["type"] == "session"
                && h["id"].is_string()
                && h["cwd"]
                    .as_str()
                    .and_then(|p| std::fs::canonicalize(p).ok())
                    .as_deref()
                    == Some(cwd)
            {
                count += 1;
                if h["id"] == id && found.replace(entry.path()).is_some() {
                    return Err(Error::Protocol(
                        "multiple Pi transcripts match the same session and workspace".into(),
                    ));
                }
            }
        }
    }
    if let Some(path) = found {
        return Ok(path);
    }
    Err(crate::OpenError::SessionNotFound {
        session_id: id.into(),
        message: format!("no saved Pi session {id} for this working directory"),
        cause: crate::NativeCause {
            method: "pi_file".into(),
            native: json!({"cwd":cwd,"sessionDir":dir,"sessionCount":count}),
        },
    }
    .redacted(&session_secrets(opts)))
}
pub(crate) fn acp_model_option(info: &Value) -> Option<&Value> {
    info["configOptions"]
        .as_array()?
        .iter()
        .find(|opt| opt["category"] == "model" || opt["id"] == "model")
}

pub(crate) fn acp_mode_advertised(info: &Value, runtime: &str, mode: &str) -> bool {
    if let Some(modes) = info["modes"]["availableModes"].as_array() {
        return modes.iter().any(|entry| entry["id"] == mode);
    }
    // Earlier Kimi ACP versions expose set_mode choices only as config options.
    runtime == "kimi"
        && info["configOptions"]
            .as_array()
            .and_then(|options| options.iter().find(|option| option["id"] == "mode"))
            .and_then(|option| option["options"].as_array())
            .is_some_and(|values| values.iter().any(|entry| entry["value"] == mode))
}

pub(crate) fn acp_effort_option(info: &Value, cursor: bool) -> Option<&Value> {
    if cursor && let Some(option) = crate::cursor_models::effort(info) {
        return Some(option);
    }
    info["configOptions"]
        .as_array()?
        .iter()
        .find(|v| v["category"] == "thought_level")
}

pub(crate) fn environment_secrets(
    env: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> Vec<String> {
    std::env::vars_os()
        .chain(env.clone())
        .filter_map(|(k, v)| {
            let k = k.to_string_lossy().to_ascii_uppercase();
            let v = v.into_string().ok()?;
            (v.len() >= 8
                && [
                    "TOKEN",
                    "PASSWORD",
                    "SECRET",
                    "API_KEY",
                    "CREDENTIAL",
                    "COOKIE",
                ]
                .iter()
                .any(|end| k.ends_with(end)))
            .then_some(v)
        })
        .collect()
}

/// Include credentials embedded in explicit environment URLs and the token in an
/// MCP authorization header, even if only the bare value appears in a request.
pub(crate) fn session_secrets(opts: &SessionOptions) -> Vec<String> {
    let mut values = environment_secrets(&opts.env);
    let mut env_value = |name: &str, value: &str| {
        let upper = name.to_ascii_uppercase();
        if value.len() >= 8
            && !value.starts_with(['/', '~'])
            && value.as_bytes().get(1) != Some(&b':')
            && matches!(
                upper.rsplit('_').next(),
                Some("KEY" | "APIKEY" | "PASSWD" | "PASSPHRASE" | "CREDENTIALS" | "PWD")
            )
        {
            values.push(value.to_owned());
        }
        if value.contains("://")
            && let Ok(url) = reqwest::Url::parse(value)
            && let Some(password) = url.password().filter(|s| !s.is_empty())
        {
            values.push(value.to_owned());
            if password.len() >= 8 {
                values.push(password.to_owned());
            }
            if let Ok(decoded) = percent_encoding::percent_decode_str(password).decode_utf8()
                && decoded.len() >= 8
            {
                values.push(decoded.into_owned());
            }
        }
    };
    for (k, v) in &opts.env {
        if let (Some(k), Some(v)) = (k.to_str(), v.to_str()) {
            env_value(k, v);
        }
    }
    for server in opts.mcp_servers.values() {
        if let McpServer::Stdio { env, .. } = server {
            for (k, v) in env {
                env_value(k, v);
            }
        }
    }
    for server in opts.mcp_servers.values() {
        values.extend(server.secret_values().filter(|s| s.len() >= 8).cloned());
        if let Some(headers) = server.headers() {
            for (k, v) in headers {
                if (k.eq_ignore_ascii_case("authorization")
                    || k.eq_ignore_ascii_case("proxy-authorization"))
                    && let Some((_, token)) = v.trim().split_once(char::is_whitespace)
                    && token.trim().len() >= 8
                {
                    values.push(token.trim().into());
                }
            }
        }
    }
    values.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    values.dedup();
    values
}

pub(crate) fn claude_environment(
    command: &mut tokio::process::Command,
    env: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) {
    for marker in [
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_CODE_CHROME_MCP_ORG_DENIED",
        "CLAUDE_CODE_EVAL_INTERVIEW_SESSION",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CLAUDE_CODE_HOST_WORKTREE",
        "CLAUDE_CODE_HOST_WORKTREE_FENCE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
    ] {
        command.env_remove(marker);
    }
    command.envs(env).env_remove("CLAUDECODE");
}

/// Controls DSH's vendor transcript upload for this launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SessionLogUpload {
    Off,
}

/// Native launch settings retained with a managed chat and reapplied on resume.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionSettings {
    /// End Codex commands or hosted Grok terminals attributed to an interrupted turn.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub end_turn_processes_on_abort: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_log_upload: Option<SessionLogUpload>,
    /// Lower the native background-memory default; explicit settings retain precedence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_memory_default: Option<NativeMemoryDefault>,
    /// Opt into observing the most recent attributed model request of each open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_requests: Option<crate::ModelRequests>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub goals: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    /// Explicit Pi project-resource choice for this process. Does not edit its trust store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_project_trusted: Option<bool>,
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub exclude_user_configuration: std::collections::BTreeSet<UserConfigurationCategory>,
    pub name: Option<String>,
    pub effort: Option<String>,
    /// Native ACP session mode to select on each open, including resume.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_mode: Option<String>,
    pub service_tier: Option<String>,
    pub additional_directories: Vec<PathBuf>,
    /// Native final-output JSON Schema for turns started by this host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Map<String, Value>>,
}
impl SessionSettings {
    pub(crate) fn validate(&self, protocol: crate::Protocol, runtime: &str) -> Result<()> {
        availability::validate(self, protocol, runtime)?;
        instructions::validate(self, protocol, runtime)?;
        if self
            .acp_mode
            .as_deref()
            .is_some_and(|mode| mode.trim().is_empty() || mode.contains('\0'))
        {
            return Err(Error::Refused(
                "ACP mode requires a nonempty native ID without NUL".into(),
            ));
        }
        if self
            .effort
            .as_ref()
            .is_some_and(|v| v.trim().is_empty() || v.contains(char::from(0)))
        {
            return Err(Error::Refused(
                "effort must be a nonempty native level".into(),
            ));
        }
        if let Some(tier) = &self.service_tier
            && (tier.trim().is_empty()
                || (protocol == crate::Protocol::Claude
                    && !matches!(tier.as_str(), "default" | "fast")))
        {
            return Err(Error::Refused("unsupported service tier".into()));
        }
        if let Some(name) = &self.name
            && (name.trim().is_empty() || name.contains('\0'))
        {
            return Err(Error::Refused(
                "session name must contain text and no NUL".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn directories(&self, cwd: &Path) -> Result<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        for dir in &self.additional_directories {
            if dir.as_os_str().is_empty() || dir.as_os_str().as_encoded_bytes().contains(&0) {
                return Err(Error::Refused(
                    "additional directories require nonempty paths without NUL".into(),
                ));
            }
            let mut resolved = PathBuf::new();
            for part in cwd.join(dir).components() {
                match part {
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        resolved.pop();
                    }
                    value => resolved.push(value.as_os_str()),
                }
            }
            if !dirs.contains(&resolved) {
                dirs.push(resolved);
            }
        }
        Ok(dirs)
    }
}

pub(crate) fn pi_has_name(file: &Path) -> Result<bool> {
    let reader = BufReader::new(std::fs::File::open(file)?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(&line)?;
        if row["type"] == "session_info" {
            return Ok(true);
        }
    }
    Ok(false)
}
