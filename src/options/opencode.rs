//! Prompt overlays for OpenCode's two native configuration formats.
use super::env_path;
use crate::{Error, Result, Runtime, SessionOptions};
use serde_json::{Value, value::RawValue};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

fn canonical_parent(dir: &Path) -> Result<PathBuf> {
    match dir.canonicalize() {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match (dir.parent(), dir.file_name())
        {
            (Some(parent), Some(name)) => Ok(canonical_parent(parent)?.join(name)),
            _ => Err(e.into()),
        },
        Err(e) => Err(e.into()),
    }
}
// Native schema setup on an empty database can race. Unrelated data directories open independently.
pub(crate) async fn startup(opts: &SessionOptions, cwd: &Path) -> Result<OwnedMutexGuard<()>> {
    type Locks = BTreeMap<PathBuf, Weak<AsyncMutex<()>>>;
    static LOCKS: OnceLock<Mutex<Locks>> = OnceLock::new();
    let data = env_path(opts, "XDG_DATA_HOME")
        .or_else(|| opts.session_home().map(|d| d.join(".local/share")))
        .ok_or_else(|| Error::Protocol("OpenCode data directory is unknown".into()))?;
    let dir = canonical_parent(&cwd.join(data).join("opencode"))?;
    #[cfg(windows)]
    let dir: PathBuf = dir.to_string_lossy().to_lowercase().into();
    let lock = {
        let mut locks = LOCKS.get_or_init(Default::default).lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(existing) = locks.get(&dir).and_then(Weak::upgrade) {
            existing
        } else {
            let new = Arc::new(AsyncMutex::new(()));
            locks.insert(dir, Arc::downgrade(&new));
            new
        }
    };
    tokio::time::timeout(Duration::from_secs(60), lock.lock_owned())
        .await
        .map_err(|_| Error::Timeout("OpenCode database startup".into()))
}

async fn run(
    rt: &Runtime,
    opts: &SessionOptions,
    args: &[&str],
    overlay: Option<&str>,
) -> Result<Vec<u8>> {
    let mut command = opts.command(&rt.program)?;
    command.kill_on_drop(true).current_dir(&opts.cwd).args(args);
    if let Some(content) = overlay {
        command.env("OPENCODE_CONFIG_CONTENT", content);
    }
    let result = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| Error::Timeout(format!("OpenCode {}", args[0])))??;
    if !result.status.success() {
        return Err(Error::Refused(format!(
            "OpenCode {} exited with {}",
            args[0], result.status
        )));
    }
    Ok(result.stdout)
}
async fn query(
    rt: &Runtime,
    opts: &SessionOptions,
    args: &[&str],
    overlay: Option<&str>,
) -> Result<Value> {
    Ok(serde_json::from_slice(
        &run(rt, opts, args, overlay).await?,
    )?)
}
fn named(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_owned)
}
fn saved_agent(exported: &Value) -> Option<String> {
    if let Some(agent) = named(&exported["info"], "agent") {
        return Some(agent);
    }
    let messages = exported["messages"].as_array()?;
    let infos = || messages.iter().rev().map(|m| &m["info"]);
    if let Some(last) = infos().find(|m| {
        m["role"] == "user"
            && m["model"]["providerID"].is_string()
            && m["model"]["modelID"].is_string()
    }) {
        return named(last, "agent");
    }
    let last = infos().find(|m| m["providerID"].is_string() && m["modelID"].is_string())?;
    named(last, "mode").or_else(|| named(last, "agent"))
}

