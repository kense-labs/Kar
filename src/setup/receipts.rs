//! Public results retain the command output separately from installation discovery.
use super::{
    InstallPlan,
    host::{self, HostInstallPlan, InstallRefusal},
    update::{self, UpdateCheck},
};
use crate::{
    Error, Installation, InstallationOptions, InstallationSnapshot, Result, Runtime,
    installation::Probe, process::OwnedProcess,
};
use serde::Serialize;
use std::{collections::VecDeque, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::Instant,
};

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstallResult {
    Installed {
        installation: InstallationSnapshot,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<String>,
        exit_code: Option<i32>,
        output: String,
    },
    AlreadyInstalled {
        installation: InstallationSnapshot,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<String>,
    },
    Failed {
        exit_code: Option<i32>,
        output: String,
    },
    Unsupported(InstallRefusal),
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpgradeResult {
    Upgraded {
        from: String,
        to: String,
        exit_code: Option<i32>,
        output: String,
    },
    Current {
        version: String,
        check: UpdateCheck,
    },
    Unchanged {
        exit_code: i32,
        version: String,
        output: String,
    },
    Failed {
        exit_code: Option<i32>,
        output: String,
    },
    Unsupported {
        reason: UpgradeReason,
        detail: String,
    },
}
#[derive(Debug, Serialize, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeReason {
    UnsupportedInstallation,
    RequiresTerminal,
}

struct Run {
    code: Option<i32>,
    output: String,
}
async fn drain<R: AsyncRead + Unpin>(mut reader: R, out: &mut VecDeque<u8>) -> std::io::Result<()> {
    const LIMIT: usize = 1024 * 1024;
    let mut chunk = [0; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        out.drain(..(out.len() + n).saturating_sub(LIMIT));
        out.extend(&chunk[..n]);
    }
}

