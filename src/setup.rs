//! Vendor-owned installation methods. Planning does not change the host.
use crate::{Error, Installation, Result, Runtime};
use serde::Serialize;
use std::process::Stdio;
use tokio::process::Command;

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
    pub fn install_plan(&self) -> Result<InstallPlan> {
        let package = match self.id.as_str() {
            "codebuddy" => Some("@tencent-ai/codebuddy-code"),
            "dsh" => Some("@deepseek-ai/dsh"),
            "pi" => Some("@earendil-works/pi-coding-agent"),
            "codex" if cfg!(windows) => Some("@openai/codex"),
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
        match self.installation().await {
            Installation::Missing => {}
            available @ Installation::Available { .. } => return Ok(available),
            Installation::Failed { reason } => return Err(Error::Refused(reason)),
        }
        match self.install_plan()? {
            InstallPlan::Npm { package } => {
                let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
                checked(Command::new(npm).args(["install", "--global", package])).await?;
            }
            InstallPlan::VendorScript {
                url,
                interpreter,
                args,
            } => {
                let temp = tempfile::NamedTempFile::new()?;
                checked(
                    Command::new("curl")
                        .args([
                            "--fail",
                            "--silent",
                            "--show-error",
                            "--location",
                            "--proto",
                            "=https",
                            "--proto-redir",
                            "=https",
                            "--max-time",
                            "120",
                            "--output",
                        ])
                        .arg(temp.path())
                        .arg(url),
                )
                .await?;
                let mut command = Command::new(interpreter);
                command.arg(temp.path()).args(args);
                if self.id == "codex" {
                    command.env("CODEX_NON_INTERACTIVE", "1");
                }
                checked(&mut command).await?;
            }
        }
        crate::clear_installation_cache();
        Ok(self.installation().await)
    }
    /// Invoke the existing executable's own updater.
    pub async fn update(&self) -> Result<Installation> {
        let args: &[&str] = match self.id.as_str() {
            "codex" | "claude" | "grok" | "antigravity" | "factory" => &["update"],
            "opencode" => &["upgrade"],
            "kimi" => &["upgrade", "-y"],
            _ => {
                return Err(Error::Unsupported(format!(
                    "native updater for {}",
                    self.id
                )));
            }
        };
        let mut selected = self.clone();
        selected.program = self.management_command(true).await?;
        checked(Command::new(&selected.program).args(args)).await?;
        crate::clear_installation_cache();
        Ok(selected.installation().await)
    }
}
async fn checked(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Refused(format!("setup command returned {status}")))
    }
}
