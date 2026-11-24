//! The current open's latest attributed root request. Bodies never enter the record stream.
mod files;
pub(crate) mod pi;
#[cfg(test)]
mod tests;
use crate::{Error, Outcome, Result, Runtime, SessionOptions};
use files::{Directory, Entry, Files};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub(crate) const CONFIRM: &str = "kar/claudeRequestCapture";
const SETTING: &str = "OTEL_LOG_RAW_API_BODIES";
const ATTRIBUTION_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequests {
    Latest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Approximate UTC time of the attributed root request.
    pub at: String,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<String>,
}

#[derive(Clone)]
pub(crate) enum RequestReader {
    Claude(Arc<Capture>),
    Pi(Arc<pi::Reader>),
}
impl RequestReader {
    pub async fn read(&self) -> Result<Option<ModelRequest>> {
        match self {
            Self::Claude(r) => r.read().await,
            Self::Pi(r) => r.read().await,
        }
    }
    pub fn close(&self) {
        match self {
            Self::Claude(r) => r.close(),
            Self::Pi(r) => r.close(),
        }
    }
}

pub(crate) struct Owner {
    pub reader: Arc<Capture>,
    _directory: Directory,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.reader.close();
    }
}
impl Owner {
    pub async fn new(
        rt: &Runtime,
        opts: &SessionOptions,
        cwd: &Path,
        id: &str,
        secrets: Vec<String>,
    ) -> Result<Self> {
        let raw = rt.launch_version(opts).await?;
        let verified = raw.to_ascii_lowercase().contains("(claude code)")
            && raw
                .split_whitespace()
                .find_map(|s| semver::Version::parse(s.trim_start_matches('v')).ok())
                .is_some_and(|v| v >= semver::Version::new(2, 1, 274));
        if !verified {
            return Err(Error::Unsupported(
                "model_requests requires Claude Code 2.1.274 or later".into(),
            ));
        }
        let cwd = cwd.to_owned();
        let dir = tokio::task::spawn_blocking(move || files::directory(&cwd))
            .await
            .map_err(|_| Error::Closed("capture initialization stopped".into()))??;
        Self::start(dir, id, secrets)
    }
    fn start(dir: Directory, id: &str, secrets: Vec<String>) -> Result<Self> {
        let path = dir
            .path()
            .to_str()
            .ok_or_else(|| Error::Unsupported("capture requires a UTF-8 temporary path".into()))?;
        let reader = Arc::new(Capture {
            session: id.to_owned(),
            setting: format!("file:{path}"),
            secrets,
            closed: tokio::sync::watch::channel(false).0,
            observed: Default::default(),
            files: Mutex::new(Files::new(dir.path())),
        });
        let weak = Arc::downgrade(&reader);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(125)).await;
                let Some(reader) = weak.upgrade() else { break };
                if reader.is_closed() {
                    break;
                }
                let _ = tokio::task::spawn_blocking(move || reader.poll(false)).await;
            }
        });
        Ok(Self {
            reader,
            _directory: dir,
        })
    }
}
struct Pending {
    id: String,
    turn: u64,
    at: Instant,
}
#[derive(Default)]
struct Observed {
    pending: VecDeque<Pending>,
    latest: Option<(String, bool)>,
    turn: u64,
    provider: Option<String>,
}
enum Attempt {
    Done(Option<ModelRequest>),
    Waiting(Option<Instant>),
}
pub(crate) struct Capture {
    session: String,
    pub setting: String,
    secrets: Vec<String>,
    closed: tokio::sync::watch::Sender<bool>,
    files: Mutex<Files>,
    observed: Mutex<Observed>,
}
fn masked(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .fold(text.to_owned(), |s, secret| s.replace(secret, "[redacted]"))
}
/// Only redact known values. A tool schema may legitimately define an object called "password".
fn mask_body(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(s) => {
            for key in secrets {
                if s.contains(key) {
                    *s = s.replace(key, "[redacted]");
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| mask_body(v, secrets)),
        Value::Object(map) => {
            for (key, mut v) in std::mem::take(map) {
                mask_body(&mut v, secrets);
                map.insert(masked(&key, secrets), v);
            }
        }
        _ => {}
    }
}
impl Capture {
    pub fn close(&self) {
        self.closed.send_replace(true);
    }
    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
    pub fn verify(&self, v: &Value) -> Result<Value> {
        let reported = &v["response"]["response"];
        let actual = &reported["effective"]["env"][SETTING];
        if v["response"]["subtype"] == "success" && actual.as_str() == Some(&self.setting) {
            return Ok(Value::Null);
        }
        let source = reported["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .find(|row| !actual.is_null() && row["settings"]["env"][SETTING] == *actual)
            .and_then(|row| row["source"].as_str())
            .filter(|s| {
                matches!(
                    *s,
                    "userSettings"
                        | "projectSettings"
                        | "localSettings"
                        | "flagSettings"
                        | "policySettings"
                )
            })
            .unwrap_or("unknown");
        Err(Error::Unsupported(format!(
            "Claude could not confirm model_requests (settings source {source})"
        )))
    }
    pub fn provider(&self, v: &Value) {
        self.observed.lock().unwrap().provider = v
            .pointer("/response/response/account/apiProvider")
            .and_then(Value::as_str)
            .map(|s| masked(s, &self.secrets));
    }
    pub fn observe(&self, v: &Value) {
        if v["type"] != "assistant"
            || v["session_id"] != self.session
            || v["parent_tool_use_id"].is_string()
            || v["is_api_error_message"] == true
            || v["message"]["model"] == "<synthetic>"
        {
            return;
        }
        let Some(id) = v["message"]["id"].as_str() else {
            return;
        };
        let mut state = self.observed.lock().unwrap();
        if let Some((last, seen)) = &mut state.latest
            && last == id
        {
            if !*seen {
                *seen = true;
                state.pending.clear();
            }
        } else if !state.pending.iter().any(|p| p.id == id) {
            let turn = state.turn;
            state.pending.push_back(Pending {
                id: id.into(),
                turn,
                at: Instant::now(),
            });
        }
    }
    pub fn ended(&self, outcome: &Result<Outcome>) {
        let mut state = self.observed.lock().unwrap();
        let turn = state.turn;
        if !matches!(outcome, Ok(Outcome::Completed)) {
            state.pending.retain(|p| p.turn != turn);
        }
        state.turn += 1;
    }
    fn poll(&self, read_body: bool) -> Result<Attempt> {
        if self.is_closed() {
            return Ok(Attempt::Done(None));
        }
        let mut files = self.files.lock().unwrap();
        let rows = files
            .rows()
            .map_err(|_| Error::Protocol("could not read Claude request index".into()))?;
        let changed = !rows.is_empty();
        for row in rows {
            let Some(entry) = Entry::from_row(&row) else {
                continue;
            };
            if row["session_id"] != self.session || row["query_source"] != "sdk" {
                files.retire(entry);
                continue;
            }
            if let Some(latest) = &files.latest {
                if latest.id == entry.id {
                    continue;
                }
                if entry.at.zip(latest.at).is_some_and(|(new, old)| new < old) {
                    self.observed
                        .lock()
                        .unwrap()
                        .pending
                        .retain(|p| p.id != entry.id);
                    files.retire(entry);
                    continue;
                }
            }
            {
                let mut state = self.observed.lock().unwrap();
                let pos = state.pending.iter().position(|p| p.id == entry.id);
                if let Some(i) = pos {
                    state.pending.drain(..=i);
                }
                state.latest = Some((entry.id.clone(), pos.is_some()));
            }
            if let Some(old) = files.latest.replace(entry) {
                files.retire(old);
            }
        }
        if changed || files.needs_prune() {
            files
                .prune()
                .map_err(|_| Error::Protocol("could not prune Claude request captures".into()))?;
        }
        if !read_body {
            return Ok(Attempt::Done(None));
        }
        if let Some(at) = self.observed.lock().unwrap().pending.back().map(|p| p.at) {
            return Ok(Attempt::Waiting(Some(at)));
        }
        let Some(entry) = &files.latest else {
            return Ok(Attempt::Done(None));
        };
        let body = entry.read(&files.dir, &self.secrets);
        let state = self.observed.lock().unwrap();
        if let Some(wait) = state.pending.back() {
            return Ok(Attempt::Waiting(Some(wait.at)));
        }
        let Some(mut request) = body else {
            return Ok(Attempt::Waiting(None));
        };
        request.provider.clone_from(&state.provider);
        Ok(Attempt::Done(Some(request)))
    }
    pub async fn read(self: &Arc<Self>) -> Result<Option<ModelRequest>> {
        let start = Instant::now();
        let mut closed = self.closed.subscribe();
        while !*closed.borrow_and_update() {
            let this = self.clone();
            let result = tokio::select! {
                biased;
                _ = closed.changed() => return Ok(None),
                result = tokio::task::spawn_blocking(move || this.poll(true)) => result,
            };
            if self.is_closed() {
                return Ok(None);
            }
            match result.map_err(|_| Error::Closed("request reader stopped".into()))?? {
                Attempt::Done(request) => return Ok(request),
                Attempt::Waiting(at) if at.unwrap_or(start).elapsed() >= ATTRIBUTION_WAIT =>
                    return Err(Error::Timeout("Claude did not provide the complete attributed root model request within two seconds".into())),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        Ok(None)
    }
}
