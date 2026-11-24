use crate::{CredentialProblem, FailureClass as Class, TurnFailure};
use serde_json::Value;
mod codex_quota;
pub(crate) use codex_quota::CodexQuota;
mod acp;
pub(crate) use acp::AcpFailures;

fn status(value: &Value) -> Option<u16> {
    u16::try_from(value.as_u64()?)
        .ok()
        .filter(|s| (100..600).contains(s))
}

pub(crate) fn from_status(status: Option<u16>) -> Class {
    match status {
        Some(400) => Class::InvalidRequest,
        Some(401) => Class::Auth,
        Some(402) => Class::Billing,
        Some(404) => Class::ModelUnavailable,
        Some(429) => Class::RateLimited,
        Some(503 | 529) => Class::Overloaded,
        Some(500..=599) => Class::Provider,
        _ => Class::Unknown,
    }
}

pub(crate) fn opencode_history(error: &Value) -> Option<TurnFailure> {
    let reason = error["message"].as_str()?;
    let status = status(&error["status"]);
    let class = acp::opencode_class(error["type"].as_str()).unwrap_or_else(|| {
        let class = from_status(status);
        if class == Class::Unknown {
            crate::failure::prose(reason)
        } else {
            class
        }
    });
    Some(TurnFailure {
        status,
        ..TurnFailure::new(class, reason)
    })
}

fn from_code(code: &str) -> Option<Class> {
    Some(match code {
        "invalid_api_key" | "authentication_error" => Class::Auth,
        "model_not_found" | "not_found_error" => Class::ModelUnavailable,
        "rate_limit_exceeded" | "rate_limit_error" => Class::RateLimited,
        "insufficient_quota"
        | "organization_usage_limit_exceeded"
        | "usage_limit_reached"
        | "usage_not_included" => Class::Quota,
        "credit_balance_exhausted" | "billing_error" => Class::Billing,
        "context_length_exceeded" => Class::InputTooLarge,
        "server_is_overloaded" | "overloaded_error" => Class::Overloaded,
        "api_error" | "server_error" => Class::Provider,
        "invalid_request_error" => Class::InvalidRequest,
        _ => return None,
    })
}

fn invalid_request(reason: &str) -> Class {
    let lower = reason.to_ascii_lowercase();
    if crate::failure::context_overflow(reason) {
        Class::InputTooLarge
    } else if lower.contains("credit balance is too low") {
        Class::Billing
    } else if lower.contains("reached your specified api usage limits") {
        Class::Quota
    } else {
        Class::InvalidRequest
    }
}

fn body_class(error: &Value) -> Option<Class> {
    if let Some(class) = error["code"].as_str().and_then(from_code) {
        return Some(class);
    }
    if error["type"] == "invalid_request_error" {
        return Some(invalid_request(
            error["message"].as_str().unwrap_or_default(),
        ));
    }
    error["type"].as_str().and_then(from_code)
}

pub(crate) fn codex(error: &Value) -> TurnFailure {
    let reason = error["message"].as_str().unwrap_or("Codex turn failed");
    let info = &error["codexErrorInfo"];
    let mut failure = TurnFailure::new(Class::Unknown, reason);
    failure.class = match info.as_str() {
        Some("unauthorized") => Class::Auth,
        Some("modelNotFound") => Class::ModelUnavailable,
        Some("usageLimitExceeded") => Class::Quota,
        Some("rateLimitExceeded") => Class::RateLimited,
        Some("serverOverloaded") => Class::Overloaded,
        Some("internalServerError") => Class::Provider,
        Some("contextWindowExceeded") => Class::InputTooLarge,
        Some("other") => {
            let body = reason
                .strip_prefix("Error running remote compact task: ")
                .unwrap_or(reason);
            let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let error = &body["error"];
            let class = body_class(error).unwrap_or(Class::Unknown);
            if class == Class::Quota {
                failure.resets_at_ms = seconds_ms(&error["resets_at"]);
            }
            class
        }
        _ => {
            failure.status = [
                "httpConnectionFailed",
                "responseStreamConnectionFailed",
                "responseStreamDisconnected",
                "responseTooManyFailedAttempts",
            ]
            .iter()
            .find_map(|v| status(&info[*v]["httpStatusCode"]));
            from_status(failure.status)
        }
    };
    failure
}

fn seconds_ms(value: &Value) -> Option<u64> {
    value
        .as_u64()?
        .checked_mul(1000)
        .filter(|ms| *ms <= 8_640_000_000_000_000)
}

pub(super) fn pi(msg: &Value) -> TurnFailure {
    let reason = msg["errorMessage"].as_str().unwrap_or("Pi run failed");
    let (status, body) = reason
        .split_once(' ')
        .and_then(|(s, b)| {
            (s.len() == 3)
                .then(|| s.parse::<u16>().ok())
                .flatten()
                .filter(|s| (100..600).contains(s))
                .map(|s| (Some(s), b))
        })
        .unwrap_or((None, reason));
    let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let class = body_class(&body["error"]).unwrap_or_else(|| {
        if invalid_request(reason) == Class::InputTooLarge {
            Class::InputTooLarge
        } else {
            from_status(status)
        }
    });
    TurnFailure {
        status,
        ..TurnFailure::new(class, reason)
    }
}

pub(super) fn qwen(v: &Value) -> TurnFailure {
    let reason = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| v["subtype"].as_str().unwrap_or("Qwen turn failed"));
    TurnFailure::new(
        if v["subtype"] == "error_max_turns" {
            Class::LimitReached
        } else {
            Class::Unknown
        },
        reason,
    )
}

