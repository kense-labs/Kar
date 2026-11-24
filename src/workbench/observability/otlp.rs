use super::*;
use axum::Json;
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Signal {
    Traces,
    Metrics,
    Logs,
}
impl Signal {
    fn name(&self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Metrics => "metrics",
            Self::Logs => "logs",
        }
    }
    fn key(&self) -> &'static str {
        match self {
            Self::Traces => "resourceSpans",
            Self::Metrics => "resourceMetrics",
            Self::Logs => "resourceLogs",
        }
    }
    fn setting(&self, field: &str) -> Option<String> {
        std::env::var(format!(
            "OTEL_EXPORTER_OTLP_{}_{field}",
            self.name().to_uppercase()
        ))
        .ok()
        .filter(|s| !s.is_empty())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Check {
    signal: Signal,
    url: String,
}
pub(super) async fn check(
    Json(input): Json<Check>,
) -> std::result::Result<Json<Value>, super::super::http::ApiError> {
    let url =
        url::Url::parse(&input.url).map_err(|_| Error::Refused("invalid OTLP endpoint".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Refused("invalid OTLP HTTP endpoint".into()).into());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(Error::from)?;
    let signal = &input.signal;
    let mut request = client.post(url);
    let configured = signal.setting("ENDPOINT").or_else(|| {
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .map(|v| format!("{}/v1/{}", v.trim_end_matches('/'), signal.name()))
    });
    if configured.as_deref() == Some(&input.url)
        && let Some(headers) = signal
            .setting("HEADERS")
            .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_HEADERS").ok())
    {
        for member in headers.split(',') {
            let Some((k, v)) = member.trim().split_once('=') else {
                continue;
            };
            let value = percent_encoding::percent_decode_str(v.trim())
                .decode_utf8()
                .map_err(|_| Error::Refused("invalid OTLP export header".into()))?;
            let key = reqwest::header::HeaderName::from_bytes(k.trim().as_bytes())
                .map_err(|_| Error::Refused("invalid OTLP export header".into()))?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| Error::Refused("invalid OTLP export header".into()))?;
            request = request.header(key, value);
        }
    }
    let protocol = signal
        .setting("PROTOCOL")
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL").ok())
        .unwrap_or_else(|| "http/protobuf".into());
    request = match protocol.as_str() {
        "http/json" => request.json(&json!({(signal.key()):[]})),
        "http/protobuf" => request
            .header("content-type", "application/x-protobuf")
            .body(Vec::new()),
        _ => {
            return Err(Error::Unsupported(
                "OTLP checks require an HTTP/JSON or HTTP/protobuf exporter".into(),
            )
            .into());
        }
    };
    let start = Instant::now();
    let result = match tokio::time::timeout(Duration::from_secs(5), request.send()).await {
        Ok(Ok(r)) if r.status().is_success() => {
            json!({"type":"accepted","latency_ms":start.elapsed().as_millis() as u64})
        }
        Ok(Ok(r)) => json!({"type":"rejected","status":r.status().as_u16()}),
        Ok(Err(e)) => json!({"type":"unreachable","timed_out":e.is_timeout()}),
        Err(_) => json!({"type":"unreachable","timed_out":true}),
    };
    Ok(Json(result))
}
