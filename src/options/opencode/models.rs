//! Read OpenCode's native model catalog without opening an ACP session or persistent daemon.
use crate::{Error, Result, Runtime, SessionOptions, forms::ModelEntry, process::OwnedProcess};
use indexmap::IndexMap;
use regex::Regex;
use serde::de::IgnoredAny;
use serde_json::{Map, Value, json, value::RawValue};
use std::{process::Stdio, sync::LazyLock};

fn release_flag(version: &str) -> Result<&'static str> {
    // Native development builds can append identifiers outside the SemVer grammar.
    static RELEASE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:opencode\s+)?v?([12])\.[0-9]+\.[0-9]+\S*$").unwrap());
    match RELEASE.captures(version.trim()).as_ref().map(|m| &m[1]) {
        Some("1") => Ok("--verbose"),
        Some("2") => Ok("--standalone"),
        _ => Err(Error::Protocol("unrecognized OpenCode release line".into())),
    }
}

fn model_id(id: &str) -> bool {
    !id.chars().any(char::is_whitespace)
        && id
            .match_indices('/')
            .any(|(i, _)| i > 0 && i + 1 < id.len())
}

fn model(id: &str) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        default: false,
        effort_levels: None,
        disabled: None,
        metadata: Map::new(),
    }
}

fn legacy(output: &str) -> Vec<ModelEntry> {
    let mut lines = output.lines().peekable();
    let mut result = Vec::new();
    while let Some(id) = lines.next() {
        if !model_id(id) || lines.peek() != Some(&"{") {
            continue;
        }
        let mut body = String::new();
        for line in lines.by_ref() {
            body.push_str(line);
            body.push('\n');
            if line == "}" {
                break;
            }
        }
        let mut entry = model(id);
        if let Ok(fields) = serde_json::from_str::<IndexMap<String, Box<RawValue>>>(&body) {
            let text = |key: &str| -> Option<String> {
                let raw = fields.get(key)?;
                let s: String = serde_json::from_str(raw.get()).ok()?;
                let s = s.trim();
                (!s.is_empty()).then(|| s.to_owned())
            };
            if let Some(name) = text("name") {
                entry
                    .metadata
                    .insert("displayName".into(), Value::String(name));
            }
            if let Some(provider) = text("providerID") {
                entry
                    .metadata
                    .insert("provider".into(), json!({"id":provider}));
            }
            if let Some(variants) = fields
                .get("variants")
                .and_then(|v| serde_json::from_str::<IndexMap<String, IgnoredAny>>(v.get()).ok())
                .filter(|v| !v.is_empty())
            {
                entry.effort_levels = Some(variants.into_keys().collect());
            }
        }
        result.push(entry);
    }
    result
}

fn standalone(output: &str) -> Result<Vec<ModelEntry>> {
    let mut models = vec![];
    for id in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !model_id(id) {
            return Err(Error::Protocol(
                "OpenCode returned an unrecognized standalone model listing".into(),
            ));
        }
        models.push(model(id));
    }
    if models.is_empty() {
        return Err(Error::Unsupported(
            "OpenCode's standalone model list was empty; a cold catalog may still be loading"
                .into(),
        ));
    }
    Ok(models)
}

async fn output(runtime: &Runtime, options: &SessionOptions, args: &[&str]) -> Result<String> {
    let mut cmd = options.command(&runtime.program)?;
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = OwnedProcess::spawn(cmd)?;
    let result = process.output().await?;
    if !result.status.success() {
        return Err(Error::Refused(format!(
            "OpenCode {} exited with {}",
            args[0], result.status
        )));
    }
    String::from_utf8(result.stdout)
        .map_err(|_| Error::Protocol("OpenCode returned non-UTF-8 catalog output".into()))
}

pub(crate) async fn list(runtime: &Runtime, options: &SessionOptions) -> Result<Vec<ModelEntry>> {
    let read = async {
        let (runtime, installed) = runtime.selected_with_version(options).await?;
        let version = match installed {
            Some(version) => version,
            None => output(&runtime, options, &["--version"]).await?,
        };
        let flag = release_flag(&version)?;
        let stdout = output(&runtime, options, &["models", flag]).await?;
        if flag == "--verbose" {
            Ok(legacy(&stdout))
        } else {
            standalone(&stdout)
        }
    };
    tokio::time::timeout(options.handshake_timeout, read)
        .await
        .map_err(|_| Error::Timeout("OpenCode model listing".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_release_lines_preserve_catalog_evidence() {
        assert_eq!(release_flag("opencode  v1.19.5dev\n").unwrap(), "--verbose");
        assert_eq!(release_flag("2.3.2+local").unwrap(), "--standalone");
        for bad in ["opencode vv2.3.2", "prefix 1.2.3", "3.0.0", "2.3.2 extra"] {
            assert!(matches!(release_flag(bad), Err(Error::Protocol(_))));
        }
        let source = "stray\r\nteam/large\r\n{\r\n  \"name\":\" Large \",\r\n  \"providerID\":\"api\",\r\n  \"variants\":{\"max\":{},\"high\":{}}\r\n}\r\napi/plain\r\napi/next\r\n{\r\n  \"name\":\" \"\r\n}\r\n";
        assert_eq!(
            serde_json::to_value(legacy(source)).unwrap(),
            json!([{"id":"team/large","displayName":"Large","provider":{"id":"api"},"effortLevels":["max","high"]},{"id":"api/next"}])
        );
        assert!(legacy("api/plain\nnoise\n").is_empty());
        assert_eq!(
            serde_json::to_value(standalone(" a/b/c \r\n\na/d\r\n").unwrap()).unwrap(),
            json!([{"id":"a/b/c"},{"id":"a/d"}])
        );
        assert!(matches!(standalone(" \n"), Err(Error::Unsupported(_))));
        for bad in ["no-provider", "a/", "/a", "a/b description"] {
            assert!(matches!(standalone(bad), Err(Error::Protocol(_))));
        }
    }
}
