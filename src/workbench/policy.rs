//! Host-wide / MDM `ManagedPolicy` loader and enforcement for macOS (`/Library/Application Support/Kar/policy.json`,
//! `/etc/kar/policy.json`, or `KAR_MANAGED_POLICY_FILE`).

use super::Workbench;
use crate::{Error, Result};
use axum::{Json, Router, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedPolicy {
    pub allowed_runtimes: Option<BTreeSet<String>>,
    pub blocked_runtimes: BTreeSet<String>,
    pub allow_webhook_tasks: bool,
    pub allow_public_relay_tunnel: bool,
    pub allow_guest_sharing: bool,
    pub allow_mcp_outbound_webhooks: bool,
    pub allow_ssh_workspaces: bool,
    pub telemetry_mode: String,
    pub update_channel: String,
}

impl Default for ManagedPolicy {
    fn default() -> Self {
        Self {
            allowed_runtimes: None,
            blocked_runtimes: BTreeSet::new(),
            allow_webhook_tasks: true,
            allow_public_relay_tunnel: true,
            allow_guest_sharing: true,
            allow_mcp_outbound_webhooks: true,
            allow_ssh_workspaces: true,
            telemetry_mode: "allowed".into(),
            update_channel: "stable".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PolicySnapshot {
    pub managed: bool,
    pub source_path: Option<PathBuf>,
    pub policy: ManagedPolicy,
}

impl ManagedPolicy {
    pub fn load() -> Result<PolicySnapshot> {
        let candidates: Vec<PathBuf> = if let Some(override_path) =
            std::env::var_os("KAR_MANAGED_POLICY_FILE").filter(|s| !s.is_empty())
        {
            vec![PathBuf::from(override_path)]
        } else {
            vec![
                PathBuf::from("/Library/Application Support/Kar/policy.json"),
                PathBuf::from("/etc/kar/policy.json"),
            ]
        };

        for path in candidates {
            if let Ok(meta) = fs::symlink_metadata(&path)
                && meta.is_file()
            {
                let bytes = fs::read(&path)?;
                let policy: Self = serde_json::from_slice(&bytes).map_err(|e| {
                    Error::Refused(format!("invalid managed policy at {}: {e}", path.display()))
                })?;
                return Ok(PolicySnapshot {
                    managed: true,
                    source_path: Some(path),
                    policy,
                });
            }
        }
        Ok(PolicySnapshot {
            managed: false,
            source_path: None,
            policy: Self::default(),
        })
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::Refused(format!("invalid managed policy at {}: {e}", path.display()))
        })
    }

    pub fn check_runtime(&self, runtime_id: &str) -> Result<()> {
        if self.blocked_runtimes.contains(runtime_id) {
            return Err(Error::Refused(format!(
                "runtime '{runtime_id}' is blocked by host ManagedPolicy"
            )));
        }
        if let Some(allowed) = &self.allowed_runtimes
            && !allowed.contains(runtime_id)
        {
            return Err(Error::Refused(format!(
                "runtime '{runtime_id}' is not in ManagedPolicy allowed_runtimes"
            )));
        }
        Ok(())
    }

    pub fn check_guest_sharing(&self) -> Result<()> {
        if !self.allow_guest_sharing {
            return Err(Error::Refused(
                "guest sharing is disabled by host ManagedPolicy".into(),
            ));
        }
        Ok(())
    }

    pub fn check_public_tunnel(&self) -> Result<()> {
        if !self.allow_public_relay_tunnel {
            return Err(Error::Refused(
                "public relay tunnels are disabled by host ManagedPolicy".into(),
            ));
        }
        Ok(())
    }

    pub fn check_mcp_outbound_webhooks(&self) -> Result<()> {
        if !self.allow_mcp_outbound_webhooks {
            return Err(Error::Refused(
                "outbound MCP webhooks are disabled by host ManagedPolicy".into(),
            ));
        }
        Ok(())
    }
}

async fn get_policy() -> std::result::Result<Json<Value>, super::http::ApiError> {
    let snap = ManagedPolicy::load()?;
    Ok(Json(json!(snap)))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new().route("/server/policy", get(get_policy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_policy_enforces_runtime_and_feature_controls() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("policy.json");
        fs::write(
            &file,
            serde_json::to_vec(&json!({
                "allowed_runtimes": ["claude", "codex"],
                "blocked_runtimes": ["codex"],
                "allow_guest_sharing": false,
                "allow_mcp_outbound_webhooks": false,
                "telemetry_mode": "disabled"
            }))
            .unwrap(),
        )
        .unwrap();
        let policy = ManagedPolicy::load_from_path(&file).unwrap();
        assert!(policy.check_runtime("claude").is_ok());
        assert!(policy.check_runtime("codex").is_err());
        assert!(policy.check_runtime("grok").is_err());
        assert!(policy.check_guest_sharing().is_err());
        assert!(policy.check_mcp_outbound_webhooks().is_err());
        assert_eq!(policy.telemetry_mode, "disabled");
    }
}
