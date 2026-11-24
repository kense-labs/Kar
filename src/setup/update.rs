//! Read-only native updater queries for a selected, identified executable.
mod feeds;
use crate::{
    Error, InstallationIssue, InstallationSnapshot, Result, Runtime, installation::Probe,
    process::OwnedProcess,
};
use semver::Version;
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateCheck {
    Ok {
        installed: String,
        latest: String,
        update_available: bool,
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    Unavailable {
        reason: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}
impl UpdateCheck {
    fn unavailable(reason: &'static str, source: Option<&str>) -> Self {
        Self::Unavailable {
            reason,
            source: source.map(str::to_owned),
        }
    }
}

pub(super) struct Target {
    pub command: PathBuf,
    pub version: Version,
}

impl Target {
    pub(super) fn new(snapshot: InstallationSnapshot) -> std::result::Result<Self, UpdateCheck> {
        let (command, version) = match snapshot {
            InstallationSnapshot::Available {
                command: Some(command),
                version,
                ..
            }
            | InstallationSnapshot::Failed {
                command,
                installed_version: version,
                code: InstallationIssue::VersionUnsupported,
                ..
            } => (command, version),
            InstallationSnapshot::Available { .. } => {
                return Err(UpdateCheck::unavailable("bundled", None));
            }
            InstallationSnapshot::Missing => return Err(UpdateCheck::unavailable("missing", None)),
            InstallationSnapshot::Failed { .. } => {
                return Err(UpdateCheck::unavailable("readiness_failed", None));
            }
        };
        let version = version
            .as_deref()
            .and_then(release)
            .ok_or_else(|| UpdateCheck::unavailable("version_unreadable", None))?;
        Ok(Self { command, version })
    }
}

pub(super) fn release(raw: &str) -> Option<Version> {
    static RELEASE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bv?(\d+\.\d+\.\d+(?:-[\w.-]+)?)\b").expect("release version pattern")
    });
    RELEASE.captures(raw).and_then(|c| Version::parse(c.get(1)?.as_str()).ok())
}

fn compare(
    installed: &Version,
    latest: Option<&str>,
    verdict: Option<bool>,
    source: &str,
    channel: Option<&str>,
) -> UpdateCheck {
    let Some(latest) = latest.and_then(release) else {
        return UpdateCheck::unavailable("version_unreadable", Some(source));
    };
    UpdateCheck::Ok {
        installed: installed.to_string(),
        latest: latest.to_string(),
        update_available: verdict.unwrap_or(&latest != installed),
        source: source.into(),
        channel: channel.map(str::to_owned),
    }
}

fn codex(installed: &Version, json: &Value, source: &str) -> UpdateCheck {
    let details = &json["checks"]["updates.status"]["details"];
    if details["update action"] == "manual or unknown" {
        return UpdateCheck::unavailable("unmanaged_installation", Some(source));
    }
    let Some(latest) = details["latest version"].as_str() else {
        return UpdateCheck::unavailable("lookup_failed", Some(source));
    };
    let verdict = details["latest version status"].as_str().and_then(|s| {
        if s.contains("newer version is available") {
            Some(true)
        } else if s.contains("not older") {
            Some(false)
        } else {
            None
        }
    });
    compare(installed, Some(latest), verdict, source, None)
}

fn grok(installed: &Version, json: &Value, source: &str) -> UpdateCheck {
    if json["error"].as_str().is_some_and(|s| !s.is_empty())
        || json["latestVersion"].as_str().is_none()
    {
        return UpdateCheck::unavailable("lookup_failed", Some(source));
    }
    compare(
        installed,
        json["latestVersion"].as_str(),
        json["updateAvailable"].as_bool(),
        source,
        json["channel"].as_str(),
    )
}

fn factory(installed: &Version, output: &str, source: &str) -> UpdateCheck {
    if let Some(latest) = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Update available: "))
    {
        return compare(installed, Some(latest), None, source, None);
    }
    if output.contains("Already up to date!") {
        return compare(
            installed,
            Some(&installed.to_string()),
            Some(false),
            source,
            None,
        );
    }
    UpdateCheck::unavailable("lookup_failed", Some(source))
}

