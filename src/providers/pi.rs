mod catalog;
mod login;
mod models;
mod sdk;
mod terminal;
use super::{Method, ProviderAuthOptions, ProviderStatus, within};
use crate::{
    Error, InstallationSnapshot, Result, Runtime,
    account::{profile_home, read_profile},
    installation::Probe,
    process::{OUTPUT_LIMIT, OwnedProcess},
};
pub use models::{
    CatalogCancellation, CatalogModel, CatalogProvider, CatalogRefreshOptions,
    CatalogRefreshResult, PiModelCatalog,
};
pub use sdk::PiLoginOptions;
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::Value;
use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::{Duration, SystemTime},
};

/// Files used by Pi's provider and model facades. Relative paths use the
/// installation working directory. Agent settings keep their own directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PiModelRuntimePaths {
    pub auth_path: Option<PathBuf>,
    pub models_path: PiModelsPath,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PiModelsPath {
    /// Read models.json in the Pi agent directory.
    #[default]
    AgentDefault,
    File(PathBuf),
    /// Pass an explicit null modelsPath to Pi.
    Disabled,
}

fn resolve_pi_file(path: &Path, cwd: &Path, home: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(Error::Protocol("Empty Pi configuration path".into()));
    }
    if let Some(value) = path.to_str() {
        if value.starts_with("file://") {
            let invalid = || Error::Protocol("Invalid Pi configuration file URL".into());
            let url = url::Url::parse(value).map_err(|_| invalid())?;
            let encoded = url.path().to_ascii_lowercase();
            if encoded.contains("%2f") || (cfg!(windows) && encoded.contains("%5c")) {
                return Err(invalid());
            }
            return url.to_file_path().map_err(|_| invalid());
        }
        #[cfg(windows)]
        if let Some(tail) = value.strip_prefix('/')
            && !value.contains('\\')
        {
            // Pi accepts Git Bash, Cygwin and WSL drive paths on Windows.
            let tail = tail
                .strip_prefix("mnt/")
                .or_else(|| tail.strip_prefix("cygdrive/"))
                .unwrap_or(tail);
            let (drive, rest) = tail.split_once('/').unwrap_or((tail, ""));
            if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() {
                return Ok(PathBuf::from(format!(
                    "{}:\\{}",
                    drive.to_ascii_uppercase(),
                    rest.replace('/', "\\")
                )));
            }
        }
    }
    Ok(match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => cwd.join(path),
    })
}

