mod actor;
mod console;
mod projection;
pub(crate) use projection::covers_suffix;
pub(crate) mod recovery;

use crate::{Error, Protocol, Result, Session, SessionOptions, stream::Journal};
use console::{Console, Lease};
use reqwest::Method;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[derive(Clone)]
enum Source {
    Local {
        program: PathBuf,
        state: PathBuf,
        config: Option<PathBuf>,
    },
    Attached(Console),
}

/// A Mister Morph local Console. Sessions on the same directory share a process.
#[derive(Clone)]
pub struct Morph {
    source: Source,
}

impl Morph {
    pub fn local(program: impl Into<PathBuf>, state_directory: impl Into<PathBuf>) -> Self {
        Self {
            source: Source::Local {
                program: program.into(),
                state: state_directory.into(),
                config: None,
            },
        }
    }
    /// Attach to an existing loopback Runtime API, as published in console/runtime.json.
    pub fn attach(url: &str, token: &str) -> Result<Self> {
        Ok(Self {
            source: Source::Attached(Console::new(url, token)?),
        })
    }
    /// Select a native YAML configuration for a Console this adapter starts.
    pub fn with_config(mut self, config: impl AsRef<Path>) -> Result<Self> {
        match &mut self.source {
            Source::Local { config: path, .. } => *path = Some(std::fs::canonicalize(config)?),
            Source::Attached(_) => {
                return Err(Error::Unsupported(
                    "changing the configuration of an attached Console".into(),
                ));
            }
        }
        Ok(self)
    }
    pub fn from_env(program: impl AsRef<Path>) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let expand = |path: PathBuf| match (path.strip_prefix("~"), &home) {
            (Ok(rest), Some(home)) => home.join(rest),
            _ => path,
        };
        let config = std::env::var_os("MISTER_MORPH_CONFIG")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .map(&expand);
        let default = home.as_ref().map(|h| h.join(".morph"));
        let state = std::env::var_os("MISTER_MORPH_FILE_STATE_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        // Older Morph versions also accept a top-level file_state_dir in config.yaml.
        let yaml = config
            .clone()
            .or_else(|| default.as_ref().map(|d| d.join("config.yaml")));
        let configured = yaml
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|yaml| {
                yaml.lines()
                    .find_map(|line| line.strip_prefix("file_state_dir:"))
                    .map(|s| {
                        s.split(" #")
                            .next()
                            .unwrap_or(s)
                            .trim()
                            .trim_matches(['\'', '"'])
                            .to_owned()
                    })
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
            });
        let state = state.or(configured).or(default).ok_or_else(|| {
            Error::Protocol("set MISTER_MORPH_FILE_STATE_DIR or provide a home directory".into())
        })?;
        let mut runtime = Self::local(program.as_ref(), expand(state));
        if let Some(path) = config {
            runtime = runtime.with_config(path)?;
        }
        Ok(runtime)
    }

    async fn lease(&self) -> Result<Lease> {
        match &self.source {
            Source::Local {
                program,
                state,
                config,
            } => Lease::acquire(program, state, config.as_deref()).await,
            Source::Attached(client) => Ok(Lease::attached(client.clone())),
        }
    }
    /// Query native LLM profiles without creating a topic or attaching a workspace.
    pub async fn models(&self) -> Result<Value> {
        let lease = tokio::time::timeout(std::time::Duration::from_secs(20), self.lease())
            .await
            .map_err(|_| Error::Timeout("discovering Morph Console".into()))??;
        let result = models(&lease.console).await;
        lease.release().await;
        result
    }
    pub async fn open(&self, mut options: SessionOptions) -> Result<Session> {
        let journal = Journal::new(options.retained_bytes)?;
        options.settings.validate(crate::Protocol::Morph, "morph")?;
        if !options.env.is_empty()
            || !options.unset_env.is_empty()
            || !options.mcp_servers.is_empty()
        {
            return Err(Error::Unsupported(
                "Morph's shared Console owns its environment and tools".into(),
            ));
        }
        options.cwd = tokio::fs::canonicalize(&options.cwd).await?;
        if !options.cwd.is_dir() {
            return Err(Error::Protocol(
                "Morph workspace must be a directory".into(),
            ));
        }
        let timeout = options.handshake_timeout;
        let (lease, id, info) = tokio::time::timeout(timeout, async {
            let lease = self.lease().await?;
            let console = &lease.console;
            if let Some(model) = &options.model
                && !models(console)
                    .await?
                    .as_array()
                    .expect("profiles array")
                    .iter()
                    .any(|p| p["id"] == *model)
            {
                return Err(Error::Refused(format!("Morph has no LLM profile {model}")));
            }
            let info = if let Some(id) = &options.resume {
                console.call(Method::GET, &["topics", id], None).await?
            } else {
                console
                    .call(Method::POST, &["topics"], Some(&json!({})))
                    .await?
            };
            let id = match &options.resume {
                Some(id) => {
                    if let Some(actual) = info["id"].as_str()
                        && actual != id
                    {
                        return Err(Error::Protocol("Morph resumed a different topic".into()));
                    }
                    id.clone()
                }
                None => required_id(&info)?.to_owned(),
            };
            console
                .call(
                    Method::PUT,
                    &["workspace"],
                    Some(&json!({"topic_id":id,"workspace_dir":options.cwd})),
                )
                .await?;
            Ok::<_, Error>((lease, id, info))
        })
        .await
        .map_err(|_| Error::Timeout("opening Morph topic".into()))??;
        journal.bind_root(&id);
        let (tx, rx) = mpsc::channel(64);
        let process_root = lease.process_root();
        let actor = actor::Actor::new(&options, id.clone(), lease, journal.clone());
        tokio::spawn(actor.run(rx));
        let mut session = Session::remote(Protocol::Morph, options, tx, journal, id, info);
        session.process_root = process_root;
        Ok(session)
    }
}
fn required_id(value: &Value) -> Result<&str> {
    value["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("Morph response omitted its id".into()))
}
async fn models(console: &Console) -> Result<Value> {
    let value = console
        .call(Method::GET, &["llm", "profiles"], None)
        .await?;
    let mut result = Vec::new();
    for profile in value
        .get("default")
        .into_iter()
        .chain(value["items"].as_array().into_iter().flatten())
    {
        if let Some(id) = profile["name"].as_str().filter(|s| !s.is_empty())
            && !result.iter().any(|p: &Value| p["id"] == id)
        {
            let mut entry = json!({"id":id});
            if let Some(model) = profile["model"].as_str() {
                entry["resolvedId"] = model.into();
            }
            result.push(entry);
        }
    }
    Ok(Value::Array(result))
}
