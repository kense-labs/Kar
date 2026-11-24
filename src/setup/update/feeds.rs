//! Fixed native update sources for runtimes without a read-only CLI query.
use super::{Probe, Target, UpdateCheck, Version, compare};
use crate::account::{profile_home, read_profile};
use serde_json::Value;
use std::path::{Path, PathBuf};

struct Feed {
    url: String,
    json: bool,
    channel: Option<&'static str>,
    minimum: Option<Version>,
    newer_only: bool,
}

fn normalized(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.into())
        .to_string_lossy()
        .replace('\\', "/")
}

fn read(path: &Path) -> Option<String> {
    read_profile(path, crate::process::OUTPUT_LIMIT)
        .ok()
        .flatten()
}

fn config_path(probe: &Probe, var: &str) -> Option<PathBuf> {
    probe
        .get(var)
        .filter(|s| !s.is_empty())
        .map(|p| probe.cwd.join(p))
}

fn claude(probe: &Probe, path: &str) -> Result<Feed, UpdateCheck> {
    if [
        "/Caskroom/",
        "/Cellar/",
        "/mise/",
        "/asdf/",
        "/WinGet/",
        "/winget/",
    ]
    .iter()
    .any(|s| path.contains(s))
        || (path.contains("/Library/Application Support/Claude/claude-code/")
            && path.ends_with("/claude.app/Contents/MacOS/claude"))
    {
        return Err(UpdateCheck::unavailable("package_manager", None));
    }
    let home = profile_home(probe);
    let dir = config_path(probe, "CLAUDE_CONFIG_DIR");
    let settings = dir
        .clone()
        .or_else(|| home.as_ref().map(|p| p.join(".claude")))
        .and_then(|p| read(&p.join("settings.json")))
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_default();
    let flag = probe
        .get("DISABLE_UPDATES")
        .filter(|s| !s.is_empty())
        .map(|s| Value::String(s.to_string_lossy().into_owned()))
        .or_else(|| settings.pointer("/env/DISABLE_UPDATES").cloned());
    if flag.is_some_and(|v| v != "" && v != "0" && v != false) {
        return Err(UpdateCheck::unavailable("updates_disabled", None));
    }
    let channel = if settings["autoUpdatesChannel"] == "stable" {
        "stable"
    } else {
        "latest"
    };
    let native = path.contains("/claude/versions/")
        || path
            .to_ascii_lowercase()
            .ends_with("/.local/bin/claude.exe");
    let recorded_global = || {
        dir.or(home)
            .and_then(|p| read(&p.join(".claude.json")))
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some_and(|v| v["installMethod"] == "global")
    };
    let url = if native {
        format!("https://downloads.claude.ai/claude-code-releases/{channel}")
    } else if path.contains("/node_modules/@anthropic-ai/claude-code/") || recorded_global() {
        format!("https://registry.npmjs.org/@anthropic-ai/claude-code/{channel}")
    } else {
        return Err(UpdateCheck::unavailable("unmanaged_installation", None));
    };
    Ok(Feed {
        url,
        json: !native,
        channel: Some(channel),
        minimum: settings["minimumVersion"].as_str().and_then(super::release),
        newer_only: false,
    })
}

fn devin_distribution(command: &Path) -> Option<&'static str> {
    let command = command.canonicalize().ok()?;
    let dist = read(&command.parent()?.parent()?.join("distribution"))?;
    match dist.trim() {
        "curl-bash" | "irm-iex" => Some("https://static.devin.ai/cli/current/manifest.json"),
        "curl-bash-enterprise" | "irm-iex-enterprise" => {
            Some("https://static.devinenterprise.com/cli/current/manifest-enterprise.json")
        }
        "curl-bash-windsurfcom" | "irm-iex-windsurfcom" => {
            Some("https://static.devin.ai/cli/current/manifest-windsurfcom.json")
        }
        _ => None,
    }
}

