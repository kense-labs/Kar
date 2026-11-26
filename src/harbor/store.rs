//! Persistent workspace, channel log, audit journal, and state store for Harbor (`kar::harbor`).

use crate::{Error, Result, stream::timestamp_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Parsed `<name>=<runtime>[:<model>]` specification for a Harbor agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl AgentSpec {
    /// Parse `<name>=<runtime>[:<model>]` (e.g. `coder=codex:gpt-5` or `reviewer=claude`).
    pub fn parse(raw: &str) -> Result<Self> {
        let Some((name, rest)) = raw.split_once('=') else {
            return Err(Error::Refused(format!(
                "expected agent spec `<name>=<runtime>[:<model>]`, got {raw:?}"
            )));
        };
        validate_agent_name(name)?;
        let (runtime, model) = match rest.split_once(':') {
            Some((rt, m)) => {
                if m.trim().is_empty() {
                    return Err(Error::Refused(format!("empty model in agent spec {raw:?}")));
                }
                (rt, Some(m.to_owned()))
            }
            None => (rest, None),
        };
        if runtime.trim().is_empty() {
            return Err(Error::Refused(format!(
                "empty runtime in agent spec {raw:?}"
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            runtime: runtime.to_owned(),
            model,
        })
    }
}

/// Validate that a Harbor agent handle is `[a-z0-9_-]{1,32}` and not reserved (`all`).
pub fn validate_agent_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 32
        || name == "all"
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(Error::Refused(format!(
            "invalid Harbor agent name {name:?}: use 1..=32 lowercase `[a-z0-9_-]` (excluding `all`)"
        )));
    }
    Ok(())
}

/// Parse `<agent>:<KEY>=<VALUE>` entries supplied via `--agent-env`.
pub fn parse_agent_env(entries: &[String]) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for entry in entries {
        let Some((agent, kv)) = entry.split_once(':') else {
            return Err(Error::Refused(format!(
                "expected `--agent-env <agent>:<KEY>=<VALUE>`, got {entry:?}"
            )));
        };
        validate_agent_name(agent)?;
        let Some((key, value)) = kv.split_once('=') else {
            return Err(Error::Refused(format!(
                "expected `--agent-env <agent>:<KEY>=<VALUE>`, got {entry:?}"
            )));
        };
        if key.is_empty() || key.contains('\0') || value.contains('\0') {
            return Err(Error::Refused(format!(
                "invalid environment variable in `--agent-env` {entry:?}"
            )));
        }
        out.entry(agent.to_owned())
            .or_default()
            .insert(key.to_owned(), value.to_owned());
    }
    Ok(out)
}

