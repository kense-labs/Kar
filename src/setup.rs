//! Vendor-owned installation methods. Planning does not change the host.
pub(crate) mod host;
mod receipts;
mod update;
pub use host::{HostInstallPlan, InstallReason, InstallRefusal, InstallStep};
pub use receipts::{InstallResult, UpgradeReason, UpgradeResult};
mod plan;
pub use plan::{InstallCommand, InstallPreview};
use crate::{Error, Result, Runtime};
use serde::Serialize;
pub use update::UpdateCheck;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct InstallLine {
    pub line: &'static str,
    pub description: &'static str,
}

impl PartialEq<&str> for InstallLine {
    fn eq(&self, other: &&str) -> bool {
        self.line == *other
    }
}

impl PartialEq<str> for InstallLine {
    fn eq(&self, other: &str) -> bool {
        self.line == other
    }
}

impl AsRef<str> for InstallLine {
    fn as_ref(&self) -> &str {
        self.line
    }
}

const OPENCODE_INSTALL_LINES: &[InstallLine] = &[
    InstallLine {
        line: "v1",
        description: "OpenCode 1: the opencode-ai 1.x releases",
    },
    InstallLine {
        line: "v2",
        description: "OpenCode 2: the @opencode/cli 2.x releases, with a new plugin and server API",
    },
];

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
    pub fn install_lines(&self) -> &'static [InstallLine] {
        if self.id == "opencode" {
            OPENCODE_INSTALL_LINES
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
                    lines
                        .iter()
                        .map(|entry| entry.line)
                        .collect::<Vec<_>>()
                        .join(" or ")
                )));
            }
            Some(line) if !lines.iter().any(|entry| entry.line == line) => {
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
}