pub(crate) fn claude_reset(info: &Value) -> Option<u64> {
    if info["status"] != "rejected"
        || matches!(
            info["overageStatus"].as_str(),
            Some("allowed" | "allowed_warning")
        )
    {
        return None;
    }
    seconds_ms(&info["resetsAt"])
}

pub(crate) fn claude(
    result: &Value,
    assistant: &Value,
    last_reset: Option<u64>,
) -> Option<TurnFailure> {
    let category = assistant["error"].as_str();
    let synthetic = assistant["is_api_error_message"] == true
        || assistant
            .pointer("/message/model")
            .is_some_and(|v| v == "<synthetic>");
    let refused = result["stop_reason"] == "refusal"
        || assistant
            .pointer("/message/stop_reason")
            .is_some_and(|v| v == "refusal");
    let subtype = result["subtype"].as_str().filter(|s| *s != "success");
    if !refused
        && result["is_error"] != true
        && subtype.is_none()
        && !(synthetic && (assistant["is_api_error_message"] == true || category.is_some()))
    {
        return None;
    }
    let mut reason = String::new();
    if synthetic {
        reason = assistant
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    if reason.is_empty() {
        reason = result["result"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let errors = result["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                [subtype, (!errors.is_empty()).then_some(errors.as_str())]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": ")
            });
    }
    if reason.is_empty() {
        reason = if refused {
            "refusal"
        } else {
            "Claude turn failed"
        }
        .into();
    }
    let mut failure = TurnFailure::new(Class::Unknown, reason);
    failure.status =
        status(&result["api_error_status"]).or_else(|| status(&assistant["api_error_status"]));
    failure.class = if refused {
        Class::ModelRefused
    } else if result["terminal_reason"] == "prompt_too_long" {
        Class::InputTooLarge
    } else if matches!(subtype, Some("error_max_turns" | "error_max_budget_usd")) {
        Class::LimitReached
    } else if subtype == Some("error_max_structured_output_retries") {
        Class::OutputSchema
    } else if category == Some("authentication_failed") {
        failure.credential = match failure.status {
            None => Some(CredentialProblem::Missing),
            Some(401) => Some(CredentialProblem::Rejected),
            _ => None,
        };
        Class::Auth
    } else if assistant["api_error"] == "usage_limit_reached" {
        let info = &assistant["api_error_params"]["rate_limit_info"];
        failure.resets_at_ms = claude_reset(info);
        failure.rate_limit_type = info["rateLimitType"].as_str().map(str::to_owned);
        Class::Quota
    } else {
        match category {
            Some("rate_limit") if last_reset.is_some() => {
                failure.resets_at_ms = last_reset;
                Class::Quota
            }
            Some("verification_required") => Class::Auth,
            Some("overloaded") => Class::Overloaded,
            Some("model_not_found") => Class::ModelUnavailable,
            Some("rate_limit") => Class::RateLimited,
            Some("billing_error") => Class::Billing,
            Some("invalid_request") => invalid_request(&failure.reason),
            _ if failure.status == Some(400) => invalid_request(&failure.reason),
            _ => from_status(failure.status),
        }
    };
    Some(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_provider_codes_precede_http_status() {
        let failed =
            pi(&json!({"errorMessage": "429 {\"error\":{\"code\":\"insufficient_quota\"}}"}));
        assert_eq!(failed.class, Class::Quota);
        assert_eq!(failed.status, Some(429));
        let other = codex(
            &json!({"message": "Error running remote compact task: {\"error\":{\"code\":\"usage_limit_reached\",\"resets_at\":1900000000}}", "codexErrorInfo":"other"}),
        );
        assert_eq!(other.class, Class::Quota);
        assert_eq!(other.resets_at_ms, Some(1_900_000_000_000));
        let transport =
            codex(&json!({"codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":503}}}));
        assert_eq!(transport.class, Class::Overloaded);
        assert_eq!(transport.status, Some(503));
        let unknown = codex(&json!({"codexErrorInfo":"sessionBudgetExceeded","message":"429 401"}));
        assert_eq!(unknown.class, Class::Unknown);
        assert_eq!(unknown.status, None);
    }

    #[test]
    fn claude_only_attributes_native_quota_and_credential_facts() {
        assert_eq!(
            claude_reset(&json!({"status":"rejected","overageStatus":"allowed","resetsAt":123})),
            None
        );
        let reset = claude_reset(&json!({"status":"rejected","resetsAt":123}));
        let msg = json!({"message":{"model":"<synthetic>","content":[{"type":"text","text":"limit detail"}]},"error":"rate_limit"});
        let result = json!({"subtype":"success","is_error":true,"api_error_status":429});
        let quota = claude(&result, &msg, reset).unwrap();
        assert_eq!(quota.reason, "limit detail");
        assert_eq!(quota.resets_at_ms, Some(123_000));
        assert_eq!(quota.class, Class::Quota);
        assert_eq!(
            claude(&result, &msg, None).unwrap().class,
            Class::RateLimited
        );
        assert_eq!(
            claude(
                &json!({"subtype":"error_max_structured_output_retries"}),
                &Value::Null,
                None
            )
            .unwrap()
            .class,
            Class::OutputSchema
        );
        assert_eq!(
            claude(
                &json!({"subtype":"success","stop_reason":"refusal"}),
                &Value::Null,
                None
            )
            .unwrap()
            .class,
            Class::ModelRefused
        );
        let rejected = claude(
            &json!({"is_error":true,"api_error_status":401}),
            &json!({"error":"authentication_failed"}),
            None,
        )
        .unwrap();
        assert_eq!(rejected.credential, Some(CredentialProblem::Rejected));
        assert_eq!(
            claude(&json!({"subtype":"success"}), &Value::Null, reset),
            None
        );
    }
}