fn literal(s: &str) -> String {
    serde_json::to_string(s)
        .expect("string is serializable")
        .replace("{env:", r"\u007benv:")
        .replace("{file:", r"\u007bfile:")
}
// Native interpolation precedes JSON parsing. Preserve the exact bytes of all existing entries.
type Object<'a> = BTreeMap<String, &'a RawValue>;
fn object(s: &str) -> Result<Object<'_>> {
    serde_json::from_str(s).map_err(|_| {
        Error::Unsupported("OpenCode inline configuration expected a JSON object".into())
    })
}
fn insert(content: &str, container: &str, empty: bool, entry: String) -> String {
    let end =
        container.as_ptr() as usize - content.as_ptr() as usize + container.trim_end().len() - 1;
    let mut updated = content.to_owned();
    updated.insert_str(end, &format!("{}{entry}", if empty { "" } else { "," }));
    updated
}
fn add_base(content: &str, group: &str, agent: &str, field: &str, text: &str) -> Result<String> {
    let path = [group, agent, field];
    let mut container = content;
    for (index, key) in path.iter().enumerate() {
        let items = object(container)?;
        if let Some(child) = items.get(*key) {
            if index == path.len() - 1 {
                return Err(Error::Unsupported(format!(
                    "OpenCode inline configuration already sets {group}.{agent}.{field}"
                )));
            }
            container = child.get();
        } else {
            let mut value = literal(text);
            for tail in path[index + 1..].iter().rev() {
                value = format!("{{{}:{value}}}", literal(tail));
            }
            return Ok(insert(
                content,
                container,
                items.is_empty(),
                format!("{}:{value}", literal(key)),
            ));
        }
    }
    unreachable!("leaf was either inserted or already configured")
}
fn add_append(content: &str, file: &str) -> Result<String> {
    let root = object(content)?;
    if let Some(entries) = root.get("instructions") {
        let list: Vec<&RawValue> = serde_json::from_str(entries.get())
            .map_err(|_| Error::Unsupported("OpenCode instructions must be an array".into()))?;
        Ok(insert(
            content,
            entries.get(),
            list.is_empty(),
            literal(file),
        ))
    } else {
        Ok(insert(
            content,
            content,
            root.is_empty(),
            format!("\"instructions\":[{}]", literal(file)),
        ))
    }
}

