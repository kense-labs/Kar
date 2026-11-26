//! Local installation discovery and noninteractive vendor readiness probes.
mod cache;
mod host;
mod paths;
use crate::{Error, Installation, Protocol, Result, Runtime, SessionOptions};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

/// An override applies only to this probe. A None entry removes the inherited value.
#[derive(Default, Debug, Clone)]
pub struct InstallationOptions {
    pub env: BTreeMap<OsString, Option<OsString>>,
    pub cwd: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationSource {
    Executable,
    Bundled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationIssue {
    ReadinessFailed,
    UnrecognizedExecutable,
    VersionUnsupported,
    DependencyMissing,
    UnsupportedHost,
    UnsupportedPlatform,
    UnsupportedNodeVersion,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstallationSnapshot {
    Available {
        via: InstallationSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        shadowed: Vec<PathBuf>,
    },
    Missing,
    Failed {
        command: PathBuf,
        code: InstallationIssue,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installed_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required_version: Option<String>,
    },
}
impl InstallationSnapshot {
    pub fn command(&self) -> Option<&Path> {
        match self {
            Self::Available { command, .. } => command.as_deref(),
            Self::Failed { command, .. } => Some(command),
            Self::Missing => None,
        }
    }
    /// Compact installation status, matching `Runtime::installation` without another probe.
    pub fn summary(self) -> Installation {
        match self {
            Self::Available { version, .. } => Installation::Available {
                version: version.unwrap_or_default(),
            },
            Self::Missing => Installation::Missing,
            Self::Failed { reason, .. } => Installation::Failed { reason },
        }
    }
}
pub(crate) struct Probe {
    pub(crate) cwd: PathBuf,
    pub(crate) env: BTreeMap<OsString, OsString>,
}
struct Candidate {
    command: PathBuf,
    shadowed: Vec<PathBuf>,
}
impl Candidate {
    fn available(&self, version: Option<String>) -> InstallationSnapshot {
        InstallationSnapshot::Available {
            via: InstallationSource::Executable,
            command: Some(self.command.clone()),
            version,
            shadowed: self.shadowed.clone(),
        }
    }
    fn new(command: PathBuf) -> Self {
        Self {
            command,
            shadowed: vec![],
        }
    }
    fn fail(&self, code: InstallationIssue, reason: &str) -> InstallationSnapshot {
        InstallationSnapshot::Failed {
            command: self.command.clone(),
            code,
            reason: reason.into(),
            installed_version: None,
            required_version: None,
        }
    }
}
impl InstallationOptions {
    pub(crate) fn effective_env(&self) -> BTreeMap<OsString, OsString> {
        self.apply(std::env::vars_os().collect(), cfg!(windows))
    }
    fn apply(
        &self,
        mut env: BTreeMap<OsString, OsString>,
        windows: bool,
    ) -> BTreeMap<OsString, OsString> {
        for (name, value) in &self.env {
            if windows {
                env.retain(|k, _| !crate::options::environment::same_name(k, name, true));
            } else {
                env.remove(name);
            }
            if let Some(v) = value {
                env.insert(name.clone(), v.clone());
            }
        }
        env
    }
}
pub(crate) use paths::account_home;
impl Probe {
    pub(crate) fn for_session(options: &SessionOptions) -> Result<Self> {
        crate::options::environment::validate_removals(&options.unset_env)?;
        Self::new(&options.installation_options())
    }
    pub(crate) fn new(options: &InstallationOptions) -> Result<Self> {
        let cwd = match &options.cwd {
            Some(p) => std::path::absolute(p)?,
            None => std::env::current_dir()?,
        };
        let env = options.effective_env();
        Ok(Self { cwd, env })
    }
    pub(crate) fn get(&self, key: &str) -> Option<&OsStr> {
        #[cfg(windows)]
        {
            self.env
                .iter()
                .find(|(k, _)| crate::options::environment::same_name(k, OsStr::new(key), true))
                .map(|(_, v)| v.as_os_str())
        }
        #[cfg(not(windows))]
        {
            self.env.get(OsStr::new(key)).map(|s| s.as_os_str())
        }
    }
    pub(crate) fn tool(&self, name: &str) -> Option<PathBuf> {
        self.locate(Path::new(name), true).map(|c| c.command)
    }
    pub(crate) fn command(&self, program: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(program);
        cmd.current_dir(&self.cwd)
            .env_clear()
            .envs(&self.env)
            .env_remove("KAR_WORKBENCH_TOKEN");
        cmd
    }
    async fn run(&self, program: &Path, args: &[&str], timeout: u64) -> Result<Output> {
        let mut cmd = self.command(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::process::OwnedProcess::spawn(cmd)?;
        tokio::time::timeout(Duration::from_secs(timeout), child.output())
            .await
            .map_err(|_| Error::Timeout("installation probe".into()))?
            .map_err(Into::into)
    }
    pub(crate) fn entry<'a>(&'a self, rt: &'a Runtime) -> (&'a Path, bool) {
        let Some(default) = crate::runtime::registered_command(&rt.id) else {
            return (&rt.program, true);
        };
        let key = format!("KAR_{}_BIN", rt.id.to_ascii_uppercase());
        // A custom Runtime or explicit --bin takes precedence over discovery options.
        if rt.program != Path::new(default)
            && std::env::var_os(&key).as_deref() != Some(rt.program.as_os_str())
        {
            return (&rt.program, true);
        }
        match self.get(&key).filter(|s| !s.is_empty()) {
            Some(value) => (Path::new(value), true),
            None => (Path::new(default), false),
        }
    }
    pub(crate) fn companion(&self, rt: &Runtime, file: &str) -> Option<PathBuf> {
        let (entry, pinned) = self.entry(rt);
        let selected = self.locate(entry, pinned).or_else(|| {
            if pinned {
                return None;
            }
            self.fallbacks(&rt.id)
                .into_iter()
                .find_map(|path| self.locate(&path, false))
        })?;
        let command = selected.command.canonicalize().ok()?;
        let path = command.parent()?.join(file);
        path.is_file().then_some(path)
    }
    pub(crate) async fn discover(&self, rt: &Runtime) -> InstallationSnapshot {
        if rt.protocol == Protocol::Cybion {
            return InstallationSnapshot::Available {
                via: InstallationSource::Bundled,
                command: None,
                version: Some(format!("bundled adapter {}", env!("CARGO_PKG_VERSION"))),
                shadowed: vec![],
            };
        }
        let (entry, pinned) = self.entry(rt);
        let mut candidates: Vec<_> = self.locate(entry, pinned).into_iter().collect();
        if !pinned {
            for path in self.fallbacks(&rt.id) {
                if let Some(found) = self.locate(&path, false)
                    && !candidates.iter().any(|c| c.command == found.command)
                {
                    candidates.push(found);
                }
            }
            // Only query npm's local prefix when direct CodeBuddy discovery was empty.
            if rt.id == "codebuddy"
                && candidates.is_empty()
                && let Some(npm) = self.locate(Path::new("npm"), true)
                && let Ok(p) = crate::setup::host::npm_prefix(self, &npm.command).await
                && let Some(found) = self.locate(
                    &p.join(if cfg!(windows) {
                        "codebuddy.cmd"
                    } else {
                        "bin/codebuddy"
                    }),
                    true,
                )
            {
                candidates.push(found);
            }
        }
        let mut failed = None;
        for candidate in candidates {
            let identity = cache::Identity::read(&candidate.command);
            let key = cache::Key::new(self, &rt.id, &candidate);
            if let Some(version) = identity.as_ref().and_then(|file| key.version(file)) {
                return candidate.available(Some(version));
            }
            let result = self.verify(rt, &candidate).await;
            if let (
                Some(before),
                InstallationSnapshot::Available {
                    version: Some(version),
                    ..
                },
            ) = (identity, &result)
            {
                key.remember(before, version);
            }
            match result {
                InstallationSnapshot::Available { .. } => return result,
                InstallationSnapshot::Failed {
                    code: InstallationIssue::VersionUnsupported,
                    ..
                } if !matches!(
                    failed,
                    Some(InstallationSnapshot::Failed {
                        code: InstallationIssue::VersionUnsupported,
                        ..
                    })
                ) =>
                {
                    failed = Some(result)
                }
                _ if failed.is_none() => failed = Some(result),
                _ => {}
            }
        }
        failed.unwrap_or(InstallationSnapshot::Missing)
    }
    async fn verify(&self, rt: &Runtime, c: &Candidate) -> InstallationSnapshot {
        use InstallationIssue::*;
        if let Some(failure) = host::missing_interpreter(self, c) {
            return failure;
        }
        let id = rt.id.as_str();
        let (args, identity): (&[&str], &str) = match id {
            "codex" => (
                &["app-server", "--help"],
                r"(?i)\bUsage:\s+codex(?:\.exe)?\s+app-server\b",
            ),
            "factory" => (
                &["exec", "--help"],
                r"(?i)\bUsage:\s+droid(?:\.exe)?\s+exec\b",
            ),
            "kimi" => (
                &["acp", "--help"],
                r"(?im)^\s*Usage:\s+kimi(?:\.exe)?\s+acp\b",
            ),
            "cursor" => (
                &["acp", "--help"],
                r"(?i)\bUsage:\s+(?:agent|cursor-agent)(?:\.exe)?\s+acp\b",
            ),
            "grok" => (
                &["agent", "stdio", "--help"],
                r"(?i)\bUsage:\s+grok(?:\.exe)?\s+agent\s+stdio\b",
            ),
            "qwen" => (
                &["--help"],
                r"(?i)\bUsage:\s+qwen(?:\.exe)?\b|\bQwen Code\b",
            ),
            "qoder" => (
                &["--help"],
                r"(?i)\bUsage:\s+qodercli(?:\.exe)?\b|\bQoder CLI\b",
            ),
            "dsh" => (
                &["--help"],
                r"(?i)\bUsage:\s+dsh(?:\.exe)?\b|\bDeepSeek Harness\b",
            ),
            "antigravity" => (
                &["--help"],
                r"(?s)\bUsage of\b.*(?:--conversation|--print-timeout|--remote-agent|\bantigravity-cli\b|Start the Agent Client Protocol \(ACP\) stdio server)",
            ),
            "devin" => (&["acp", "--help"], ""),
            "codebuddy" => (
                &["--help"],
                r"(?im)^\s*(?:Usage:\s+(?:codebuddy|cbc)(?:\.exe)?(?:\|(?:codebuddy|cbc))?\b|CodeBuddy Code\b)",
            ),
            "opencode" => (
                &["--help"],
                r"(?im)^\s*(?:opencode(?:\.exe)?\s+(?:acp\b|<subcommand>|\[project\])|OpenCode command line interface\s*$)",
            ),
            _ => (&[], ""),
        };
        let timeout = if matches!(id, "kimi" | "devin" | "antigravity") {
            30
        } else {
            15
        };
        if !args.is_empty() {
            let out = match self.run(&c.command, args, timeout).await {
                Ok(out) => out,
                Err(error) => {
                    return host::spawn_failure(c, error, "could not run the readiness probe");
                }
            };
            if !out.status.success() {
                return host::reported_failure(c, &out)
                    .unwrap_or_else(|| c.fail(ReadinessFailed, "the readiness command failed"));
            }
            let help = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if !identity.is_empty()
                && !Regex::new(identity)
                    .expect("vendor help regex")
                    .is_match(&help)
            {
                return c.fail(
                    UnrecognizedExecutable,
                    "help did not identify the expected vendor CLI",
                );
            }
        }
        let version_arg = if rt.protocol == Protocol::Morph {
            "version"
        } else {
            "--version"
        };
        let out = match self.run(&c.command, &[version_arg], timeout).await {
            Ok(out) => out,
            Err(error) => return host::spawn_failure(c, error, "could not run the version probe"),
        };
        if let Some(failure) = host::reported_failure(c, &out) {
            return failure;
        }
        let version = out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|s| !s.is_empty());
        let pattern = match id {
            "codex" => r"(?i)^codex(?:-cli)?\s+\d+\.\d+\.\d+",
            "claude" => r"(?i)\(Claude Code\)",
            "grok" => r"(?i)^grok\s+\d+\.\d+\.\d+",
            "devin" => r"^devin\s+\d+\.\d+\.\d+(?:-[A-Za-z\d.-]+)?(?:\s+\([0-9a-f]+\))?$",
            _ => "",
        };
        if !pattern.is_empty() {
            let Some(v) = &version else {
                return c.fail(ReadinessFailed, "the version command failed");
            };
            if !Regex::new(pattern)
                .expect("vendor version regex")
                .is_match(v)
            {
                return c.fail(
                    UnrecognizedExecutable,
                    "version did not identify the expected vendor CLI",
                );
            }
        }
        if let Some(required) = requirement(id) {
            let Some(raw) = &version else {
                return c.fail(ReadinessFailed, "the version command failed");
            };
            let parsed = read_version(id, raw);
            if !parsed.as_ref().is_some_and(|v| supported(id, v)) {
                return InstallationSnapshot::Failed {
                    command: c.command.clone(),
                    code: VersionUnsupported,
                    reason: format!("{id} requires {required}"),
                    installed_version: parsed.map(|v| v.to_string()),
                    required_version: Some(required.into()),
                };
            }
        } else if args.is_empty() && version.is_none() {
            return c.fail(ReadinessFailed, "the version command failed");
        }
        c.available(version)
    }
}
fn requirement(id: &str) -> Option<&'static str> {
    Some(match id {
        "opencode" => ">=1.18.30 <2.0.0 or >=2.0.27 <3.0.0",
        "qoder" => "stable >=1.1.78",
        "pi" => "stable >=1.4.0",
        "antigravity" => "stable >=1.5.0",
        "dsh" => ">=0.2.1-rc.5",
        _ => return None,
    })
}
pub(crate) fn read_version(id: &str, raw: &str) -> Option<semver::Version> {
    let value = match id {
        "opencode" => raw
            .strip_prefix("opencode ")
            .unwrap_or(raw)
            .trim_start_matches('v'),
        "qoder" | "dsh" => {
            return raw
                .split_whitespace()
                .find_map(|s| semver::Version::parse(s.trim_start_matches('v')).ok());
        }
        _ => raw,
    };
    semver::Version::parse(value).ok()
}
fn supported(id: &str, v: &semver::Version) -> bool {
    use semver::Version;
    match id {
        "opencode" => match v.major {
            1 => v >= &Version::new(1, 18, 30),
            2 => v >= &Version::new(2, 0, 27),
            _ => false,
        },
        "qoder" => v.pre.is_empty() && v >= &Version::new(1, 1, 78),
        "pi" => v.pre.is_empty() && v.build.is_empty() && v >= &Version::new(1, 4, 0),
        "antigravity" => v.pre.is_empty() && v >= &Version::new(1, 5, 0),
        "dsh" => v >= &Version::parse("0.2.1-rc.5").unwrap(),
        _ => true,
    }
}
pub fn clear_installation_cache() {
    cache::clear();
}
impl Runtime {
    pub(crate) async fn management_command(&self, upgrade: bool) -> Result<PathBuf> {
        match self
            .probe_installation(&InstallationOptions::default())
            .await?
        {
            InstallationSnapshot::Available {
                command: Some(path),
                ..
            } => Ok(path),
            InstallationSnapshot::Failed {
                command,
                code: InstallationIssue::VersionUnsupported,
                ..
            } if upgrade => Ok(command),
            InstallationSnapshot::Failed { reason, .. } => Err(Error::Unsupported(reason)),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} executable not found", self.id),
            )
            .into()),
        }
    }
    pub async fn probe_installation(
        &self,
        options: &InstallationOptions,
    ) -> Result<InstallationSnapshot> {
        Ok(Probe::new(options)?.discover(self).await)
    }
    /// Bind a launch to a concrete path, including when the app has a different PATH from a terminal.
    pub(crate) async fn selected(&self, options: &SessionOptions) -> Result<Runtime> {
        Ok(self.selected_with_version(options).await?.0)
    }
    pub(crate) async fn selected_with_version(
        &self,
        options: &SessionOptions,
    ) -> Result<(Runtime, Option<String>)> {
        if self.protocol == Protocol::Cybion {
            return Ok((self.clone(), None));
        }
        let probe = Probe::for_session(options)?;
        let (entry, pinned) = probe.entry(self);
        // Explicit launchers can implement a compatible protocol, including a caller-supplied wrapper.
        if pinned {
            let mut rt = self.clone();
            rt.program = match probe.locate(entry, true) {
                Some(found) => found.command,
                None if self.protocol == Protocol::Morph => entry.to_owned(),
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("{} executable not found", self.id),
                    )
                    .into());
                }
            };
            return Ok((rt, None));
        }
        match probe.discover(self).await {
            InstallationSnapshot::Available {
                command: Some(command),
                version,
                ..
            } => {
                let mut rt = self.clone();
                rt.program = command;
                Ok((rt, version))
            }
            InstallationSnapshot::Missing if self.protocol == Protocol::Morph => {
                Ok((self.clone(), None))
            }
            InstallationSnapshot::Failed { reason, .. } => Err(Error::Unsupported(reason)),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} executable not found", self.id),
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod environment_tests {
    use super::*;

    #[test]
    fn windows_removes_aliases_and_posix_preserves_other_spellings() {
        let inherited = BTreeMap::from([
            ("Path".into(), "one".into()),
            ("PATH".into(), "two".into()),
            ("Token".into(), "private".into()),
            ("Ärea".into(), "old".into()),
            ("KEEP".into(), "unchanged".into()),
        ]);
        let changes = InstallationOptions {
            env: BTreeMap::from([
                ("path".into(), Some("new".into())),
                ("TOKEN".into(), None),
                ("ärea".into(), None),
                ("MISSING".into(), None),
                ("EMPTY".into(), Some("".into())),
            ]),
            ..Default::default()
        };
        let windows = changes.apply(inherited.clone(), true);
        assert_eq!(
            windows,
            BTreeMap::from([
                ("path".into(), "new".into()),
                ("KEEP".into(), "unchanged".into()),
                ("EMPTY".into(), "".into())
            ])
        );
        let posix = changes.apply(inherited.clone(), false);
        assert_eq!(posix.len(), inherited.len() + 2);
        assert_eq!(
            posix.get(OsStr::new("Token")),
            inherited.get(OsStr::new("Token"))
        );
        assert_eq!(
            posix.get(OsStr::new("Ärea")),
            inherited.get(OsStr::new("Ärea"))
        );
    }
}
