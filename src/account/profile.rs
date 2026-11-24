use super::{AuthStatus, LastAccount, Reader, projection::field};
use crate::installation::Probe;
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use serde_json::Value;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::LazyLock,
};

pub(super) fn path(probe: &Probe, name: &str) -> Option<PathBuf> {
    Some(probe.cwd.join(probe.get(name).filter(|v| !v.is_empty())?))
}
pub(crate) fn home(probe: &Probe) -> Option<PathBuf> {
    path(probe, if cfg!(windows) { "USERPROFILE" } else { "HOME" }).or_else(std::env::home_dir)
}

pub(crate) fn default_gemini_directory(probe: &Probe) -> bool {
    let Some(requested) = path(probe, "GEMINI_HOME") else {
        return true;
    };
    home(probe).is_some_and(|h| {
        let native = h.join(".gemini");
        if cfg!(windows) {
            native
                .as_os_str()
                .eq_ignore_ascii_case(requested.as_os_str())
        } else {
            native == requested
        }
    })
}

pub(crate) fn read_file(path: &Path, limit: u64) -> io::Result<Option<String>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other?,
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return Err(io::Error::other("Invalid configuration file"));
    }
    let mut s = String::new();
    file.take(limit + 1).read_to_string(&mut s)?;
    if s.len() as u64 > limit {
        return Err(io::Error::other("Configuration exceeds size limit"));
    }
    Ok(Some(s))
}

pub(super) async fn enrich(reader: Reader, probe: &Probe, out: &str, status: &mut AuthStatus) {
    match (reader, status) {
        (Reader::Claude, status) => {
            let Some(dir) = path(probe, "CLAUDE_CONFIG_DIR").or_else(|| home(probe)) else {
                return;
            };
            let data = tokio::task::spawn_blocking(move || {
                let raw = read_file(&dir.join(".claude.json"), 16 * 1024 * 1024).ok()??;
                let mut value: Value = serde_json::from_str(&raw).ok()?;
                Some(value["oauthAccount"].take())
            })
            .await
            .ok()
            .flatten();
            let Some(profile) = data else { return };
            let live: Value = serde_json::from_str(out).unwrap_or_default();
            claude(status, &profile, &live);
        }
        (
            Reader::Codex,
            AuthStatus::LoggedIn {
                account: Some(a), ..
            },
        ) if a.method.as_deref() == Some("chatgpt") => {
            let Some(dir) =
                path(probe, "CODEX_HOME").or_else(|| home(probe).map(|h| h.join(".codex")))
            else {
                return;
            };
            let mut configs = vec![dir.join("config.toml")];
            let system = if cfg!(windows) {
                path(probe, "ProgramData").map(|d| d.join("OpenAI/Codex"))
            } else {
                Some(PathBuf::from("/etc/codex"))
            };
            if let Some(system) = system {
                configs.extend([
                    system.join("config.toml"),
                    system.join("managed_config.toml"),
                ]);
            }
            let email = a.email.clone();
            a.account_id =
                tokio::task::spawn_blocking(move || codex(&dir, &configs, email.as_deref()))
                    .await
                    .ok()
                    .flatten();
        }
        _ => {}
    }
}

fn claude(status: &mut AuthStatus, profile: &Value, live: &Value) {
    let email = field(profile, "emailAddress");
    match status {
        AuthStatus::LoggedOut { last_account, .. } => {
            let display_name = field(profile, "displayName");
            if email.is_some() || display_name.is_some() {
                *last_account = Some(LastAccount {
                    email,
                    display_name,
                });
            }
        }
        AuthStatus::LoggedIn {
            account: Some(a), ..
        } if a.method.as_deref() == Some("claude.ai") || a.plan.is_some() => {
            let email_matches = email
                .zip(a.email.as_ref())
                .map(|(x, y)| x.to_lowercase() == y.to_lowercase());
            let org = field(profile, "organizationUuid")
                .zip(field(live, "orgId"))
                .map(|(x, y)| x == y);
            let name = field(profile, "organizationName")
                .zip(field(live, "orgName"))
                .map(|(x, y)| x == y);
            if [email_matches, org, name].contains(&Some(false))
                || ![email_matches, org].contains(&Some(true))
            {
                return;
            }
            static PLAN: LazyLock<regex::Regex> = LazyLock::new(|| {
                regex::Regex::new(r"^[\p{L}\p{N}](?:[\p{L}\p{N} .+-]{0,38}[\p{L}\p{N}+])?$")
                    .unwrap()
            });
            if let Some(p) = field(profile, "planDisplayName").filter(|p| PLAN.is_match(p)) {
                a.plan = Some(p);
            }
            a.account_id = field(profile, "accountUuid");
        }
        _ => {}
    }
}
fn file_store(table: &toml::Table) -> bool {
    fn has_nested_setting(v: &toml::Value) -> bool {
        match v {
            toml::Value::Table(t) => {
                t.contains_key("cli_auth_credentials_store") || t.values().any(has_nested_setting)
            }
            toml::Value::Array(a) => a.iter().any(has_nested_setting),
            _ => false,
        }
    }
    table
        .get("cli_auth_credentials_store")
        .is_none_or(|s| s.as_str() == Some("file"))
        && !table.values().any(has_nested_setting)
}
fn codex(dir: &Path, configs: &[PathBuf], email: Option<&str>) -> Option<String> {
    for file in configs {
        if let Some(config) = read_file(file, 1024 * 1024).ok()?
            && !file_store(&config.parse::<toml::Table>().ok()?)
        {
            return None;
        }
    }
    let raw = read_file(&dir.join("auth.json"), 1024 * 1024).ok()??;
    let root: Value = serde_json::from_str(&raw).ok()?;
    if root.get("auth_mode").is_some_and(|m| m != "chatgpt") {
        return None;
    }
    let tokens = &root["tokens"];
    let id = field(tokens, "account_id")?;
    if let Some(token) = tokens["id_token"].as_str() {
        let parts: Vec<_> = token.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(parts[1])
            .or_else(|_| URL_SAFE.decode(parts[1]))
            .ok()?;
        let claim: Value = serde_json::from_slice(&bytes).ok()?;
        if email.is_some_and(|live| {
            field(&claim, "email").is_none_or(|e| !e.eq_ignore_ascii_case(live))
        }) || !claim.is_object()
            || field(&claim["https://api.openai.com/auth"], "chatgpt_account_id")
                .is_some_and(|c| c != id)
        {
            return None;
        }
    } else if email.is_some() {
        return None;
    }
    Some(id)
}
