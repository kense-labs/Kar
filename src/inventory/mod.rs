//! Credential-free workspace discovery that does not open a model session.
mod antigravity;
mod claude;
mod codex;
pub(crate) mod codex_models;
mod cursor;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
mod dsh;
mod entries;
pub use entries::{CommandEntry, McpServerEntry, SkillEntry, SkillScope, ToolEntry};
mod grok;
mod opencode;
mod pi;
mod skills;
use crate::{
    Error, InstallationIssue, InstallationOptions, InstallationSnapshot, Result, Runtime,
    installation::Probe,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;

#[derive(Clone, Default)]
pub struct InventoryOptions {
    pub installation: InstallationOptions,
    pub timeout: Option<Duration>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InventoryScope {
    Workspace { cwd: PathBuf },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum View {
    Discovered,
    Context,
    Registered,
    McpOnly,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedCode {
    NativeQueryUnavailable,
    TransportUnavailable,
    ScopeUnavailable,
    InstallationUnsupported,
    VersionUnsupported,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableCode {
    Timeout,
    QueryFailed,
    NotAuthenticated,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum InventoryResult<T> {
    Ok {
        scope: InventoryScope,
        observed_at: String,
        view: View,
        items: Vec<T>,
        partial: bool,
    },
    Unsupported {
        code: UnsupportedCode,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        installed_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        required_version: Option<String>,
    },
    Unavailable {
        code: UnavailableCode,
        reason: String,
    },
}
impl<T> InventoryResult<T> {
    fn unsupported(code: UnsupportedCode, reason: &str) -> Self {
        Self::Unsupported {
            code,
            reason: reason.into(),
            installed_version: None,
            required_version: None,
        }
    }
    fn unavailable(code: UnavailableCode, reason: &str) -> Self {
        Self::Unavailable {
            code,
            reason: reason.into(),
        }
    }
    fn observed(cwd: &Path, items: Vec<T>, partial: bool, view: View) -> Self {
        Self::Ok {
            scope: InventoryScope::Workspace { cwd: cwd.into() },
            observed_at: format!("{:.3}", jiff::Timestamp::now()),
            view,
            items,
            partial,
        }
    }
}
impl Runtime {
    /// Return native skill metadata scoped to one working directory.
    pub async fn skills(&self, opts: &InventoryOptions) -> Result<InventoryResult<SkillEntry>> {
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        if self.id == "dsh" {
            return dsh::skills(self, opts).await;
        }
        if self.id == "cursor" {
            let probe = Probe::new(&opts.installation)?;
            return inventory_deadline(opts.timeout, async |_| cursor::skills(self, &probe).await)
                .await;
        }
        if self.id == "antigravity" {
            return self.query_workspace(opts, true, antigravity::skills).await;
        }
        if self.id == "opencode" {
            return self.query_workspace(opts, true, opencode::skills).await;
        }
        self.workspace_inventory(
            opts,
            matches!(
                self.id.as_str(),
                "codex" | "factory" | "kimi" | "qwen" | "devin" | "claude" | "grok" | "pi"
            ),
            View::Discovered,
            skills::read,
        )
        .await
    }
    /// Read the commands that the selected executable advertises at startup.
    pub async fn commands(&self, opts: &InventoryOptions) -> Result<InventoryResult<CommandEntry>> {
        self.workspace_inventory(
            opts,
            matches!(self.id.as_str(), "claude" | "pi"),
            View::Discovered,
            async |id, probe, path, _| {
                if id == "pi" {
                    pi::commands(probe, path).await
                } else {
                    claude::commands(probe, path).await
                }
            },
        )
        .await
    }
    /// Discover native MCP servers in a workspace without creating a conversation.
    pub async fn mcp_servers(
        &self,
        opts: &InventoryOptions,
    ) -> Result<InventoryResult<McpServerEntry>> {
        self.workspace_inventory(
            opts,
            matches!(self.id.as_str(), "codex" | "claude" | "grok"),
            View::Discovered,
            servers,
        )
        .await
    }
    /// List native tools and their schemas. The view identifies the registry or MCP scope.
    pub async fn tools(&self, opts: &InventoryOptions) -> Result<InventoryResult<ToolEntry>> {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        if self.id == "pi" {
            return pi::registry::tools(self, opts).await;
        }
        self.workspace_inventory(
            opts,
            matches!(self.id.as_str(), "codex" | "claude"),
            View::McpOnly,
            tools,
        )
        .await
    }
    async fn workspace_inventory<T>(
        &self,
        opts: &InventoryOptions,
        supported: bool,
        view: View,
        query: impl AsyncFnOnce(&str, &Probe, &Path, Instant) -> Result<(Vec<T>, bool)>,
    ) -> Result<InventoryResult<T>> {
        self.query_workspace(opts, supported, async |probe, path, _, end| {
            let (items, partial) = query(&self.id, probe, path, end).await?;
            Ok(InventoryResult::observed(&probe.cwd, items, partial, view))
        })
        .await
    }

    async fn query_workspace<T>(
        &self,
        opts: &InventoryOptions,
        supported: bool,
        query: impl AsyncFnOnce(&Probe, &Path, Option<&str>, Instant) -> Result<InventoryResult<T>>,
    ) -> Result<InventoryResult<T>> {
        let mut probe = Probe::new(&opts.installation)?;
        if self.id == "antigravity" {
            probe
                .env
                .insert("AGY_CLI_DISABLE_AUTO_UPDATE".into(), "true".into());
        }
        inventory_deadline(opts.timeout, async |end| {
            if !supported {
                return Ok(InventoryResult::unsupported(
                    UnsupportedCode::NativeQueryUnavailable,
                    "This runtime has no native reader for this inventory",
                ));
            }
            let (path, version) = match probe.discover(self).await {
                InstallationSnapshot::Available {
                    command: Some(p),
                    version,
                    ..
                } => (p, version),
                InstallationSnapshot::Failed {
                    code: InstallationIssue::VersionUnsupported,
                    installed_version,
                    required_version,
                    ..
                } => {
                    return Ok(InventoryResult::Unsupported {
                        code: UnsupportedCode::VersionUnsupported,
                        reason: "Unsupported native version".into(),
                        installed_version,
                        required_version,
                    });
                }
                _ => {
                    return Ok(InventoryResult::unsupported(
                        UnsupportedCode::InstallationUnsupported,
                        "The selected executable is unavailable",
                    ));
                }
            };
            query(&probe, &path, version.as_deref(), end).await
        })
        .await
    }
}

async fn inventory_deadline<T>(
    timeout: Option<Duration>,
    read: impl AsyncFnOnce(Instant) -> Result<InventoryResult<T>>,
) -> Result<InventoryResult<T>> {
    let timeout = timeout.unwrap_or(Duration::from_secs(15));
    let end = Instant::now()
        .checked_add(timeout)
        .filter(|_| !timeout.is_zero())
        .ok_or_else(|| Error::Protocol("Invalid inventory query timeout".into()))?;
    match tokio::time::timeout_at(end, read(end)).await {
        Ok(Ok(result)) => Ok(result),
        Err(_) | Ok(Err(Error::Timeout(_))) => Ok(InventoryResult::unavailable(
            UnavailableCode::Timeout,
            "Inventory query timed out",
        )),
        Ok(Err(Error::Unsupported(_))) => Ok(InventoryResult::unsupported(
            UnsupportedCode::NativeQueryUnavailable,
            "Native inventory query is unavailable",
        )),
        Ok(Err(_)) => Ok(InventoryResult::unavailable(
            UnavailableCode::QueryFailed,
            "Native inventory query failed",
        )),
    }
}

fn rpc(mut frame: serde_json::Value, method: &'static str) -> Result<serde_json::Value> {
    match frame["error"]["code"].as_i64() {
        Some(-32601) => Err(Error::Unsupported(method.into())),
        Some(code) => Err(Error::ProviderRpc { method, code }),
        None if frame["result"].is_object() => Ok(frame["result"].take()),
        _ => Err(Error::Protocol("Invalid native inventory response".into())),
    }
}

async fn servers(
    id: &str,
    probe: &Probe,
    path: &Path,
    _end: Instant,
) -> Result<(Vec<McpServerEntry>, bool)> {
    match id {
        "claude" => claude::servers(probe, path).await,
        "grok" => grok::servers(probe, path).await,
        _ => codex::servers(probe, path).await,
    }
}
async fn tools(
    id: &str,
    probe: &Probe,
    path: &Path,
    _end: Instant,
) -> Result<(Vec<ToolEntry>, bool)> {
    if id == "claude" {
        claude::tools(probe, path).await
    } else {
        codex::tools(probe, path).await
    }
}

fn invalid() -> Error {
    Error::Protocol("Invalid native inventory metadata".into())
}
fn rows(v: &Value) -> Result<&Vec<Value>> {
    v.as_array().ok_or_else(invalid)
}
fn field(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_owned)
}

fn name(row: &Value) -> Result<String> {
    field(row, "name").ok_or_else(invalid)
}