pub(crate) struct Prepared {
    pub agents: Vec<String>,
    pub file: Option<NamedTempFile>,
    pub content: String,
}
pub(crate) async fn release(rt: &Runtime, opts: &SessionOptions) -> Result<semver::Version> {
    let version = run(rt, opts, &["--version"], None).await?;
    let number = std::str::from_utf8(&version)
        .unwrap_or("")
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim_start_matches('v');
    let release = semver::Version::parse(number)
        .map_err(|_| Error::Unsupported("unknown OpenCode release".into()))?;
    if !matches!(release.major, 1 | 2) {
        return Err(Error::Unsupported(format!(
            "unknown OpenCode release {release}"
        )));
    }
    Ok(release)
}
pub(crate) async fn prepare(
    rt: &Runtime,
    opts: &SessionOptions,
    fork_source: Option<&str>,
    release: &semver::Version,
) -> Result<Prepared> {
    let v2 = release.major == 2;
    let (group, field) = if v2 {
        ("agents", "system")
    } else {
        ("agent", "prompt")
    };
    let existing = opts.env_value("OPENCODE_CONFIG_CONTENT");
    let mut content = existing
        .map(|v| {
            v.into_string()
                .map_err(|_| Error::Unsupported("non-UTF8 OpenCode inline configuration".into()))
        })
        .transpose()?
        .unwrap_or_else(|| "{}".into());
    object(&content)?;
    let mut agents = vec![];
    if let Some(base) = &opts.settings.system_prompt {
        if base.is_empty() {
            return Err(Error::Unsupported(
                "OpenCode treats an empty base prompt as omitted".into(),
            ));
        }
        if v2 {
            if *release < semver::Version::new(2, 2, 0) {
                return Err(Error::Unsupported(
                    "OpenCode v2 base instructions require 2.2.0 or later".into(),
                ));
            }
            let list = query(rt, opts, &["debug", "agents", "--standalone"], None).await?;
            agents = list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["mode"] != "subagent" && a["hidden"] != true)
                .filter_map(|a| named(a, "id"))
                .collect();
            if agents.is_empty() {
                return Err(Error::Refused(
                    "OpenCode reported no visible primary agents".into(),
                ));
            }
        } else {
            let config = query(rt, opts, &["debug", "config"], None).await?;
            let saved = if let Some(id) = fork_source.or(opts.resume.as_deref()) {
                saved_agent(&query(rt, opts, &["export", id, "--sanitize"], None).await?)
            } else {
                None
            };
            let agent = saved
                .or_else(|| named(&config, "default_agent").filter(|s| !s.is_empty()))
                .unwrap_or_else(|| "build".into());
            if !matches!(agent.as_str(), "build" | "plan") && !config["agent"][&agent].is_object() {
                return Err(Error::Refused(format!(
                    "OpenCode agent {agent:?} does not exist"
                )));
            }
            agents.push(agent);
        }
        for agent in &agents {
            content = add_base(&content, group, agent, field, base)?;
        }
    }
    let mut file = None;
    if let Some(append) = &opts.settings.append_system_prompt {
        if v2 && *release < semver::Version::new(2, 0, 30) {
            return Err(Error::Unsupported(
                "OpenCode v2 append requires 2.0.30 or later".into(),
            ));
        }
        let mut f = tempfile::Builder::new().suffix(".md").tempfile()?;
        f.write_all(append.as_bytes())?;
        f.flush()?;
        content = add_append(
            &content,
            f.path().to_str().ok_or_else(|| {
                Error::Unsupported("OpenCode instructions need a UTF-8 path".into())
            })?,
        )?;
        file = Some(f);
    }
    if v2 && let Some(base) = &opts.settings.system_prompt {
        let list = query(
            rt,
            opts,
            &["debug", "agents", "--standalone"],
            Some(&content),
        )
        .await?;
        for agent in &agents {
            if !list
                .as_array()
                .into_iter()
                .flatten()
                .any(|a| a["id"].as_str() == Some(agent) && a["system"].as_str() == Some(base))
            {
                return Err(Error::Refused(format!(
                    "OpenCode did not confirm instructions for {agent:?}"
                )));
            }
        }
    } else if !v2 {
        let config = query(rt, opts, &["debug", "config"], Some(&content)).await?;
        if let Some(agent) = agents.first()
            && config["agent"][agent][field].as_str() != opts.settings.system_prompt.as_deref()
        {
            return Err(Error::Refused(
                "OpenCode did not confirm the requested agent prompt".into(),
            ));
        }
        if let Some(f) = &file
            && config["instructions"]
                .as_array()
                .and_then(|v| v.last())
                .and_then(Value::as_str)
                != f.path().to_str()
        {
            return Err(Error::Refused(
                "OpenCode did not confirm the appended instructions file".into(),
            ));
        }
    }
    Ok(Prepared {
        agents,
        file,
        content,
    })
}

pub(crate) fn confirm(info: &Value, agents: &[String]) -> Result<()> {
    if agents.is_empty() {
        return Ok(());
    }
    let selected = info["configOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["category"] == "mode")
        .and_then(|a| a["currentValue"].as_str())
        .or_else(|| info.pointer("/modes/currentModeId")?.as_str());
    if !selected.is_some_and(|id| agents.iter().any(|a| a == id)) {
        return Err(Error::Refused(
            "OpenCode opened on an agent outside the configured prompt set".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlay_preserves_native_interpolation_and_duplicate_member_resolution() {
        let original = r#"{ "kept":"\u007benv:S}", "agents":{},"agents":{"a":{"mode":"primary"}}, "instructions":["{file:/original}"] }"#;
        let base = add_base(original, "agents", "a", "system", "literal {env:S}").unwrap();
        let both = add_append(&base, "/tmp/{file:literal}.md").unwrap();
        assert!(
            both.contains(r#""kept":"\u007benv:S}""#)
                && both.contains(r#""system":"literal \u007benv:S}""#)
        );
        let parsed: Value = serde_json::from_str(&both).unwrap();
        assert_eq!(parsed["agents"]["a"]["mode"], "primary");
        assert_eq!(parsed["instructions"][1], "/tmp/{file:literal}.md");
        assert!(add_base(&both, "agents", "a", "system", "replace").is_err());
        assert!(add_append(r#"{"instructions":{}}"#, "/x").is_err());
    }
}
