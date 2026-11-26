//! Read OpenCode's native model catalog without opening an ACP session or persistent daemon.
use crate::{Error, Result, Runtime, SessionOptions, forms::ModelEntry, process::OwnedProcess};
use indexmap::IndexMap;
use regex::Regex;
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::de::IgnoredAny;
use serde_json::{Map, Value, json, value::RawValue};
use std::{collections::HashSet, process::Stdio, sync::LazyLock, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    time::Instant,
};

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
            {
                let levels: Vec<String> = variants
                    .into_keys()
                    .filter(|level| level != "default")
                    .collect();
                if !levels.is_empty() {
                    entry.effort_levels = Some(levels);
                }
            }
        }
        result.push(entry);
    }
    result
}

fn non_whitespace_token(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
        .ok_or_else(|| {
            Error::Protocol("OpenCode model catalog contains an invalid identifier".into())
        })
}

fn extract_effort_levels(variants: &Value) -> Result<Option<Vec<String>>> {
    let mut seen = HashSet::new();
    let mut levels = Vec::new();
    let mut push = |id: &str| {
        if id != "default" && seen.insert(id.to_owned()) {
            levels.push(id.to_owned());
        }
    };
    match variants {
        Value::Null => {}
        Value::Array(items) => {
            for item in items {
                let id = item
                    .as_object()
                    .and_then(|obj| obj.get("id"))
                    .and_then(Value::as_str)
                    .or_else(|| item.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::Protocol("OpenCode model variant has no id".into()))?;
                push(id);
            }
        }
        Value::Object(map) => {
            for key in map.keys() {
                if key.is_empty() {
                    return Err(Error::Protocol("OpenCode model variant has no id".into()));
                }
                push(key);
            }
        }
        _ => {
            return Err(Error::Protocol(
                "OpenCode model variants are malformed".into(),
            ));
        }
    }
    Ok((!levels.is_empty()).then_some(levels))
}

fn project_v2_row(provider: &str, row: &Value) -> Result<ModelEntry> {
    let raw_id = non_whitespace_token(&row["id"])?;
    let mut entry = model(&format!("{provider}/{raw_id}"));
    entry
        .metadata
        .insert("provider".into(), json!({ "id": provider }));
    if let Some(name) = row["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        entry
            .metadata
            .insert("displayName".into(), Value::String(name.to_owned()));
    }
    if let Some(variants) = row.get("variants") {
        entry.effort_levels = extract_effort_levels(variants)?;
    }
    Ok(entry)
}

