//! Pi owns package inheritance, version matching, resource discovery and install locations.
use super::sdk::Host;
use crate::{Error, Result, installation::Probe, pi_sdk};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn offline(probe: &Probe) -> bool {
    probe
        .get("PI_OFFLINE")
        .and_then(|s| s.to_str())
        .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn settings_have_packages(path: &Path) -> bool {
    let contents = match crate::account::read_profile(path, 1024 * 1024) {
        Ok(None) => return false,
        Ok(Some(s)) => s,
        // Large or unusual settings are interpreted by the native resolver under its deadline.
        Err(_) => return true,
    };
    let contents = contents.trim_start_matches('\u{feff}');
    serde_json::from_str::<Value>(contents).map_or(true, |v| match v.get("packages") {
        None | Some(Value::Null) => false,
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    })
}

fn diagnostic_source(source: String) -> String {
    let (prefix, input) = source
        .strip_prefix("git:")
        .map_or(("", source.as_str()), |s| ("git:", s));
    if let Ok(mut url) = url::Url::parse(input)
        && url.has_host()
        && (!url.username().is_empty() || url.password().is_some() || url.query().is_some())
    {
        let _ = url.set_password(None);
        let _ = url.set_username("");
        url.set_query(None);
        return format!("{prefix}{url}");
    }
    source
}

pub(crate) async fn check(
    probe: &Probe,
    program: &Path,
    version: Option<&str>,
    trusted: Option<bool>,
    allow_install: bool,
    timeout: Duration,
) -> Result<()> {
    let explicit_offline = offline(probe);
    if allow_install && !explicit_offline {
        return Ok(());
    }
    let home = crate::account::profile_home(probe)
        .ok_or_else(|| Error::Refused("Pi home directory is unknown".into()))?;
    let agent_dir = super::agent_directory(probe, &home);
    let trust = trusted.unwrap_or(true);
    if !(trust && settings_have_packages(&probe.cwd.join(".pi/settings.json")))
        && (explicit_offline || !settings_have_packages(&agent_dir.join("settings.json")))
    {
        return Ok(());
    }
    let dist = if let Some(dist) = pi_sdk::sdk(program) {
        dist
    } else if let Some(root) = probe.get("KAR_PI_SDK_DIR").filter(|s| !s.is_empty()) {
        let actual;
        let version = if let Some(v) = version {
            v
        } else {
            let output = tokio::time::timeout(
                timeout,
                probe
                    .command(program)
                    .arg("--version")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| Error::Timeout("Pi version".into()))??;
            if !output.status.success() {
                return Err(Error::Refused("cannot verify the Pi SDK version".into()));
            }
            actual = String::from_utf8_lossy(&output.stdout).into_owned();
            actual.trim()
        };
        pi_sdk::companion(&probe.cwd.join(root), Some(version))
            .map_err(|e| Error::Unsupported(e.message().into()))?
            .dist
    } else if explicit_offline {
        eprintln!(
            "Kar: offline Pi will skip automatic installation; its standalone executable has no SDK for project package diagnostics"
        );
        return Ok(());
    } else {
        return Err(Error::Unsupported("Pi package preflight requires the selected npm installation or its matching SDK in KAR_PI_SDK_DIR".into()));
    };
    let query = async {
        let node = probe.command(&pi_sdk::node_for(program));
        let mut host = Host::launch(node, &dist, true, timeout)
            .await?
            .ok_or_else(|| {
                Error::Unsupported("Pi package preflight requires the matching native SDK".into())
            })?;
        host.send(&json!({"op":"checkPackages","agentDir":agent_dir,"settingsOptions":{"projectTrusted":trust}})).await?;
        let (tag, data) = host.next().await?;
        host.close().await;
        if tag != "PACKAGES" {
            return Err(Error::Protocol(
                "Pi package resolver did not complete".into(),
            ));
        }
        Ok::<_, Error>(serde_json::from_str::<Vec<String>>(&data)?)
    };
    let names = tokio::time::timeout(timeout, query)
        .await
        .map_err(|_| Error::Timeout("Pi package preflight".into()))??;
    if names.is_empty() {
        return Ok(());
    }
    let sources =
        serde_json::to_string(&names.into_iter().map(diagnostic_source).collect::<Vec<_>>())?;
    if explicit_offline {
        eprintln!(
            "Kar: Pi could not locate configured project packages in its offline store: {sources}"
        );
        Ok(())
    } else {
        Err(Error::Refused(format!(
            "project_packages_blocked: Pi must install or replace configured packages {sources}; set PI_OFFLINE=1 to skip unavailable packages, or allow package installation explicitly"
        )))
    }
}