/// Resolve the Harbor workspace directory from explicit `--dir`, `KAR_HARBOR_DIR` / `HARBOR_DIR`,
/// or `~/.kar-harbor/<run_name>` (default `"default"`).
pub fn resolve_harbor_dir(explicit: Option<&Path>, run_name: Option<&str>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    if let Some(env_dir) = std::env::var_os("KAR_HARBOR_DIR")
        .or_else(|| std::env::var_os("HARBOR_DIR"))
        .filter(|v| !v.is_empty())
    {
        return Ok(PathBuf::from(env_dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::Unsupported("HOME is not set; pass `--dir <path>`".into()))?;
    let slug = run_name.unwrap_or("default");
    if slug.is_empty() || slug.contains('/') || slug.contains('\\') || slug == "." || slug == ".." {
        return Err(Error::Refused(format!("invalid Harbor run name {slug:?}")));
    }
    Ok(home.join(".kar-harbor").join(slug))
}

/// Extract `@name` mentions from message text (`@all` or `@agent_name`).
pub fn mentions_in(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let preceded_by_ident = i > 0
                && (bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'-');
            if !preceded_by_ident {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_lowercase()
                        || bytes[end].is_ascii_uppercase()
                        || bytes[end].is_ascii_digit()
                        || bytes[end] == b'_'
                        || bytes[end] == b'-')
                {
                    end += 1;
                }
                if end > start {
                    let mention = text[start..end].to_ascii_lowercase();
                    if seen.insert(mention.clone()) {
                        found.push(mention);
                    }
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    found
}

/// Compute recipient agent names for a channel message:
/// - `@all` routes to every registered agent except `from`.
/// - Explicit `@agent` mentions route to those registered agents (excluding `from`).
/// - An unmentioned message from an external human/QA sender (`!known_agents.contains(from)`)
///   broadcasts to all registered agents.
/// - An unmentioned message from an agent without `@name` or `@all` routes to nobody.
pub fn route_recipients(from: &str, text: &str, known_agents: &[String]) -> Vec<String> {
    let mentions = mentions_in(text);
    if mentions.iter().any(|m| m == "all") {
        return known_agents
            .iter()
            .filter(|a| a.as_str() != from)
            .cloned()
            .collect();
    }
    let named: Vec<String> = known_agents
        .iter()
        .filter(|a| a.as_str() != from && mentions.iter().any(|m| m == *a))
        .cloned()
        .collect();
    if !named.is_empty() {
        return named;
    }
    let sender_is_agent = known_agents.iter().any(|a| a == from);
    if !sender_is_agent {
        return known_agents.to_vec();
    }
    Vec::new()
}

/// One message recorded in `channel.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarborMessage {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub from: String,
    pub to: Vec<String>,
    pub text: String,
}

pub fn format_channel_message(msg: &HarborMessage) -> String {
    let targets = if msg.to.is_empty() {
        String::new()
    } else {
        format!(
            " -> {}",
            msg.to
                .iter()
                .map(|n| format!("@{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    format!("[#{} @{}{}] {}", msg.seq, msg.from, targets, msg.text)
}

/// Unconsumed channel message sequence persisted in `state.json` for crash redelivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndeliveredItem {
    pub seq: u64,
    #[serde(default)]
    pub from_person: bool,
}

/// Persisted UTC day token counter for `--day-tokens`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayTokenCount {
    pub day: String,
    pub tokens: u64,
}

/// Persisted per-agent metadata in `state.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub name: String,
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub opens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_tokens: Option<DayTokenCount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undelivered: Vec<UndeliveredItem>,
}

impl AgentState {
    pub fn from_spec(spec: AgentSpec, env: BTreeMap<String, String>) -> Self {
        Self {
            name: spec.name,
            runtime: spec.runtime,
            model: spec.model,
            cwd: None,
            env,
            session_id: None,
            opens: 0,
            context_tokens: None,
            day_tokens: None,
            undelivered: Vec::new(),
        }
    }
}

/// Persisted top-level `state.json` document.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarborState {
    pub agents: BTreeMap<String, AgentState>,
}

/// Loopback control endpoint metadata written to `daemon.json` (`0600`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonEndpoint {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub started_ms: u64,
}

/// Structured audit/lifecycle entry appended to `journal.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(flatten)]
    pub details: Map<String, Value>,
}

/// Thread-safe, crash-safe file store for one Harbor run directory.
pub struct HarborStore {
    dir: PathBuf,
    channel_lock: Mutex<u64>,
    journal_lock: Mutex<u64>,
    state_lock: Mutex<()>,
}

impl HarborStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("agents"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        let next_msg = count_jsonl_seq(&dir.join("channel.jsonl"))?;
        let next_journal = count_jsonl_seq(&dir.join("journal.jsonl"))?;
        Ok(Self {
            dir,
            channel_lock: Mutex::new(next_msg),
            journal_lock: Mutex::new(next_journal),
            state_lock: Mutex::new(()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn agent_dir(&self, name: &str) -> PathBuf {
        self.dir.join("agents").join(name)
    }

    pub fn agent_work_dir(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("work")
    }

    pub fn voyage_rel_path(&self, name: &str, open_num: u64) -> String {
        format!("agents/{name}/session-{open_num}.voyage.jsonl")
    }

    pub fn voyage_path(&self, name: &str, open_num: u64) -> PathBuf {
        self.dir.join(self.voyage_rel_path(name, open_num))
    }

    pub fn channel_path(&self) -> PathBuf {
        self.dir.join("channel.jsonl")
    }

    pub fn journal_path(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn daemon_path(&self) -> PathBuf {
        self.dir.join("daemon.json")
    }

    /// Append a message to `channel.jsonl` with synchronous `sync_data()`.
    pub fn append_message(
        &self,
        from: impl Into<String>,
        to: Vec<String>,
        text: impl Into<String>,
    ) -> Result<HarborMessage> {
        let mut seq_guard = self.channel_lock.lock().unwrap();
        *seq_guard += 1;
        let msg = HarborMessage {
            seq: *seq_guard,
            timestamp_ms: timestamp_ms(),
            from: from.into(),
            to,
            text: text.into(),
        };
        append_jsonl_synced(&self.channel_path(), &msg)?;
        Ok(msg)
    }

    /// Read all messages in `channel.jsonl`.
    pub fn messages(&self) -> Result<Vec<HarborMessage>> {
        read_jsonl(&self.channel_path())
    }

    /// Append an audit/lifecycle entry to `journal.jsonl` with synchronous `sync_data()`.
    pub fn append_journal(
        &self,
        kind: impl Into<String>,
        agent: Option<&str>,
        details: Value,
    ) -> Result<JournalEntry> {
        let mut seq_guard = self.journal_lock.lock().unwrap();
        *seq_guard += 1;
        let map = match details {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let entry = JournalEntry {
            seq: *seq_guard,
            timestamp_ms: timestamp_ms(),
            kind: kind.into(),
            agent: agent.map(str::to_owned),
            details: map,
        };
        append_jsonl_synced(&self.journal_path(), &entry)?;
        Ok(entry)
    }

    /// Read all entries in `journal.jsonl`.
    pub fn journal_entries(&self) -> Result<Vec<JournalEntry>> {
        read_jsonl(&self.journal_path())
    }

    /// Load `state.json` (returning an empty `HarborState` if not yet written).
    pub fn load_state(&self) -> Result<HarborState> {
        let _guard = self.state_lock.lock().unwrap();
        match fs::read_to_string(self.state_path()) {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HarborState::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically persist `state.json` via temporary file + `sync_all` + `persist`.
    pub fn save_state(&self, state: &HarborState) -> Result<()> {
        let _guard = self.state_lock.lock().unwrap();
        write_atomic_json(&self.dir, &self.state_path(), state)
    }

    pub fn write_daemon_endpoint(&self, endpoint: &DaemonEndpoint) -> Result<()> {
        write_atomic_json(&self.dir, &self.daemon_path(), endpoint)
    }

    pub fn read_daemon_endpoint(&self) -> Result<Option<DaemonEndpoint>> {
        match fs::read_to_string(self.daemon_path()) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn remove_daemon_endpoint(&self) {
        let _ = fs::remove_file(self.daemon_path());
    }
}

fn count_jsonl_seq(path: &Path) -> Result<u64> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut max_seq = 0u64;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && let Some(seq) = v.get("seq").and_then(Value::as_u64)
        {
            max_seq = max_seq.max(seq);
        }
    }
    Ok(max_seq)
}

fn append_jsonl_synced<T: Serialize>(path: &Path, item: &T) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    let mut bytes = serde_json::to_vec(item)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<T>(&line) {
            out.push(parsed);
        }
    }
    Ok(out)
}

fn write_atomic_json<T: Serialize>(dir: &Path, target: &Path, value: &T) -> Result<()> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".harbor-").suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o600));
    }
    let mut tmp = builder.tempfile_in(dir)?;
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    tmp.as_file_mut().write_all(&bytes)?;
    tmp.as_file_mut().flush()?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(target).map_err(|e| Error::Io(e.error))?;
    Ok(())
}
