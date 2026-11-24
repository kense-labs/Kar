//! Optional tier for a confirmed signed-in account, without starting a model session.
use super::{native::Wire, profile};
use crate::installation::Probe;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::time::Instant;

pub(super) async fn plan(probe: &Probe, path: &Path, until: Instant) -> Option<String> {
    // The identity lookup must not discard a completed login or quota reading.
    let deadline = until
        .checked_sub(Duration::from_millis(100))?
        .min(Instant::now() + Duration::from_secs(5));
    if deadline <= Instant::now() {
        return None;
    }
    tokio::time::timeout_at(deadline, async {
        let state = profile::home(probe)?.join(".gemini/antigravity-cli/cache/acp-auth-state.json");
        let method = tokio::task::spawn_blocking(move || {
            let raw = profile::read_file(&state, 64 * 1024).ok()??;
            let v: Value = serde_json::from_str(&raw).ok()?;
            match v["methodId"].as_str()? {
                "oauth-personal" => Some("oauth-personal"),
                "oauth-business" => Some("oauth-business"),
                _ => None,
            }
        })
        .await
        .ok()??;
        let mut cmd = probe.command(path);
        cmd.arg("acp");
        if let Some(dir) = profile::path(probe, "GEMINI_HOME") {
            cmd.arg("--gemini-dir").arg(dir);
        }
        if probe.get("SSH_CONNECTION").is_none_or(|v| v.is_empty()) {
            cmd.env("SSH_CONNECTION", "1");
        }
        cmd.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
        let mut wire = Wire::open(cmd).ok()?;
        let hello = wire.initialize(Wire::capabilities()).await.ok()?;
        if hello.pointer("/agentCapabilities/_meta/antigravity/tier") != Some(&Value::Bool(true))
            || !hello["authMethods"]
                .as_array()?
                .iter()
                .any(|a| a["id"] == method)
        {
            return None;
        }
        let reply = wire
            .rpc(2, "authenticate", json!({"methodId":method}))
            .await
            .ok()?;
        let name = reply
            .pointer("/result/_meta/antigravity/tier/name")?
            .as_str()?
            .trim();
        (!name.is_empty()).then(|| name.into())
    })
    .await
    .ok()
    .flatten()
}