fn project_v2_models(payload: &Value) -> Result<Vec<ModelEntry>> {
    let mut models = Vec::new();
    if let Some(rows) = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
    {
        for row in rows {
            if !row.is_object() {
                return Err(Error::Protocol(
                    "OpenCode model row is not an object".into(),
                ));
            }
            let provider = non_whitespace_token(&row["providerID"])?;
            models.push(project_v2_row(provider, row)?);
        }
    } else if let Some(providers) = payload
        .get("providers")
        .or_else(|| payload.pointer("/data/providers"))
        .or_else(|| payload.get("all"))
        .and_then(Value::as_array)
    {
        for provider_obj in providers {
            let provider = non_whitespace_token(&provider_obj["id"])?;
            match &provider_obj["models"] {
                Value::Object(map) => {
                    for (fallback_id, row) in map {
                        if !row.is_object() {
                            return Err(Error::Protocol(
                                "OpenCode provider model row is not an object".into(),
                            ));
                        }
                        if row.get("id").is_some_and(Value::is_string) {
                            models.push(project_v2_row(provider, row)?);
                        } else {
                            let mut patched = row.clone();
                            patched["id"] = Value::String(fallback_id.clone());
                            models.push(project_v2_row(provider, &patched)?);
                        }
                    }
                }
                Value::Array(rows) => {
                    for row in rows {
                        models.push(project_v2_row(provider, row)?);
                    }
                }
                Value::Null => {}
                _ => {
                    return Err(Error::Protocol(
                        "OpenCode provider models are malformed".into(),
                    ));
                }
            }
        }
    } else {
        return Err(Error::Protocol(
            "OpenCode returned an unrecognized model catalog envelope".into(),
        ));
    }
    if models.is_empty() {
        return Err(Error::Unsupported(
            "OpenCode returned an empty model snapshot before catalog readiness could be established"
                .into(),
        ));
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

fn parse_loopback_origin(line: &[u8]) -> Option<Url> {
    let candidate = if let Ok(frame) = serde_json::from_slice::<Value>(line) {
        frame["url"].as_str().map(str::to_owned)
    } else {
        std::str::from_utf8(line)
            .ok()
            .map(str::trim)
            .and_then(|s| s.rsplit_once("listening on "))
            .map(|(_, url)| url.trim().to_owned())
    }?;
    let url = Url::parse(&candidate).ok()?;
    (url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some_and(|p| p > 0)
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

async fn fetch_json(
    client: &Client,
    origin: &Url,
    password: &str,
    path: &str,
) -> std::result::Result<(StatusCode, Value), ()> {
    let url = origin.join(path.trim_start_matches('/')).map_err(|_| ())?;
    let mut res = client
        .get(url)
        .basic_auth("opencode", Some(password))
        .send()
        .await
        .map_err(|_| ())?;
    let status = res.status();
    if !status.is_success() {
        return Ok((status, Value::Null));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|_| ())? {
        if bytes.len().saturating_add(chunk.len()) > 16 * 1024 * 1024 {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    Ok((status, value))
}

async fn serve_models(
    runtime: &Runtime,
    options: &SessionOptions,
) -> Option<Result<Vec<ModelEntry>>> {
    let password = uuid::Uuid::new_v4().simple().to_string();
    let mut cmd = options.command(&runtime.program).ok()?;
    cmd.args(["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"])
        .env("OPENCODE_PASSWORD", &password)
        .env("OPENCODE_SERVER_PASSWORD", &password)
        .env_remove("OPENCODE_SERVER_USERNAME")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    cmd.env("BROWSER", "/usr/bin/true");
    let mut child = OwnedProcess::spawn(cmd).ok()?;
    let startup_budget = options.handshake_timeout.min(Duration::from_secs(5));
    let mut reader = BufReader::new(child.inner().stdout.take()?);
    let announce = tokio::time::timeout(startup_budget, async {
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = (&mut reader)
                .take(65537)
                .read_until(b'\n', &mut line)
                .await
                .ok()?;
            if n == 0 || n > 65536 {
                return None;
            }
            if let Some(origin) = parse_loopback_origin(&line) {
                return Some(origin);
            }
        }
    })
    .await;
    let Some(origin) = announce.ok().flatten() else {
        child.stop().await;
        return None;
    };
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    });
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(directory) = options.cwd.to_str() {
        let encoded =
            percent_encoding::utf8_percent_encode(directory, percent_encoding::NON_ALPHANUMERIC);
        if let Ok(header) = encoded.to_string().parse() {
            headers.insert("x-opencode-directory", header);
        }
    }
    let Ok(client) = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .default_headers(headers)
        .timeout(options.handshake_timeout)
        .build()
    else {
        child.stop().await;
        return None;
    };
    let outcome = async {
        // Native v2 CLI polls initial plugins up to five seconds before reading /api/model.
        let poll_until = Instant::now() + Duration::from_secs(5);
        loop {
            match fetch_json(&client, &origin, &password, "/api/plugin").await {
                Ok((StatusCode::OK, body)) => {
                    let empty = body["data"]
                        .as_array()
                        .or_else(|| body.as_array())
                        .is_some_and(Vec::is_empty);
                    if !empty || Instant::now() >= poll_until {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok((StatusCode::NOT_FOUND, _)) => break,
                _ => return None,
            }
        }
        for route in [
            "/api/model",
            "/api/provider",
            "/provider",
            "/config/providers",
        ] {
            match fetch_json(&client, &origin, &password, route).await {
                Ok((StatusCode::OK, body)) => return Some(project_v2_models(&body)),
                Ok((StatusCode::NOT_FOUND, _)) => continue,
                _ => return None,
            }
        }
        None
    }
    .await;
    child.stop().await;
    outcome
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
        if flag == "--verbose" {
            let stdout = output(&runtime, options, &["models", "--verbose"]).await?;
            Ok(legacy(&stdout))
        } else if let Some(result) = serve_models(&runtime, options).await {
            result
        } else {
            let stdout = output(&runtime, options, &["models", "--standalone"]).await?;
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
        let source = "stray\r\nteam/large\r\n{\r\n  \"name\":\" Large \",\r\n  \"providerID\":\"api\",\r\n  \"variants\":{\"default\":{},\"max\":{},\"high\":{}}\r\n}\r\napi/default-only\r\n{\r\n  \"variants\":{\"default\":{}}\r\n}\r\napi/plain\r\napi/next\r\n{\r\n  \"name\":\" \"\r\n}\r\n";
        assert_eq!(
            serde_json::to_value(legacy(source)).unwrap(),
            json!([
                {"id":"team/large","displayName":"Large","provider":{"id":"api"},"effortLevels":["max","high"]},
                {"id":"api/default-only"},
                {"id":"api/next"}
            ])
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

    #[test]
    fn v2_catalog_projects_metadata_and_strips_default_variant() {
        let api_model = json!({
            "data": [
                {
                    "providerID": "openai",
                    "id": "gpt-5",
                    "name": " GPT-5 ",
                    "variants": [{"id": "default"}, {"id": "low"}, {"id": "high"}, {"id": "low"}]
                },
                {
                    "providerID": "anthropic",
                    "id": "claude-sonnet-4-5",
                    "name": "   ",
                    "variants": [{"id": "default"}]
                }
            ]
        });
        assert_eq!(
            serde_json::to_value(project_v2_models(&api_model).unwrap()).unwrap(),
            json!([
                {
                    "id": "anthropic/claude-sonnet-4-5",
                    "provider": {"id": "anthropic"}
                },
                {
                    "id": "openai/gpt-5",
                    "displayName": "GPT-5",
                    "provider": {"id": "openai"},
                    "effortLevels": ["low", "high"]
                }
            ])
        );

        let config_providers = json!({
            "providers": [
                {
                    "id": "openrouter",
                    "models": {
                        "x-ai/grok-4": {
                            "name": "Grok 4",
                            "variants": {"default": {}, "high": {}}
                        }
                    }
                }
            ]
        });
        assert_eq!(
            serde_json::to_value(project_v2_models(&config_providers).unwrap()).unwrap(),
            json!([{
                "id": "openrouter/x-ai/grok-4",
                "displayName": "Grok 4",
                "provider": {"id": "openrouter"},
                "effortLevels": ["high"]
            }])
        );

        assert!(matches!(
            project_v2_models(&json!({"data": []})),
            Err(Error::Unsupported(_))
        ));
        assert!(parse_loopback_origin(br#"{"url":"http://127.0.0.1:43210"}"#).is_some());
        assert!(
            parse_loopback_origin(b"opencode server listening on http://127.0.0.1:43210\n")
                .is_some()
        );
        assert!(parse_loopback_origin(br#"{"url":"http://0.0.0.0:43210"}"#).is_none());
    }
}