fn select(id: &str, probe: &Probe, target: &Target) -> Result<Feed, UpdateCheck> {
    if id == "claude" {
        return claude(probe, &normalized(&target.command));
    }
    let unavailable = || UpdateCheck::unavailable("unsupported_installation", None);
    let (url, json) = match id {
        "devin" => (
            devin_distribution(&target.command)
                .ok_or_else(unavailable)?
                .into(),
            true,
        ),
        "kimi" => {
            if normalized(&target.command).contains("/Cellar/") {
                return Err(UpdateCheck::unavailable("package_manager", None));
            }
            let dir = config_path(probe, "KIMI_CODE_HOME")
                .or_else(|| profile_home(probe).map(|p| p.join(".kimi-code")));
            let global = dir
                .and_then(|p| read(&p.join("region")))
                .is_some_and(|s| s.trim() == "global");
            (
                (if global {
                    "https://code.kimi.ai/kimi-code/latest"
                } else {
                    "https://code.kimi.com/kimi-code/latest"
                })
                .into(),
                false,
            )
        }
        "antigravity" => {
            if !cfg!(target_os = "macos") {
                return Err(UpdateCheck::unavailable("platform_source_unverified", None));
            }
            let arch = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                _ => return Err(unavailable()),
            };
            (
                format!(
                    "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/darwin_{arch}.json"
                ),
                true,
            )
        }
        _ => return Err(UpdateCheck::unavailable("no_check", None)),
    };
    Ok(Feed {
        url,
        json,
        channel: None,
        minimum: None,
        newer_only: true,
    })
}

async fn fetch(url: &str) -> Option<Vec<u8>> {
    let client = reqwest::Client::builder().https_only(true).build().ok()?;
    let mut response = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len().saturating_add(chunk.len()) > crate::process::OUTPUT_LIMIT as usize {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

fn report(feed: &Feed, installed: &Version, body: &[u8]) -> UpdateCheck {
    let json;
    let value = if feed.json {
        json = serde_json::from_slice::<Value>(body).ok();
        json.as_ref().and_then(|j| j["version"].as_str())
    } else {
        std::str::from_utf8(body).ok().map(str::trim)
    };
    let mut verdict = None;
    if let Some(latest) = value.and_then(super::release) {
        let numeric = |v: &Version| (v.major, v.minor, v.patch);
        if feed
            .minimum
            .as_ref()
            .is_some_and(|minimum| numeric(&latest) < numeric(minimum))
        {
            verdict = Some(false);
        } else if feed.newer_only {
            verdict = Some(numeric(&latest) > numeric(installed));
        }
    }
    compare(installed, value, verdict, &feed.url, feed.channel)
}

pub(super) async fn query(id: &str, probe: &Probe, target: &Target) -> UpdateCheck {
    let feed = match select(id, probe, target) {
        Ok(feed) => feed,
        Err(status) => return status,
    };
    match fetch(&feed.url).await {
        Some(body) => report(&feed, &target.version, &body),
        None => UpdateCheck::unavailable("lookup_failed", Some(&feed.url)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_feeds_respect_channels_minimums_and_downgrades() {
        let mut feed = Feed {
            url: "fixed test source".into(),
            json: false,
            channel: Some("stable"),
            minimum: Some(Version::new(2, 1, 4)),
            newer_only: false,
        };
        let installed = Version::new(2, 1, 2);
        assert!(
            matches!(report(&feed,&installed,b"2.1.3"),UpdateCheck::Ok {update_available:false,channel:Some(ch),..} if ch=="stable")
        );
        assert!(matches!(
            report(&feed, &installed, b"2.1.4"),
            UpdateCheck::Ok {
                update_available: true,
                ..
            }
        ));
        feed.minimum = None;
        feed.newer_only = true;
        feed.json = true;
        assert!(matches!(
            report(&feed, &installed, br#"{"version":"v2.0.9"}"#),
            UpdateCheck::Ok {
                update_available: false,
                ..
            }
        ));
        assert!(matches!(
            report(&feed, &installed, br#"{"version":"invalid"}"#),
            UpdateCheck::Unavailable {
                reason: "version_unreadable",
                ..
            }
        ));
    }
}
