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

    pub(crate) fn rejected(&self, error: &Value, received_ms: u64) -> TurnFailure {
        let code = error["code"].as_i64();
        let mut reason = error["message"]
            .as_str()
            .unwrap_or("ACP prompt failed")
            .to_owned();
        if let Some(detail) = error["data"].as_str().filter(|s| !s.is_empty()) {
            reason.push_str(": ");
            reason.push_str(detail);
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
                None
            }
            _ => None,
        };
        TurnFailure {
            status: http,
            ..TurnFailure::new(named.unwrap_or_else(|| prose(&reason)), reason)
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

fn quota_countdown(reason: &str, received_ms: u64, standalone: bool) -> Option<TurnFailure> {
    static NOTICE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"Individual quota reached\. Please upgrade your subscription to increase your limits\. Resets in (?:([0-9]+)h)?(?:([0-9]+)m)?(?:([0-9]+)s)?\."
    ).unwrap()
    });
    let caps = NOTICE.captures(reason)?;
    if !(1..=3).any(|i| caps.get(i).is_some()) {
        return None;
    }
    if standalone && reason.strip_prefix("\n⚠ ")?.strip_suffix('\n')? != caps.get(0)?.as_str() {
        return None;
    }
    let mut reset = received_ms;
    for (index, factor) in [(1, 3_600_000), (2, 60_000), (3, 1_000)] {
        if let Some(m) = caps.get(index) {
            reset = reset.checked_add(m.as_str().parse::<u64>().ok()?.checked_mul(factor)?)?;
        }
    }
    (reset <= 8_640_000_000_000_000).then(|| TurnFailure {
        resets_at_ms: Some(reset),
        ..TurnFailure::new(Class::Quota, reason)
    })
}
