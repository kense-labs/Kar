//! Qwen's user/system configuration presence audit never opens a token file.
use super::{
    AuthStatus, Reader,
    profile::{home, path, read_file},
};
use crate::{
    Result,
    credentials::{Purpose, for_runtime},
    installation::Probe,
};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::LazyLock,
};

const LIMIT: u64 = 4 * 1024 * 1024;
type Variables = BTreeMap<String, String>;

fn dotenv(file: &Path) -> Result<Variables> {
    let mut vars = Variables::new();
    let Some(raw) = read_file(file, LIMIT)? else {
        return Ok(vars);
    };
    // Match dotenv's quoted (including multiline) and plain, comment-terminated assignments.
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^[\t ]*(?:export[\t ]+)?([\w.-]+)(?:[\t ]*=[\t ]*|:[\t ]+)(?:'((?:\\'|[^'])*)'|"((?:\\"|[^"])*)"|\x60((?:\\\x60|[^\x60])*)\x60|([^#\r\n]*))[^\r\n]*"#).unwrap()
    });
    for c in LINE.captures_iter(raw.trim_start_matches('\u{feff}')) {
        let value = if let Some(v) = c.get(3) {
            v.as_str().replace("\\n", "\n").replace("\\r", "\r")
        } else if let Some(v) = c.get(2).or_else(|| c.get(4)) {
            v.as_str().to_owned()
        } else {
            c.get(5).map_or("", |v| v.as_str()).trim().to_owned()
        };
        vars.insert(c[1].into(), value);
    }
    Ok(vars)
}
fn config_dir(probe: &Probe, home: &Path, raw: &Path) -> PathBuf {
    if raw.as_os_str().is_empty() {
        return home.join(".qwen");
    }
    if let Some(s) = raw.to_str() {
        if s == "~" {
            return home.to_owned();
        }
        if let Some(tail) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
            return tail
                .split(['/', '\\'])
                .filter(|p| !p.is_empty())
                .fold(home.to_owned(), |d, p| d.join(p));
        }
    }
    probe.cwd.join(raw)
}
fn exists(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(m) => Ok(m.is_file()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn map_value<'a>(m: &'a Variables, k: &str) -> Option<&'a str> {
    #[cfg(windows)]
    let v = m
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(k))
        .map(|(_, v)| v);
    #[cfg(not(windows))]
    let v = m.get(k);
    v.filter(|v| !v.is_empty()).map(String::as_str)
}
fn invalid() -> io::Error {
    io::Error::other("Unsupported Qwen configuration")
}

pub(crate) fn config_home(probe: &Probe, home: &Path, strict: bool) -> Result<PathBuf> {
    let initial = probe.get("QWEN_HOME");
    let mut dir = config_dir(probe, home, Path::new(initial.unwrap_or_default()));
    if initial.is_none() {
        for file in [dir.join(".env"), home.join(".env")] {
            let env = match dotenv(&file) {
                Err(_) if !strict => continue,
                result => result?,
            };
            if let Some(value) = map_value(&env, "QWEN_HOME") {
                dir = config_dir(probe, home, Path::new(value));
                break;
            }
        }
    }
    Ok(dir)
}

pub(crate) fn system_paths(probe: &Probe) -> (PathBuf, PathBuf) {
    let system = path(probe, "QWEN_CODE_SYSTEM_SETTINGS_PATH").unwrap_or_else(|| {
        PathBuf::from(if cfg!(target_os = "macos") {
            "/Library/Application Support/QwenCode/settings.json"
        } else if cfg!(windows) {
            r"C:\ProgramData\qwen-code\settings.json"
        } else {
            "/etc/qwen-code/settings.json"
        })
    });
    let defaults = path(probe, "QWEN_CODE_SYSTEM_DEFAULTS_PATH")
        .unwrap_or_else(|| system.with_file_name("system-defaults.json"));
    (system, defaults)
}

pub(crate) fn settings(file: &Path) -> Result<Option<Value>> {
    let Some(raw) = read_file(file, LIMIT)? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_reader(json_comments::StripComments::with_settings(
        json_comments::CommentSettings::c_style(),
        raw.trim_start_matches('\u{feff}').as_bytes(),
    ))?;
    if !value.is_object() {
        return Err(invalid().into());
    }
    Ok(Some(value))
}

pub(super) fn inspect(probe: &Probe) -> Result<AuthStatus> {
    let reader = Reader::Qwen;
    let home = home(probe).ok_or_else(invalid)?;
    let dir = config_home(probe, &home, true)?;
    let default_dir = home.join(".qwen");
    let (system, defaults) = system_paths(probe);
    let mut keys: BTreeSet<String> = for_runtime("qwen")
        .expect("Qwen metadata")
        .env
        .iter()
        .filter(|v| v.purpose == Purpose::ApiKey)
        .map(|v| v.name.clone())
        .collect();
    let mut settings_env = Vec::new();
    let mut observed = BTreeSet::new();
    for (source, file) in [
        ("system defaults", defaults),
        ("user settings", dir.join("settings.json")),
        ("system settings", system),
    ] {
        let Some(value) = settings(&file)? else {
            continue;
        };
        if value
            .get("$version")
            .is_some_and(|v| !matches!(v.as_u64(), Some(1..=4)))
        {
            return Err(invalid().into());
        }
        let auth = &value["security"]["auth"];
        let selected = auth["selectedType"]
            .as_str()
            .or_else(|| value["selectedAuthType"].as_str())
            .unwrap_or_default();
        let known = matches!(
            selected,
            "openai" | "openai-responses" | "anthropic" | "gemini" | "vertex-ai" | "qwen-oauth"
        );
        if auth["apiKey"].as_str().is_some_and(|s| !s.is_empty()) {
            observed.insert(if known {
                format!("{source}: security.auth.apiKey ({selected})")
            } else {
                format!("{source}: security.auth.apiKey")
            });
        }
        if matches!(selected, "vertex-ai" | "qwen-oauth") {
            observed.insert(format!("{source}: selectedType ({selected})"));
        } else if !selected.is_empty() && !known {
            observed.insert(format!("{source}: unrecognized selectedType"));
        }
        if auth["useExternal"] == true || value["useExternalAuth"] == true {
            observed.insert(format!("{source}: external authentication configured"));
        }
        settings_env.push(
            value["env"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect::<Variables>(),
        );
        for models in value["modelProviders"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
        {
            for name in models
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["envKey"].as_str())
                .filter(|s| !s.is_empty())
            {
                keys.insert(name.into());
            }
        }
    }
    let mut files = vec![];
    let mut seen = BTreeSet::new();
    for f in [
        dir.join(".env"),
        default_dir.join(".env"),
        home.join(".env"),
    ] {
        if seen.insert(f.clone()) {
            files.push(dotenv(&f)?);
        }
    }
    let inherited = |key: &str| {
        probe
            .get(key)
            .and_then(|v| v.to_str())
            .filter(|v| !v.is_empty())
    };
    let value = |key: &str| {
        inherited(key).or_else(|| {
            files
                .iter()
                .chain(settings_env.iter().rev())
                .find_map(|m| map_value(m, key))
        })
    };
    for key in keys {
        if value(&key).is_some() {
            let safe = key.len() <= 128
                && key.bytes().enumerate().all(|(i, c)| {
                    c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                });
            let label = if safe {
                key.as_str()
            } else {
                "custom modelProviders envKey"
            };
            let source = if inherited(&key).is_some() {
                "environment"
            } else {
                "user .env or settings.env"
            };
            observed.insert(format!("{source}: {label}"));
        }
    }
    if let Some(adc) = value("GOOGLE_APPLICATION_CREDENTIALS") {
        let present = exists(&probe.cwd.join(adc))?;
        observed.insert(format!(
            "GOOGLE_APPLICATION_CREDENTIALS file {}",
            if present { "exists" } else { "not found" }
        ));
    }
    if value("QWEN_OAUTH").is_some() {
        observed.insert("QWEN_OAUTH requests legacy OAuth".into());
    }
    if value("GOOGLE_CLOUD_PROJECT").is_some() {
        observed.insert("GOOGLE_CLOUD_PROJECT: Vertex ADC may use external credentials".into());
    }
    let adc = if cfg!(windows) {
        value("APPDATA").map(|d| probe.cwd.join(d).join("gcloud"))
    } else {
        Some(home.join(".config/gcloud"))
    };
    if let Some(d) = adc
        && exists(&d.join("application_default_credentials.json"))?
    {
        observed.insert("Vertex ADC file exists".into());
    }
    if exists(&dir.join("oauth_creds.json"))? {
        observed.insert("legacy Qwen OAuth credential file exists".into());
    }
    let scope =
        "Project settings, workspace .env files and launch arguments may still supply credentials.";
    if observed.is_empty() {
        return Ok(AuthStatus::LoggedOut {
            source: reader.source().into(),
            last_account: None,
            detail: Some(format!("No user/system credential found. {scope}")),
        });
    }
    Ok(reader.configured(
        &format!("Configured credential sources are unverified. {scope}"),
        observed.into_iter().collect(),
    ))
}
