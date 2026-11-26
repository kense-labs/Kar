use super::{Class, TurnFailure, from_code, from_status, status};
use crate::failure::prose;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub(crate) enum AcpFailures {
    Generic,
    OpenCode,
    CodeBuddy,
    Cursor,
    Devin,
    Grok { final_retry: Option<Class> },
    Antigravity(QuotaNotice),
}

impl AcpFailures {
    pub(crate) fn new(runtime: &str) -> Self {
        match runtime {
            "opencode" => Self::OpenCode,
            "codebuddy" => Self::CodeBuddy,
            "cursor" => Self::Cursor,
            "devin" => Self::Devin,
            "grok" => Self::Grok { final_retry: None },
            "antigravity" => Self::Antigravity(QuotaNotice::default()),
            _ => Self::Generic,
        }
    }

    pub(crate) fn start(&mut self) {
        match self {
            Self::Grok { final_retry } => *final_retry = None,
            Self::Antigravity(notice) => *notice = QuotaNotice::default(),
            _ => {}
        }
    }

    pub(crate) fn observe(&mut self, packet: &Value, received_ms: u64) {
        let update = &packet["params"]["update"];
        match self {
            Self::Grok { final_retry }
                if packet["method"] == "_x.ai/session_notification"
                    && update["sessionUpdate"] == "retry_state"
                    && matches!(update["type"].as_str(), Some("failed" | "exhausted")) =>
            {
                *final_retry = match update["error_type"].as_str() {
                    Some("auth") => Some(Class::Auth),
                    Some("rate_limited") => Some(Class::RateLimited),
                    Some("context_length") => Some(Class::InputTooLarge),
                    Some("api") => Some(Class::Provider),
                    _ => None,
                };
            }
            Self::Antigravity(notice) if packet["method"].is_string() => {
                if packet.get("id").is_some() || packet["method"] != "session/update" {
                    notice.blocked = true;
                } else {
                    notice.update(update, received_ms);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn normalize_opencode_error(&self, error: &Value) -> Option<&'static str> {
        if !matches!(self, Self::OpenCode) {
            return None;
        }
        let matches_goal = [
            error["message"].as_str(),
            error["data"].as_str(),
            error["data"]["message"].as_str(),
        ]
        .into_iter()
        .flatten()
        .any(is_opencode_no_paused_goal);
        matches_goal.then_some("OpenCode has no paused goal to resume.")
    }

    pub(crate) fn rejected(&self, error: &Value, received_ms: u64) -> TurnFailure {
        if let Some(mapped) = self.normalize_opencode_error(error) {
            return TurnFailure::new(Class::InvalidRequest, mapped);
        }
        let code = error["code"].as_i64();
        let mut reason = error["message"]
            .as_str()
            .unwrap_or("ACP prompt failed")
            .to_owned();
        if let Some(detail) = error["data"].as_str().filter(|s| !s.is_empty()) {
            reason.push_str(": ");
            reason.push_str(detail);
        }
        if matches!(self, Self::OpenCode) && is_opencode_no_paused_goal(&reason) {
            return TurnFailure::new(
                Class::InvalidRequest,
                "OpenCode has no paused goal to resume.",
            );
        }
        let mut http = None;
        let named = match (self, code) {
            (_, Some(-32000)) => Some(Class::Auth),
            (Self::OpenCode, Some(-32603)) => opencode_class(error["data"]["errorName"].as_str()),
            (Self::Grok { .. }, Some(-32003)) => {
                http = Some(429);
                let named = reason
                    .split_once("API error (status ")
                    .and_then(|(_, s)| s.split_once("): "))
                    .and_then(|(_, s)| s.split_once(':'))
                    .and_then(|(name, _)| from_code(name));
                Some(if reason.contains("subscription:free-usage-exhausted") {
                    Class::Quota
                } else {
                    named
                        .filter(|c| matches!(c, Class::Billing | Class::Quota))
                        .unwrap_or(Class::RateLimited)
                })
            }
            (Self::Grok { final_retry }, Some(-32603)) => {
                http = status(&error["data"]["http_status"]);
                let by_status = from_status(http);
                match final_retry {
                    None | Some(Class::Provider) if by_status != Class::Unknown => Some(by_status),
                    other => *other,
                }
            }
            (Self::Antigravity(_), Some(_)) => {
                if let Some(failure) = quota_countdown(&reason, received_ms, false) {
                    return failure;
                }
                if reason.contains("QUOTA_EXHAUSTED") {
                    Some(Class::Quota)
                } else if reason.contains("RESOURCE_EXHAUSTED") {
                    Some(Class::RateLimited)
                } else {
                    None
                }
            }
            _ => None,
        };
        let mut class = named.unwrap_or_else(|| prose(&reason));
        let explicit_reset = extract_reset_at_ms(Some(error), &reason, received_ms, false);
        if class == Class::Unknown && explicit_reset.is_some() {
            class = if reason.contains("quotaReset") {
                Class::Quota
            } else {
                Class::RateLimited
            };
        }
        let resets_at_ms = explicit_reset.or_else(|| {
            matches!(class, Class::Quota | Class::RateLimited)
                .then(|| extract_reset_at_ms(Some(error), &reason, received_ms, true))
                .flatten()
        });
        TurnFailure {
            status: http,
            resets_at_ms,
            ..TurnFailure::new(class, reason)
        }
    }

    pub(crate) fn result_failure(&self, result: &Value) -> Option<TurnFailure> {
        if result["stopReason"] == "cancelled" {
            return None;
        }
        match self {
            Self::CodeBuddy => codebuddy_result(result),
            Self::Antigravity(n)
                if result["stopReason"] == "end_turn"
                    && !n.blocked
                    && (n.prior_tool || result.get("usage").is_none()) =>
            {
                quota_countdown(&n.text, n.received_ms, true)
            }
            _ => None,
        }
    }
}

pub(super) fn opencode_class(name: Option<&str>) -> Option<Class> {
    Some(match name? {
        "provider.auth" => Class::Auth,
        "provider.free-limit"
        | "provider.go-limit"
        | "provider.budget-limit"
        | "provider.model-budget-limit"
        | "provider.chatgpt-limit"
        | "provider.quota" => Class::Quota,
        "provider.rate-limit" => Class::RateLimited,
        "provider.content-filter" => Class::ModelRefused,
        "provider.invalid-request" => Class::InvalidRequest,
        _ => return None,
    })
}

fn codebuddy_result(result: &Value) -> Option<TurnFailure> {
    let meta = &result["_meta"];
    let outcome = meta["codebuddy.ai/outcome"].as_str();
    let failed = outcome.is_some_and(|name| name.starts_with("FAILED"));
    let raw = meta.get("codebuddy.ai/errorMessage");
    if !failed && (result["stopReason"] != "refusal" || raw.is_none()) {
        return None;
    }
    let text = raw.and_then(Value::as_str).unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(text)
        .ok()
        .filter(Value::is_object);
    let error = parsed.as_ref().unwrap_or(&Value::Null);
    let nested = &error["error"];
    let data = &error["data"];
    let inner = &nested["data"];
    let http = status(&data["statusCode"]).or_else(|| status(&inner["statusCode"]));
    let class = match data["category"]
        .as_str()
        .or_else(|| inner["category"].as_str())
    {
        Some("quota") => Class::Quota,
        Some("auth" | "custom_model_auth") => Class::Auth,
        Some("model_service" | "empty_model_response") => Class::Provider,
        Some("network" | "internal") => Class::Unknown,
        _ => from_status(http),
    };
    let reason = error["message"]
        .as_str()
        .or_else(|| nested["message"].as_str())
        .or_else(|| (parsed.is_none() && !text.is_empty()).then_some(text))
        .or(outcome)
        .or_else(|| result["stopReason"].as_str())
        .unwrap_or("CodeBuddy task failed");
    Some(TurnFailure {
        status: http,
        ..TurnFailure::new(class, reason)
    })
}

#[derive(Default)]
pub(crate) struct QuotaNotice {
    text: String,
    received_ms: u64,
    prior_tool: bool,
    blocked: bool,
}

impl QuotaNotice {
    fn update(&mut self, update: &Value, received_ms: u64) {
        match update["sessionUpdate"].as_str() {
            Some("tool_call" | "tool_call_update") => {
                *self = Self {
                    prior_tool: true,
                    ..Self::default()
                };
            }
            Some(
                "session_info_update"
                | "config_option_update"
                | "available_commands_update"
                | "current_mode_update",
            ) => {}
            Some("usage_update") => self.blocked |= !self.text.is_empty(),
            Some("agent_message_chunk") if !self.blocked && update["content"]["type"] == "text" => {
                if let Some(text) = update["content"]["text"]
                    .as_str()
                    .filter(|s| s.len() <= 256 - self.text.len())
                {
                    self.text.push_str(text);
                    self.received_ms = received_ms;
                } else {
                    self.blocked = true;
                }
            }
            _ => self.blocked = true,
        }
    }
}

const MAX_RESET_AT_MS: u64 = 8_640_000_000_000_000;

fn is_opencode_no_paused_goal(text: &str) -> bool {
    let trimmed = text.trim();
    matches!(
        trimmed,
        "No paused goal to resume" | "No paused goal to resume."
    ) || trimmed.ends_with(": No paused goal to resume")
        || trimmed.ends_with(": No paused goal to resume.")
}

fn duration_boundary_ok(after: &str) -> bool {
    let mut chars = after.chars();
    match chars.next() {
        None => true,
        Some(c) if c.is_ascii_alphanumeric() || matches!(c, '_' | 'µ' | 'μ') => false,
        Some('.') => !chars.next().is_some_and(|d| d.is_ascii_digit()),
        Some(_) => true,
    }
}

fn parse_go_duration_ms(raw: &str) -> Option<u64> {
    static FULL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:[0-9]+(?:\.[0-9]+)?(?:ns|us|µs|μs|ms|h|m|s))+$").unwrap()
    });
    static PART: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"([0-9]+(?:\.[0-9]+)?)(ns|us|µs|μs|ms|h|m|s)").unwrap());
    let raw = raw.trim();
    if !FULL.is_match(raw) {
        return None;
    }
    let mut total_ms = 0.0_f64;
    for caps in PART.captures_iter(raw) {
        let amount: f64 = caps.get(1)?.as_str().parse().ok()?;
        let unit_ms = match caps.get(2)?.as_str() {
            "ns" => 1e-6,
            "us" | "µs" | "μs" => 1e-3,
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            _ => return None,
        };
        total_ms += amount * unit_ms;
    }
    if !total_ms.is_finite() || total_ms <= 0.0 || total_ms > MAX_RESET_AT_MS as f64 {
        return None;
    }
    Some((total_ms.round() as u64).max(1))
}