async fn run(mut cmd: Command, deadline: Instant, timeout: Duration, what: &str) -> Run {
    let note = || {
        format!(
            "\n[Kar: stopped the {what} after {} ms]\n",
            timeout.as_millis()
        )
    };
    if Instant::now() >= deadline {
        return Run {
            code: None,
            output: note(),
        };
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match OwnedProcess::spawn(cmd) {
        Ok(c) => c,
        Err(e) => {
            return Run {
                code: None,
                output: format!("[Kar: could not start the {what}: {e}]\n"),
            };
        }
    };
    let stdout = child.inner().stdout.take().expect("piped stdout");
    let stderr = child.inner().stderr.take().expect("piped stderr");
    let (mut out, mut err) = (VecDeque::new(), VecDeque::new());
    let result = tokio::time::timeout_at(deadline, async {
        let (_, _, status) = tokio::try_join!(
            drain(stdout, &mut out),
            drain(stderr, &mut err),
            child.wait()
        )?;
        std::io::Result::Ok(status)
    })
    .await;
    let mut output = String::from_utf8_lossy(out.make_contiguous()).into_owned();
    output.push_str(&String::from_utf8_lossy(err.make_contiguous()));
    let code = match result {
        Ok(Ok(status)) => status.code(),
        Ok(Err(e)) => {
            output.push_str(&format!("\n[Kar: {what} I/O failed: {e}]\n"));
            None
        }
        Err(_) => {
            output.push_str(&note());
            None
        }
    };
    Run { code, output }
}

fn observed_line(rt: &Runtime, snapshot: &InstallationSnapshot) -> Option<String> {
    if rt.id != "opencode" {
        return None;
    }
    let InstallationSnapshot::Available {
        version: Some(version),
        ..
    } = snapshot
    else {
        return None;
    };
    let line = format!("v{}", update::release(version)?.major);
    rt.install_lines()
        .iter()
        .any(|entry| entry.line == line)
        .then_some(line)
}

fn deadline(timeout: Duration) -> Result<Instant> {
    if timeout.is_zero() {
        return Err(Error::Protocol("setup timeout must be positive".into()));
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::Protocol("setup timeout is out of range".into()))
}

async fn version_now(probe: &Probe, command: &Path) -> Option<semver::Version> {
    let mut cmd = update::setup_command(probe, command);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedProcess::spawn(cmd).ok()?;
    let out = tokio::time::timeout(Duration::from_secs(30), child.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    update::release(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| update::release(&String::from_utf8_lossy(&out.stderr)))
}

impl Runtime {
    /// Inspect required programs, the current user's write access, and native destination settings.
    pub async fn inspect_install(
        &self,
        line: Option<&str>,
        options: &InstallationOptions,
    ) -> Result<HostInstallPlan> {
        let probe = Probe::new(options)?;
        Ok(match host::inspect(self, &probe, line, None).await {
            Ok((preview, locations)) => HostInstallPlan::checked(preview, locations),
            Err(refusal) => HostInstallPlan::Unsupported(refusal),
        })
    }

    pub async fn install_receipt(
        &self,
        line: Option<&str>,
        timeout: Duration,
        options: &InstallationOptions,
    ) -> Result<InstallResult> {
        deadline(timeout)?;
        let probe = Probe::new(options)?;
        let before = probe.discover(self).await;
        if matches!(before, InstallationSnapshot::Available { .. }) {
            return Ok(InstallResult::AlreadyInstalled {
                line: observed_line(self, &before),
                installation: before,
            });
        }
        let (preview, _) = match host::inspect(self, &probe, line, Some(&before)).await {
            Ok(ready) => ready,
            Err(r) => return Ok(InstallResult::Unsupported(r)),
        };
        let due = deadline(timeout)?;
        let observed = match preview.plan {
            InstallPlan::Npm { package } => {
                let mut cmd = update::setup_command(&probe, Path::new(super::plan::npm_program()));
                cmd.args(["install", "--global", package]);
                run(cmd, due, timeout, "installer").await
            }
            InstallPlan::VendorScript {
                url,
                interpreter,
                args,
            } => {
                let temp = tempfile::NamedTempFile::new()?;
                let mut curl = update::setup_command(&probe, Path::new("curl"));
                curl.args(super::plan::download_args())
                    .arg(temp.path())
                    .arg(url);
                let mut fetched = run(curl, due, timeout, "installer").await;
                if fetched.code == Some(0) {
                    let mut cmd = update::setup_command(&probe, Path::new(interpreter));
                    cmd.arg(temp.path()).args(args);
                    if self.id == "codex" {
                        cmd.env("CODEX_NON_INTERACTIVE", "1");
                    }
                    let installed = run(cmd, due, timeout, "installer").await;
                    fetched.code = installed.code;
                    fetched.output.push_str(&installed.output);
                }
                fetched
            }
        };
        crate::clear_installation_cache();
        let after = probe.discover(self).await;
        if matches!(after, InstallationSnapshot::Available { .. }) {
            Ok(InstallResult::Installed {
                line: observed_line(self, &after),
                installation: after,
                exit_code: observed.code,
                output: observed.output,
            })
        } else {
            Ok(InstallResult::Failed {
                exit_code: observed.code,
                output: observed.output,
            })
        }
    }

    pub async fn upgrade_receipt(
        &self,
        timeout: Duration,
        options: &InstallationOptions,
    ) -> Result<UpgradeResult> {
        deadline(timeout)?;
        let probe = Probe::new(options)?;
        let unavailable =
            |reason: UpgradeReason, detail: String| UpgradeResult::Unsupported { reason, detail };
        use UpgradeReason::{RequiresTerminal, UnsupportedInstallation};
        let snapshot = probe.discover(self).await;
        let command = match &snapshot {
            InstallationSnapshot::Available {
                command: Some(command),
                ..
            }
            | InstallationSnapshot::Failed {
                code: crate::InstallationIssue::VersionUnsupported,
                command,
                ..
            } => command.clone(),
            InstallationSnapshot::Available { .. } => {
                return Ok(unavailable(
                    UnsupportedInstallation,
                    "bundled runtime moves with its host".into(),
                ));
            }
            InstallationSnapshot::Failed { reason, .. } => {
                return Ok(unavailable(UnsupportedInstallation, reason.clone()));
            }
            InstallationSnapshot::Missing => {
                return Ok(unavailable(
                    UnsupportedInstallation,
                    "the selected executable was not found".into(),
                ));
            }
        };
        // Discovery can use a cached snapshot. Read the selected path again immediately before mutation.
        let fresh = version_now(&probe, &command).await;
        let target = fresh
            .clone()
            .map(|version| update::Target {
                command: command.clone(),
                version,
            })
            .or_else(|| update::Target::new(snapshot).ok());
        let mut before = fresh.clone();
        let args: &[&str] = match self.id.as_str() {
            "opencode" if fresh.is_none() => {
                return Ok(UpgradeResult::Failed {
                    exit_code: None,
                    output: "could not identify the selected OpenCode release line".into(),
                });
            }
            "codex" | "claude" | "grok" | "antigravity" | "factory" => &["update"],
            "opencode" if fresh.as_ref().is_some_and(|v| v.major == 2) => &["upgrade"],
            "kimi"
                if target
                    .as_ref()
                    .is_none_or(|t| t.version >= semver::Version::new(0, 43, 0)) =>
            {
                &["upgrade", "-y"]
            }
            "kimi" => {
                return Ok(unavailable(
                    RequiresTerminal,
                    "this Kimi version requires an interactive terminal for upgrade".into(),
                ));
            }
            _ => {
                return Ok(unavailable(
                    UnsupportedInstallation,
                    format!(
                        "no verified native updater for the selected {} release",
                        self.id
                    ),
                ));
            }
        };
        if self.id != "opencode"
            && let Some(target) = &target
        {
            let check =
                tokio::time::timeout(timeout, update::query(&self.id, &probe, target)).await;
            match check {
                Ok(Ok(
                    check @ UpdateCheck::Ok {
                        update_available: false,
                        ..
                    },
                )) => {
                    return Ok(UpgradeResult::Current {
                        version: target.version.to_string(),
                        check,
                    });
                }
                Ok(Ok(UpdateCheck::Ok { installed, .. })) if before.is_none() => {
                    before = update::release(&installed);
                }
                Ok(Ok(UpdateCheck::Unavailable {
                    reason: "package_manager" | "unmanaged_installation" | "updates_disabled",
                    ..
                })) if self.id == "claude" => {
                    return Ok(unavailable(
                        UnsupportedInstallation,
                        "Claude's current installation method does not permit native upgrade"
                            .into(),
                    ));
                }
                _ => (),
            }
        }
        let mut cmd = update::setup_command(&probe, &command);
        cmd.args(args);
        if self.id == "claude" {
            crate::options::claude_environment(&mut cmd, &Default::default());
        }
        let result = run(cmd, deadline(timeout)?, timeout, "updater").await;
        crate::clear_installation_cache();
        let after = version_now(&probe, &command).await;
        if let (Some(before), Some(after)) = (&before, &after)
            && before != after
        {
            return Ok(UpgradeResult::Upgraded {
                from: before.to_string(),
                to: after.to_string(),
                exit_code: result.code,
                output: result.output,
            });
        }
        match after {
            Some(version) if result.code == Some(0) => Ok(UpgradeResult::Unchanged {
                exit_code: 0,
                version: version.to_string(),
                output: result.output,
            }),
            _ => Ok(UpgradeResult::Failed {
                exit_code: result.code,
                output: result.output,
            }),
        }
    }

    pub async fn install(&self) -> Result<Installation> {
        self.install_with(None, Duration::from_secs(600)).await
    }
    pub async fn install_with(
        &self,
        line: Option<&str>,
        timeout: Duration,
    ) -> Result<Installation> {
        match self
            .install_receipt(line, timeout, &Default::default())
            .await?
        {
            InstallResult::Installed { installation, .. }
            | InstallResult::AlreadyInstalled { installation, .. } => Ok(installation.summary()),
            InstallResult::Unsupported(r) => Err(Error::Unsupported(r.detail)),
            InstallResult::Failed { exit_code, output } => Err(Error::Refused(format!(
                "installer exited {exit_code:?} without a discoverable executable: {output}"
            ))),
        }
    }
    pub async fn update(&self) -> Result<Installation> {
        self.update_with(Duration::from_secs(600)).await
    }
    pub async fn update_with(&self, timeout: Duration) -> Result<Installation> {
        match self.upgrade_receipt(timeout, &Default::default()).await? {
            UpgradeResult::Upgraded { to, .. } => match self
                .probe_installation(&Default::default())
                .await?
                .summary()
            {
                i @ Installation::Available { .. } => Ok(i),
                _ => Ok(Installation::Available { version: to }),
            },
            UpgradeResult::Current { version, .. } => Ok(Installation::Available { version }),
            UpgradeResult::Unchanged {
                version, output, ..
            } => Err(Error::Refused(format!(
                "updater kept version {version}: {output}"
            ))),
            UpgradeResult::Failed { exit_code, output } => Err(Error::Refused(format!(
                "updater exited {exit_code:?}: {output}"
            ))),
            UpgradeResult::Unsupported { detail, .. } => Err(Error::Unsupported(detail)),
        }
    }
}