/// Pi provider readiness, stored credential metadata and native login/logout.
pub struct PiProviders {
    probe: Probe,
    command: PathBuf,
    agent_dir: PathBuf,
    auth_path: PathBuf,
    models_path: Option<PathBuf>,
    timeout: Duration,
    login_timeout: Duration,
    version: Option<String>,
}
impl Runtime {
    pub async fn pi_provider_auth(&self, options: ProviderAuthOptions) -> Result<PiProviders> {
        self.pi_provider_auth_with_paths(options, PiModelRuntimePaths::default())
            .await
    }
    pub async fn pi_provider_auth_with_paths(
        &self,
        options: ProviderAuthOptions,
        paths: PiModelRuntimePaths,
    ) -> Result<PiProviders> {
        if self.id != "pi" {
            return Err(Error::Unsupported(
                "Pi provider credentials require the Pi runtime".into(),
            ));
        }
        let mut probe = Probe::new(&options.installation)?;
        let installation = within(options.timeout, "Pi installation", async {
            Ok(probe.discover(self).await)
        })
        .await?;
        let InstallationSnapshot::Available {
            command: Some(command),
            version,
            ..
        } = installation
        else {
            return Err(Error::Unsupported(
                "Pi provider credentials require an installed executable".into(),
            ));
        };
        let home = profile_home(&probe)
            .ok_or_else(|| Error::Protocol("Missing Pi home directory".into()))?;
        let agent = match probe
            .get("KAR_PI_AGENT_DIR")
            .filter(|v| !v.is_empty())
            .or_else(|| probe.get("PI_CODING_AGENT_DIR"))
            .filter(|v| !v.is_empty())
            .map(Path::new)
        {
            Some(p) => match p.strip_prefix("~") {
                Ok(rest) => home.join(rest),
                Err(_) => probe.cwd.join(p),
            },
            None => home.join(".pi/agent"),
        };
        probe
            .env
            .insert("PI_CODING_AGENT_DIR".into(), agent.clone().into_os_string());
        let auth_path = match paths.auth_path {
            Some(path) => resolve_pi_file(&path, &probe.cwd, &home)?,
            None => agent.join("auth.json"),
        };
        let models_path = match paths.models_path {
            PiModelsPath::AgentDefault => Some(agent.join("models.json")),
            PiModelsPath::File(path) => Some(resolve_pi_file(&path, &probe.cwd, &home)?),
            PiModelsPath::Disabled => None,
        };
        Ok(PiProviders {
            probe,
            command,
            agent_dir: agent,
            auth_path,
            models_path,
            timeout: options.timeout,
            login_timeout: options.login_timeout,
            version,
        })
    }
}
#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "type")]
    method: Method,
}
struct Entries(Vec<(String, Method)>);
impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Entries;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a Pi credential map")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut items = vec![];
                while let Some((id, meta)) = map.next_entry::<String, Metadata>()? {
                    items.push((id, meta.method));
                }
                Ok(Entries(items))
            }
        }
        d.deserialize_map(Fields)
    }
}
fn stamp(path: &Path) -> io::Result<(u64, SystemTime)> {
    let m = std::fs::metadata(path)?;
    Ok((m.len(), m.modified()?))
}
// Pi truncates and rewrites auth.json under a proper-lockfile directory at its canonical path.
// Retry a changing or locked snapshot without creating a credential file or taking its lock.
fn snapshot(path: &Path) -> Result<Option<Entries>> {
    let file = match path.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(Entries(vec![]))),
        Err(e) => return Err(e.into()),
    };
    let mut lock = file.as_os_str().to_owned();
    lock.push(".lock");
    let lock = Path::new(&lock);
    if lock.try_exists()? {
        return Ok(None);
    }
    let start = stamp(&file)?;
    let contents = read_profile(&file, OUTPUT_LIMIT)?;
    if lock.try_exists()? || start != stamp(&file)? {
        return Ok(None);
    }
    let Some(data) = contents else {
        return Ok(None);
    };
    let data = data.strip_prefix('\u{feff}').unwrap_or(&data);
    if data.is_empty() {
        return Ok(Some(Entries(vec![])));
    }
    serde_json::from_str(data)
        .map(Some)
        .map_err(|_| Error::Protocol("Invalid Pi credential metadata".into()))
}
impl PiProviders {
    async fn host(&self) -> Result<Option<sdk::Host>> {
        let host = sdk::Host::start(self).await?;
        // The terminal always opens the two default filenames in the agent
        // directory. Falling back with independent paths would target another account.
        if host.is_none()
            && (self.auth_path != self.agent_dir.join("auth.json")
                || self.models_path.as_ref() != Some(&self.agent_dir.join("models.json")))
        {
            return Err(super::ProviderErrorKind::UnsupportedInstallation.error(
                "Independent Pi configuration files require an installation with a loadable Pi SDK",
            ));
        }
        Ok(host)
    }
    async fn stored(&self) -> Result<Entries> {
        within(self.timeout, "Pi provider metadata", async {
            loop {
                let p = self.auth_path.clone();
                if let Some(rows) = tokio::task::spawn_blocking(move || snapshot(&p))
                    .await
                    .map_err(|_| Error::Closed("Pi metadata reader stopped".into()))??
                {
                    return Ok(rows);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
    }
    async fn run(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = self.probe.command(&self.command);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        within(self.timeout, "Pi provider command", async {
            Ok(OwnedProcess::spawn(cmd)?.output().await?)
        })
        .await
    }
    /// A metadata-only read never evaluates a configured key command or refreshes OAuth.
    pub async fn list_providers(&self, resolve: bool) -> Result<Vec<ProviderStatus>> {
        let entries = self.stored().await?.0;
        if !resolve {
            let menu = if entries.iter().any(|(_, m)| *m == Method::Oauth) {
                if let Some(host) = sdk::Host::start(self).await? {
                    Some(within(self.timeout, "Pi provider menu", host.menu(self)).await?)
                } else {
                    self.registry().ok().map(|(rows, _)| rows)
                }
            } else {
                None
            };
            return Ok(entries
                .into_iter()
                .map(|(id, m)| describe(&id, m, menu.as_deref()))
                .collect());
        }
        let mut rows = Vec::with_capacity(entries.len());
        for (id, _) in entries {
            rows.push(self.status(&id).await?);
        }
        Ok(rows)
    }
    /// Explicit resolution uses Pi's own credential selection and refresh behavior.
    pub async fn status(&self, id: &str) -> Result<ProviderStatus> {
        if let Some(host) = self.host().await? {
            return within(self.timeout, "Pi provider status", host.status(self, id)).await;
        }
        if id.is_empty() || id.starts_with('-') {
            return Ok(status(id, None));
        }
        let answer = self
            .run(&["auth", "check", "--provider", id, "--json"])
            .await?;
        let data: Value = serde_json::from_slice(&answer.stdout)
            .map_err(|_| Error::Unsupported("Pi auth check --json is unavailable".into()))?;
        if data["provider"] != id {
            return Err(Error::Protocol("Pi reported another provider".into()));
        }
        match (
            answer.status.code(),
            data["status"].as_str(),
            data["authType"].as_str(),
        ) {
            (Some(0), Some("ready"), Some("oauth")) => {
                let menu = self.registry().ok().map(|(rows, _)| rows);
                Ok(describe(id, Method::Oauth, menu.as_deref()))
            }
            (Some(0), Some("ready"), Some("api_key")) => Ok(status(id, Some(Method::ApiKey))),
            (Some(1), Some("not_ready"), _) => Ok(status(id, None)),
            _ => Err(Error::Refused(
                "Pi could not resolve provider credentials".into(),
            )),
        }
    }
    pub async fn logout(&self, id: &str) -> Result<()> {
        if !self.stored().await?.0.iter().any(|(key, _)| key == id) {
            return Ok(());
        }
        if let Some(host) = self.host().await? {
            return within(self.timeout, "Pi provider logout", host.logout(self, id)).await;
        }
        if id.is_empty() || id.trim() != id || id.starts_with('-') {
            return Err(Error::Unsupported("Invalid Pi provider identifier".into()));
        }
        if !self.run(&["auth", "logout", id]).await?.status.success() {
            return Err(Error::Refused("Pi refused provider logout".into()));
        }
        if self.stored().await?.0.iter().any(|(key, _)| key == id) {
            return Err(Error::Refused(
                "Pi still reports a stored credential".into(),
            ));
        }
        Ok(())
    }
}
fn status(id: &str, method: Option<Method>) -> ProviderStatus {
    ProviderStatus {
        provider_id: id.into(),
        configured: method.is_some(),
        method,
        label: None,
        subscription: method.filter(|m| *m == Method::ApiKey).map(|_| false),
    }
}
fn describe(id: &str, method: Method, menu: Option<&[super::LoginProvider]>) -> ProviderStatus {
    let mut row = status(id, Some(method));
    if method == Method::Oauth
        && let Some(providers) = menu
    {
        row.subscription = Some(
            providers
                .iter()
                .find(|p| p.provider_id == id)
                .and_then(|p| p.methods.iter().find(|m| m.method == Method::Oauth))
                .and_then(|m| m.subscription)
                .unwrap_or(false),
        );
    }
    row
}