fn add_delay_ms(received_ms: u64, delay_ms: u64) -> Option<u64> {
    received_ms
        .checked_add(delay_ms)
        .filter(|&reset| reset <= MAX_RESET_AT_MS)
}

fn parse_timestamp_ms(raw: &str) -> Option<u64> {
    let parse_once = |candidate: &str| -> Option<u64> {
        let is_numeric = !candidate.is_empty()
            && (candidate.bytes().all(|b| b.is_ascii_digit())
                || candidate
                    .split_once('.')
                    .is_some_and(|(int_part, frac_part)| {
                        !int_part.is_empty()
                            && !frac_part.is_empty()
                            && int_part.bytes().all(|b| b.is_ascii_digit())
                            && frac_part.bytes().all(|b| b.is_ascii_digit())
                    }));
        if is_numeric {
            let val: f64 = candidate.parse().ok()?;
            if !val.is_finite() || val <= 0.0 {
                return None;
            }
            let ms = if val < 100_000_000_000.0 {
                (val * 1_000.0).round()
            } else {
                val.round()
            };
            if ms <= 0.0 || ms > MAX_RESET_AT_MS as f64 {
                return None;
            }
            return Some(ms as u64);
        }
        let ts: jiff::Timestamp = candidate.parse().ok()?;
        u64::try_from(ts.as_millisecond())
            .ok()
            .filter(|&ms| ms > 0 && ms <= MAX_RESET_AT_MS)
    };
    let trimmed = raw.trim();
    parse_once(trimmed).or_else(|| trimmed.strip_suffix('.').and_then(parse_once))
}

