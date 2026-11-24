//! Vendor-owned installation methods. Planning does not change the host.
mod update;
mod plan;
pub use plan::{InstallCommand, InstallPreview};
use crate::{Error, Installation, Result, Runtime};
use serde::Serialize;
use std::{process::Stdio, time::Duration};
use tokio::process::Command;
pub use update::UpdateCheck;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum InstallPlan {
    VendorScript {
        url: &'static str,
        interpreter: &'static str,
        args: &'static [&'static str],
    },
    Npm {
        package: &'static str,
    },
}
impl Runtime {
    pub fn install_lines(&self) -> &'static [&'static str] {
        if self.id == "opencode" {
            &["v1", "v2"]
        } else {
            &[]
        }
    }

    pub fn install_plan(&self) -> Result<InstallPlan> {
        self.install_plan_for(None)
    }

    pub fn install_plan_for(&self, line: Option<&str>) -> Result<InstallPlan> {
        let lines = self.install_lines();
        match line {
            None if !lines.is_empty() => {
                return Err(Error::Unsupported(format!(
                    "{} needs a release line; choose --line {}",
                    self.id,
                    lines.join(" or ")
                )));
            }
            Some(line) if !lines.contains(&line) => {
                return Err(Error::Unsupported(format!(
                    "{} has no release line {line:?}",
                    self.id
                )));
            }
            _ => {}
        }
        let package = match self.id.as_str() {
            "codebuddy" => Some("@tencent-ai/codebuddy-code"),
            "dsh" => Some("@deepseek-ai/dsh"),
            "pi" => Some("@earendil-works/pi-coding-agent"),
            "codex" if cfg!(windows) => Some("@openai/codex"),
            "opencode" if cfg!(windows) && line == Some("v1") => Some("opencode-ai"),
            "opencode" if cfg!(windows) => Some("@opencode/cli"),
            "claude" if cfg!(windows) => Some("@anthropic-ai/claude-code"),
            "qwen" if cfg!(windows) => Some("@qwen-code/qwen-code"),
            _ => None,
        };
        if let Some(package) = package {
            return Ok(InstallPlan::Npm { package });
        }
        if !cfg!(unix) {
            return Err(Error::Unsupported(format!(
                "installer for {} on this platform",
                self.id
            )));
        }
        let (url, interpreter, args): (&str, &str, &[&str]) = match self.id.as_str() {
            "codex" => ("https://chatgpt.com/codex/install.sh", "sh", &[]),
            "claude" => ("https://claude.ai/install.sh", "bash", &[]),
            "opencode" if line == Some("v1") => ("https://opencode.ai/install", "bash", &[]),
            "opencode" => ("https://opencode.ai/v2/install", "bash", &[]),
            "grok" => ("https://x.ai/cli/install.sh", "bash", &[]),
            "kimi" => ("https://code.kimi.com/kimi-code/install.sh", "bash", &[]),
            "devin" => ("https://cli.devin.ai/install.sh", "bash", &[]),
            "antigravity" => ("https://antigravity.google/cli/install.sh", "bash", &[]),
            "qwen" => (
                "https://qwen-code-assets.oss-cn-hangzhou.aliyuncs.com/installation/install-qwen-standalone.sh",
                "bash",
                &["--method", "standalone", "--no-modify-path"],
            ),
            "qoder" => ("https://qoder.com/install", "bash", &["--skip-path"]),
            "factory" => ("https://app.factory.ai/cli", "sh", &[]),
            "cursor" => ("https://cursor.com/install", "bash", &[]),
            _ => return Err(Error::Unsupported(format!("installer for {}", self.id))),
        };
        Ok(InstallPlan::VendorScript {
            url,
            interpreter,
            args,
        })
    }
    pub async fn install(&self) -> Result<Installation> {
        self.install_with(None, Duration::from_secs(600)).await
    }

    pub async fn install_with(
        &self,
        line: Option<&str>,
        timeout: Duration,
    ) -> Result<Installation> {
        if timeout.is_zero() {
            return Err(Error::Protocol("installer timeout must be positive".into()));
        }
        let probe = crate::installation::Probe::new(&Default::default())?;
        let installation = probe.discover(self).await.summary();
        match installation {
            Installation::Missing if probe.entry(self).1 => {
                return Err(Error::Refused(format!(
                    "the selected {} executable is missing; its installer writes to a vendor-selected location",
                    self.id
                )));
            }
            Installation::Missing => {}
            available @ Installation::Available { .. } => return Ok(available),
            Installation::Failed { reason } => return Err(Error::Refused(reason)),
        }
        let plan = self.install_plan_for(line)?;
        let run = tokio::time::timeout(timeout, self.run_installer(plan)).await;
        crate::clear_installation_cache();
        let run = run.map_err(|_| Error::Timeout(format!("{} installer", self.id)))?;
        match (probe.discover(self).await.summary(), run) {
            (installed @ Installation::Available { .. }, _) => Ok(installed),
            (_, Err(error)) => Err(error),
            (Installation::Failed { reason }, _) => Err(Error::Refused(reason)),
            (Installation::Missing, _) => Err(Error::Refused(format!(
                "{} installer finished without a discoverable executable",
                self.id
            ))),
        }
    }

    async fn run_installer(&self, plan: InstallPlan) -> Result<()> {
        match plan {
            InstallPlan::Npm { package } => {
                let mut command = Command::new(plan::npm_program());
                command.args(["install", "--global", package]);
                checked(command).await?;
            }
            InstallPlan::VendorScript {
                url,
                interpreter,
                args,
            } => {
                let temp = tempfile::NamedTempFile::new()?;
                let mut download = Command::new("curl");
                download
                    .args(plan::download_args())
                    .arg(temp.path())
                    .arg(url);
                checked(download).await?;
                let mut command = Command::new(interpreter);
                command.arg(temp.path()).args(args);
                if self.id == "codex" {
                    command.env("CODEX_NON_INTERACTIVE", "1");
                }
                checked(command).await?;
            }
        }
        Ok(())
    }
    /// Ask the executable's own updater and verify the version of this same path afterwards.
    pub async fn update(&self) -> Result<Installation> {
        self.update_with(Duration::from_secs(600)).await
    }

    pub async fn update_with(&self, timeout: Duration) -> Result<Installation> {
        if timeout.is_zero() {
            return Err(Error::Protocol("update timeout must be positive".into()));
        }
        let run = tokio::time::timeout(timeout, self.run_update()).await;
        crate::clear_installation_cache();
        run.map_err(|_| Error::Timeout(format!("{} updater", self.id)))?
    }

    async fn run_update(&self) -> Result<Installation> {
        let probe = crate::installation::Probe::new(&Default::default())?;
        let before = update::Target::new(probe.discover(self).await).map_err(|reason| {
            Error::Unsupported(format!("{} upgrade target: {reason:?}", self.id))
        })?;
        let args: &[&str] = match self.id.as_str() {
            "codex" | "claude" | "grok" | "antigravity" | "factory" => &["update"],
            "opencode" if before.version.major == 2 => &["upgrade"],
            "opencode" => {
                return Err(Error::Unsupported(
                    "the native OpenCode updater requires v2".into(),
                ));
            }
            "kimi" if before.version >= semver::Version::new(0, 43, 0) => &["upgrade", "-y"],
            "kimi" => {
                return Err(Error::Unsupported(
                    "this Kimi upgrade requires a terminal".into(),
                ));
            }
            _ => {
                return Err(Error::Unsupported(format!(
                    "native updater for {}",
                    self.id
                )));
            }
        };
        match update::query(&self.id, &probe, &before).await? {
            UpdateCheck::Ok {
                update_available: false,
                ..
            } => {
                return Ok(Installation::Available {
                    version: before.version.to_string(),
                });
            }
            UpdateCheck::Unavailable {
                reason: "unmanaged_installation" | "package_manager" | "updates_disabled",
                ..
            } => {
                return Err(Error::Unsupported(format!(
                    "this {} installation is not managed by the native updater",
                    self.id
                )));
            }
            _ => {}
        }
        let mut command = update::setup_command(&probe, &before.command);
        command.args(args);
        if self.id == "claude" {
            crate::options::claude_environment(&mut command, &Default::default());
        }
        let run = checked(command).await;
        crate::clear_installation_cache();
        let mut selected = self.clone();
        selected.program = before.command;
        let observed = probe.discover(&selected).await;
        if let Installation::Available { version } = observed.clone().summary()
            && let Ok(after) = update::Target::new(observed)
            && before.version != after.version
        {
            return Ok(Installation::Available { version });
        }
        run?;
        Err(Error::Refused(format!(
            "{} updater did not produce a new usable version of the selected executable",
            self.id
        )))
    }
}
async fn checked(mut cmd: Command) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .env_remove("KAR_WORKBENCH_TOKEN");
    let status = crate::process::OwnedProcess::spawn(cmd)?.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Refused(format!("setup command returned {status}")))
    }
}
