use crate::{Error, Protocol, Result, Runtime, Session};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountAction {
    Login,
    Logout,
    Status,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Installation {
    Available { version: String },
    Missing,
    Failed { reason: String },
}
impl Runtime {
    pub async fn installation(&self) -> Installation {
        match self.probe_installation(&Default::default()).await {
            Ok(snapshot) => snapshot.summary(),
            Err(e) => Installation::Failed {
                reason: e.to_string(),
            },
        }
    }
    /// Delegate sign-in to the vendor's CLI in the caller's terminal.
    pub async fn account(&self, action: AccountAction) -> Result<()> {
        use AccountAction::*;
        if action == Login && matches!(self.id.as_str(), "factory" | "codebuddy") {
            return self
                .login(&Default::default(), &mut |e: crate::account::LoginEvent| {
                    Ok(e.present(std::io::stdout())?)
                })
                .await?
                .into_result();
        }
        if action == Logout && self.supports_logout() {
            return self.logout(&Default::default()).await?.into_result();
        }
        let args: &[&str] = match (self.id.as_str(), action) {
            ("codex", Login) | ("kimi" | "qoder" | "cursor", Login) => &["login"],
            ("cursor", Status) => &["status"],
            ("codex", Status) => &["login", "status"],
            ("claude" | "opencode" | "devin", Login) => &["auth", "login"],
            ("opencode", Logout) => &["auth", "logout"],
            ("claude", Status) => &["auth", "status", "--json"],
            ("devin", Status) => &["auth", "status"],
            ("qoder", Status) => &["status", "--output", "json"],
            ("grok", Login) => &["login", "--device-auth"],
            _ => {
                return Err(Error::Unsupported(format!(
                    "native {action:?} for {}",
                    self.id
                )));
            }
        };
        let status = Command::new(self.management_command(false).await?)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .status()
            .await?;
        if !status.success() {
            return Err(Error::Refused(format!("{} returned {status}", self.id)));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Inventory {
    Skills,
    Commands,
    McpServers,
}
/// Native discovery queries available through a runtime or an open session.
#[derive(Clone, Copy, Debug)]
pub enum Catalog {
    Models,
    Inventory(Inventory),
}
impl Runtime {
    /// Read a catalog scoped to a workspace. Factory models and skills can be
    /// discovered before sign-in or session creation. Session-specific settings
    /// select the regular open/resume path.
    pub async fn catalog(&self, kind: Catalog, options: crate::SessionOptions) -> Result<Value> {
        crate::options::environment::validate_removals(&options.unset_env)?;
        let standalone = options.resume.is_none()
            && options.launch_args.is_empty()
            && options.model.is_none()
            && options.settings == crate::SessionSettings::default()
            && options.mcp_servers.is_empty()
            && options.retained_bytes.is_none();
        if standalone
            && self.protocol == Protocol::Codex
            && self.id == "codex"
            && matches!(kind, Catalog::Models)
        {
            return crate::inventory::codex_models::read(
                &self.selected(&options).await?,
                &options,
                false,
            )
            .await;
        }
        if standalone
            && matches!(
                self.id.as_str(),
                "kimi"
                    | "qwen"
                    | "devin"
                    | "antigravity"
                    | "cursor"
                    | "claude"
                    | "grok"
                    | "opencode"
                    | "pi"
            )
            && matches!(kind, Catalog::Inventory(Inventory::Skills))
        {
            let opts = crate::inventory::InventoryOptions {
                installation: options.installation_options(),
                timeout: Some(options.handshake_timeout),
            };
            return Ok(serde_json::to_value(self.skills(&opts).await?)?);
        }
        if standalone
            && self.id == "cursor"
            && self.protocol == Protocol::Acp
            && matches!(kind, Catalog::Models)
            && let Some(models) =
                crate::cursor_models::catalog(&self.selected(&options).await?, &options).await?
        {
            return Ok(models);
        }
        if standalone
            && self.id == "opencode"
            && self.protocol == Protocol::Acp
            && matches!(kind, Catalog::Models)
        {
            return Ok(serde_json::to_value(
                crate::options::opencode::models::list(self, &options).await?,
            )?);
        }
        if standalone
            && self.id == "grok"
            && self.protocol == Protocol::Acp
            && matches!(kind, Catalog::Models)
        {
            return crate::grok::catalog(&self.selected(&options).await?, &options).await;
        }
        if standalone
            && self.id == "factory"
            && self.protocol == Protocol::Factory
            && matches!(
                kind,
                Catalog::Models | Catalog::Inventory(Inventory::Skills)
            )
        {
            return crate::factory::inventory(&self.selected(&options).await?, &options, kind)
                .await;
        }
        if standalone
            && self.protocol == Protocol::Morph
            && matches!(kind, Catalog::Models)
            && options.env.is_empty()
            && options.unset_env.is_empty()
        {
            return crate::community::Morph::from_env(&self.program)?
                .models()
                .await;
        }
        let session = self.open(options).await?;
        let result = session.catalog(kind).await;
        let closed = session.close().await;
        let catalog = result?;
        closed?;
        Ok(catalog)
    }
}
impl Session {
    pub async fn catalog(&self, kind: Catalog) -> Result<Value> {
        match kind {
            Catalog::Models => self.models().await,
            Catalog::Inventory(kind) => self.inventory(kind).await,
        }
    }
    pub fn current_context(&self) -> Option<crate::ContextUsage> {
        self.snapshot().context_usage
    }
    /// On-demand Claude inventory. The private native response is never recorded.
    pub async fn context_breakdown(&self) -> Result<crate::ContextBreakdown> {
        if self.protocol() != Protocol::Claude {
            return Err(Error::Unsupported(
                "context category breakdown on this adapter".into(),
            ));
        }
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.rpc("get_context_usage", json!({"detail":"full"})),
        )
        .await
        .map_err(|_| Error::Timeout("reading Claude context breakdown".into()))??;
        Ok(serde_json::from_value(output)?)
    }
    /// Native context-window report. Qwen labels estimated token counts in the response.
    pub async fn context_usage(&self) -> Result<Value> {
        if self.protocol() == Protocol::Factory {
            return self.rpc("droid.get_context_stats", json!({})).await;
        }
        if self.protocol() == Protocol::Qwen
            && self.info().pointer("/capabilities/can_get_context_usage")
                == Some(&Value::Bool(true))
        {
            return self
                .rpc("get_context_usage", json!({"show_details":false}))
                .await;
        }
        if let Some(context) = self.current_context() {
            return Ok(serde_json::to_value(context)?);
        }
        Err(Error::Unsupported(
            "runtime has not reported context usage".into(),
        ))
    }
    pub async fn account_usage(&self) -> Result<Value> {
        match self.protocol() {
            Protocol::Codex => self.rpc("account/rateLimits/read", json!({})).await,
            Protocol::Claude => self.rpc("get_usage", json!({})).await,
            _ => Err(Error::Unsupported("account quota via this protocol".into())),
        }
    }
    pub async fn inventory(&self, kind: Inventory) -> Result<Value> {
        match (self.protocol(), kind) {
            (Protocol::Factory, Inventory::Skills) => crate::factory::project(
                Catalog::Inventory(kind),
                &self
                    .rpc("droid.list_skills", json!({"includeContent":false}))
                    .await?,
            ),
            (Protocol::Factory, Inventory::Commands) => {
                self.rpc("droid.list_commands", json!({})).await
            }
            (Protocol::Dsh, Inventory::Commands) => self.rpc("commands", json!({})).await,
            (Protocol::Dsh, Inventory::Skills) => self.rpc("skills", json!({})).await,
            (Protocol::Codex, Inventory::Skills) => {
                self.rpc("skills/list", json!({"cwds":[self.cwd()]})).await
            }
            (Protocol::Codex, Inventory::McpServers) => {
                self.pages("mcpServerStatus/list", json!({"detail":"full","limit":100}))
                    .await
            }
            (Protocol::Claude, Inventory::Commands) => self
                .info()
                .get("commands")
                .cloned()
                .ok_or_else(|| Error::Unsupported("runtime did not report commands".into())),
            (Protocol::Claude, Inventory::McpServers) => self.rpc("mcp_status", json!({})).await,
            (Protocol::Pi, Inventory::Commands) => self.rpc("get_commands", json!({})).await,
            _ => Err(Error::Unsupported(format!(
                "{kind:?} through this protocol"
            ))),
        }
    }
}