fn extract_value_field<'a>(value: &'a Value, key: &str, depth: usize) -> Option<&'a Value> {
    if depth > 8 {
        return None;
    }
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).filter(|v| !v.is_null()) {
                return Some(found);
            }
            for child in map.values() {
                if let Some(found) = extract_value_field(child, key, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => {
            for item in items {
                if let Some(found) = extract_value_field(item, key, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_reset_from_value(error: &Value, received_ms: u64) -> Option<u64> {
    if let Some(ts_val) = extract_value_field(error, "quotaResetTimeStamp", 0) {
        let parsed = match ts_val {
            Value::String(s) => parse_timestamp_ms(s),
            Value::Number(n) => parse_timestamp_ms(&n.to_string()),
            _ => None,
        };
        if parsed.is_some() {
            return parsed;
        }
    }
    for key in ["quotaResetDelay", "retryDelay"] {
        if let Some(delay_str) = extract_value_field(error, key, 0).and_then(Value::as_str)
            && let Some(delay_ms) = parse_go_duration_ms(delay_str)
            && let Some(reset) = add_delay_ms(received_ms, delay_ms)
        {
            return Some(reset);
        }
    }
    None
}

fn extract_reset_at_ms(
    error: Option<&Value>,
    text: &str,
    received_ms: u64,
    include_resets_in: bool,
) -> Option<u64> {
    static RESET_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\bquotaResetTimeStamp["']?\s*[:=]\s*["']?([^\s"',\]}]+)"#).unwrap()
    });
    static RESET_DELAY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\bquotaResetDelay["']?\s*[:=]\s*["']?((?:[0-9]+(?:\.[0-9]+)?(?:ns|us|µs|μs|ms|h|m|s))+)"#).unwrap()
    });
    static RETRY_DELAY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"\bretryDelay["']?\s*[:=]\s*["']?((?:[0-9]+(?:\.[0-9]+)?(?:ns|us|µs|μs|ms|h|m|s))+)"#,
        )
        .unwrap()
    });
    static RESETS_IN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bResets in ((?:[0-9]+(?:\.[0-9]+)?(?:ns|us|µs|μs|ms|h|m|s))+)").unwrap()
    });

    if let Some(err) = error
        && let Some(reset) = extract_reset_from_value(err, received_ms)
    {
        return Some(reset);
    }
    if let Some(caps) = RESET_TIMESTAMP.captures(text)
        && let Some(ts) = parse_timestamp_ms(caps.get(1)?.as_str())
    {
        return Some(ts);
    }
    for pat in [&*RESET_DELAY, &*RETRY_DELAY] {
        if let Some(caps) = pat.captures(text)
            && let Some(m) = caps.get(1)
            && duration_boundary_ok(&text[m.end()..])
            && let Some(delay_ms) = parse_go_duration_ms(m.as_str())
            && let Some(reset) = add_delay_ms(received_ms, delay_ms)
        {
            return Some(reset);
        }
    }
    if include_resets_in {
        for caps in RESETS_IN.captures_iter(text) {
            if let Some(m) = caps.get(1)
                && duration_boundary_ok(&text[m.end()..])
                && let Some(delay_ms) = parse_go_duration_ms(m.as_str())
                && let Some(reset) = add_delay_ms(received_ms, delay_ms)
            {
                return Some(reset);
            }
        }
    }
    None
}