pub(super) fn setup_command(probe: &Probe, executable: &Path) -> tokio::process::Command {
    let mut cmd = probe.command(executable);
    for name in probe.env.keys() {
        let key = name.to_string_lossy().to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "npm_config_user_agent" | "npm_execpath" | "npm_node_execpath" | "npm_command"
        ) || key.starts_with("npm_lifecycle_")
            || key.starts_with("npm_package_")
        {
            cmd.env_remove(name);
        }
    }
    cmd.env_remove("KAR_WORKBENCH_TOKEN");
    cmd
}

pub(super) async fn query(id: &str, probe: &Probe, target: &Target) -> Result<UpdateCheck> {
    let (source, args): (&str, &[&str]) = match id {
        "grok" if target.version < Version::new(1, 0, 46) => {
            return Ok(UpdateCheck::unavailable("check_flag_unverified", None));
        }
        "factory" if target.version < Version::new(1, 12, 1) => {
            return Ok(UpdateCheck::unavailable("check_flag_unverified", None));
        }
        "codex" => ("codex doctor --json", &["doctor", "--json"]),
        "grok" => (
            "grok update --check --json",
            &["update", "--check", "--json"],
        ),
        "factory" => ("droid update --check", &["update", "--check"]),
        _ => return Ok(feeds::query(id, probe, target).await),
    };
    let mut cmd = setup_command(probe, &target.command);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedProcess::spawn(cmd)?;
    let output = child.output().await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let report = if id == "factory" {
        if output.status.success() {
            factory(
                &target.version,
                &format!("{text}\n{}", String::from_utf8_lossy(&output.stderr)),
                source,
            )
        } else {
            UpdateCheck::unavailable("lookup_failed", Some(source))
        }
    } else if let Ok(json) = serde_json::from_str(&text) {
        if id == "codex" {
            codex(&target.version, &json, source)
        } else {
            grok(&target.version, &json, source)
        }
    } else {
        UpdateCheck::unavailable("lookup_failed", Some(source))
    };
    Ok(report)
}

impl Runtime {
    /// Probe the selected installation and ask the native release source without running an upgrade.
    pub async fn check_update(&self, timeout: Duration) -> Result<UpdateCheck> {
        if timeout.is_zero() {
            return Err(Error::Protocol("update timeout must be positive".into()));
        }
        tokio::time::timeout(timeout, async {
            let probe = Probe::new(&Default::default())?;
            let target = match Target::new(probe.discover(self).await) {
                Ok(target) => target,
                Err(status) => return Ok(status),
            };
            query(&self.id, &probe, &target).await
        })
        .await
        .map_err(|_| Error::Timeout(format!("{} update check", self.id)))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_updater_verdicts_and_unmanaged_installations() {
        let installed = Version::new(2, 4, 10);
        let details = |action, status| json!({"checks":{"updates.status":{"details":{"update action":action,"latest version":"v2.4.9","latest version status":status}}}});
        assert!(matches!(
            codex(
                &installed,
                &details("automatic", "installed is not older"),
                "source"
            ),
            UpdateCheck::Ok {
                update_available: false,
                ..
            }
        ));
        assert!(matches!(
            codex(
                &installed,
                &details("manual or unknown", "newer version is available"),
                "source"
            ),
            UpdateCheck::Unavailable {
                reason: "unmanaged_installation",
                ..
            }
        ));
        assert!(
            matches!(grok(&installed,&json!({"latestVersion":"2.4.10","channel":"alpha","updateAvailable":true}),"source"),UpdateCheck::Ok {update_available:true,channel:Some(c),..} if c=="alpha")
        );
        assert!(matches!(
            grok(
                &installed,
                &json!({"error":"unavailable","latestVersion":"2.4.10"}),
                "source"
            ),
            UpdateCheck::Unavailable {
                reason: "lookup_failed",
                ..
            }
        ));
        assert!(matches!(
            factory(&installed, "✓ Already up to date!", "source"),
            UpdateCheck::Ok {
                update_available: false,
                ..
            }
        ));
        assert!(
            matches!(factory(&installed,"Update available: v2.5.0","source"),UpdateCheck::Ok {latest,update_available:true,..} if latest=="2.5.0")
        );
    }
}
