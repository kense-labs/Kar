//! A native history namespace, shared by host configuration, discovery and saved-chat reopen.
use crate::{Error, Result, SessionOptions};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::File,
    io::Read,
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum HistoryRuntime {
    Claude,
    Codex,
    Pi,
}
impl HistoryRuntime {
    pub fn runtime(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
        }
    }
    fn home_env(self) -> &'static str {
        match self {
            Self::Codex => "CODEX_HOME",
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Pi => "KAR_PI_AGENT_DIR",
        }
    }
    fn home(self) -> Option<PathBuf> {
        env::var_os(self.home_env())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                (self == Self::Pi)
                    .then(|| {
                        env::var_os("PI_CODING_AGENT_DIR")
                            .filter(|s| !s.is_empty())
                            .map(PathBuf::from)
                    })
                    .flatten()
            })
            .or_else(|| {
                env::home_dir().map(|h| {
                    h.join(match self {
                        Self::Codex => ".codex",
                        Self::Claude => ".claude",
                        Self::Pi => ".pi/agent",
                    })
                })
            })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryInstance {
    pub id: String,
    pub runtime: HistoryRuntime,
    pub home: PathBuf,
    pub sessions: PathBuf,
    pub program: Option<PathBuf>,
}
impl HistoryInstance {
    pub(crate) fn configure(&self, opt: &mut SessionOptions) {
        if self.runtime == HistoryRuntime::Pi {
            opt.pi_session_root = Some(self.sessions.clone());
        }
        opt.env.insert(
            self.runtime.home_env().into(),
            self.home.clone().into_os_string(),
        );
    }
    pub(crate) fn receipt(&self, stamp: &str) -> String {
        if self.id == self.runtime.runtime() {
            stamp.into()
        } else {
            format!("{}/{}", self.id, stamp)
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    instances: BTreeMap<String, InstanceSettings>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceSettings {
    runtime: HistoryRuntime,
    enabled: Option<bool>,
    home: Option<PathBuf>,
    sessions: Option<PathBuf>,
    program: Option<PathBuf>,
}
pub(crate) fn configured() -> Result<Vec<HistoryInstance>> {
    instances(None)
}

/// An explicitly selected instance may have no native history until its first prompt.
pub(crate) fn for_launch(runtime: &str, id: &str) -> Result<HistoryInstance> {
    instances(Some(id))?
        .into_iter()
        .find(|instance| instance.id == id && instance.runtime.runtime() == runtime)
        .ok_or_else(|| Error::Refused(format!("unknown or disabled {runtime} instance {id}")))
}

fn instances(selected: Option<&str>) -> Result<Vec<HistoryInstance>> {
    let mut conf = FileConfig::default();
    if let Some(path) = env::var_os("KAR_AGENT_SESSION_CONFIG").filter(|s| !s.is_empty()) {
        let file = File::open(path)?;
        let mut data = String::new();
        file.take(1024 * 1024 + 1).read_to_string(&mut data)?;
        if data.len() > 1024 * 1024 {
            return Err(Error::Refused(
                "provider instance configuration exceeds 1 MiB".into(),
            ));
        }
        conf = toml::from_str(&data).map_err(|err| {
            Error::Refused(format!("invalid provider instance configuration: {err}"))
        })?;
    }
    let runtimes = [
        HistoryRuntime::Claude,
        HistoryRuntime::Codex,
        HistoryRuntime::Pi,
    ];
    for runtime in runtimes {
        conf.instances
            .entry(runtime.runtime().into())
            .or_insert(InstanceSettings {
                runtime,
                enabled: None,
                home: None,
                sessions: None,
                program: None,
            });
    }
    let mut configs = conf.instances.into_iter().collect::<Vec<_>>();
    configs.sort_by(|(a, x), (b, y)| {
        (x.runtime, a != x.runtime.runtime(), a).cmp(&(y.runtime, b != y.runtime.runtime(), b))
    });
    let mut seen = BTreeSet::new();
    let mut result = vec![];
    for (id, conf) in configs {
        if id.trim().is_empty()
            || id.trim() != id
            || id.chars().any(char::is_control)
            || id.contains('/')
            || runtimes
                .iter()
                .any(|r| r.runtime() == id && *r != conf.runtime)
        {
            return Err(Error::Refused(
                "invalid or reserved provider instance ID".into(),
            ));
        }
        if conf.enabled == Some(false) || selected.is_some_and(|name| name != id) {
            continue;
        }
        if conf.sessions.is_some() && conf.runtime != HistoryRuntime::Pi {
            return Err(Error::Refused(
                "a custom sessions directory is available only for Pi".into(),
            ));
        }
        if [&conf.home, &conf.sessions, &conf.program]
            .into_iter()
            .flatten()
            .any(|p| !p.is_absolute())
        {
            return Err(Error::Refused(
                "provider instance paths must be absolute".into(),
            ));
        }
        let Some(home) = conf.home.or_else(|| conf.runtime.home()) else {
            continue;
        };
        let home = if home.is_absolute() {
            home
        } else {
            env::current_dir()?.join(home)
        };
        let sessions = conf.sessions.unwrap_or_else(|| {
            home.join(if conf.runtime == HistoryRuntime::Claude {
                "projects"
            } else {
                "sessions"
            })
        });
        let sessions = match sessions.canonicalize() {
            Ok(path) => path,
            Err(e) if selected.is_some() && e.kind() == std::io::ErrorKind::NotFound => sessions,
            Err(e) if selected.is_some() => return Err(e.into()),
            Err(_) => continue,
        };
        if seen.insert((conf.runtime, sessions.clone())) {
            result.push(HistoryInstance {
                id,
                runtime: conf.runtime,
                home: home.canonicalize().unwrap_or(home),
                sessions,
                program: conf.program,
            });
        }
    }
    Ok(result)
}