fn quota_countdown(reason: &str, received_ms: u64, standalone: bool) -> Option<TurnFailure> {
    static NOTICE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"Individual quota reached\. Please upgrade your subscription to increase your limits\. Resets in ((?:[0-9]+(?:\.[0-9]+)?(?:ns|us|µs|μs|ms|h|m|s))+)\.",
        )
        .unwrap()
    });
    let caps = NOTICE.captures(reason)?;
    if standalone && reason.strip_prefix("\n⚠ ")?.strip_suffix('\n')? != caps.get(0)?.as_str() {
        return None;
    }
    let delay_ms = parse_go_duration_ms(caps.get(1)?.as_str())?;
    let reset = add_delay_ms(received_ms, delay_ms)?;
    Some(TurnFailure {
        resets_at_ms: Some(reset),
        ..TurnFailure::new(Class::Quota, reason)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_fractional_go_durations_timestamps_and_retry_delays() {
        let now = 1_700_000_000_000_u64;
        let agy = AcpFailures::new("antigravity");

        // Integer & fractional "Resets in ..." in standalone and rejected Antigravity notices.
        let int_notice = agy.rejected(
            &json!({
                "code": -32603,
                "message": "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h2m3s."
            }),
            now,
        );
        assert_eq!(int_notice.class, Class::Quota);
        assert_eq!(int_notice.resets_at_ms, Some(now + 3_723_000));

        let frac_notice = agy.rejected(
            &json!({
                "code": -32603,
                "message": "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 4m42.5s."
            }),
            now,
        );
        assert_eq!(frac_notice.class, Class::Quota);
        assert_eq!(frac_notice.resets_at_ms, Some(now + 282_500));

        let subsecond_notice = agy.rejected(
            &json!({
                "code": -32603,
                "message": "Rate limit exceeded. Resets in 500ms."
            }),
            now,
        );
        assert_eq!(subsecond_notice.class, Class::RateLimited);
        assert_eq!(subsecond_notice.resets_at_ms, Some(now + 500));

        // All Go duration units: h, m, s, ms, us, µs, ns.
        assert_eq!(parse_go_duration_ms("1h2m3.5s"), Some(3_723_500));
        assert_eq!(parse_go_duration_ms("500ms"), Some(500));
        assert_eq!(parse_go_duration_ms("2500us"), Some(3));
        assert_eq!(parse_go_duration_ms("2000µs"), Some(2));
        assert_eq!(parse_go_duration_ms("100ns"), Some(1));

        // quotaResetTimeStamp: ISO-8601, epoch seconds, epoch milliseconds.
        let iso_err = agy.rejected(
            &json!({
                "code": -32603,
                "message": "Error 429, Message: quota exhausted [reason:QUOTA_EXHAUSTED quotaResetTimeStamp:2026-11-26T10:00:00Z quotaResetDelay:99s], Status: RESOURCE_EXHAUSTED"
            }),
            now,
        );
        assert_eq!(iso_err.class, Class::Quota);
        let expected_iso: jiff::Timestamp = "2026-11-26T10:00:00Z".parse().unwrap();
        assert_eq!(
            iso_err.resets_at_ms,
            Some(u64::try_from(expected_iso.as_millisecond()).unwrap())
        );

        let epoch_s = agy.rejected(
            &json!({
                "code": -32603,
                "message": "QUOTA_EXHAUSTED",
                "data": {"quotaResetTimeStamp": "1764151200"}
            }),
            now,
        );
        assert_eq!(epoch_s.resets_at_ms, Some(1_764_151_200_000));

        let epoch_ms = agy.rejected(
            &json!({
                "code": -32603,
                "message": "QUOTA_EXHAUSTED [quotaResetTimeStamp:1764151200500]"
            }),
            now,
        );
        assert_eq!(epoch_ms.resets_at_ms, Some(1_764_151_200_500));

        // Unparseable quotaResetTimeStamp falls back to quotaResetDelay.
        let fallback_delay = agy.rejected(
            &json!({
                "code": -32603,
                "message": "[reason:QUOTA_EXHAUSTED quotaResetTimeStamp:not-a-timestamp quotaResetDelay:1m2.5s]"
            }),
            now,
        );
        assert_eq!(fallback_delay.resets_at_ms, Some(now + 62_500));

        // retryDelay in prose and structured JSON.
        let retry_prose = agy.rejected(
            &json!({
                "code": -32603,
                "message": "RESOURCE_EXHAUSTED [retryDelay:12.5s]"
            }),
            now,
        );
        assert_eq!(retry_prose.class, Class::RateLimited);
        assert_eq!(retry_prose.resets_at_ms, Some(now + 12_500));

        let retry_json = AcpFailures::new("generic").rejected(
            &json!({
                "code": -32603,
                "message": "Too Many Requests",
                "data": {"details": [{"retryDelay": "12.5s"}]}
            }),
            now,
        );
        assert_eq!(retry_json.resets_at_ms, Some(now + 12_500));

        // OpenCode "No paused goal to resume" mapping (#43250).
        let oc = AcpFailures::new("opencode").rejected(
            &json!({"code": -32602, "message": "No paused goal to resume"}),
            now,
        );
        assert_eq!(oc.class, Class::InvalidRequest);
        assert_eq!(oc.reason, "OpenCode has no paused goal to resume.");
    }
}
