use super::{AuthStatus, Credential, LoginAccount, Metering, Reader};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub(super) fn field(value: &Value, key: &str) -> Option<String> {
    clean(value[key].as_str()?)
}
fn clean(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

pub(super) fn codex_account(v: &Value) -> LoginAccount {
    let method = field(v, "type");
    let (credential, metering) = match method.as_deref() {
        Some("chatgpt") => (Some(Credential::Account), Some(Metering::Plan)),
        Some("apiKey") => (Some(Credential::ApiKey), None),
        _ => (None, None),
    };
    LoginAccount {
        method,
        credential,
        metering,
        email: field(v, "email"),
        plan: field(v, "planType"),
        ..Default::default()
    }
}

pub(super) fn text(reader: Reader, exit: Option<i32>, out: &str, err: &str) -> AuthStatus {
    let reported = || out.lines().chain(err.lines());
    match reader {
        Reader::Codex if exit == Some(0) => {
            let mut a = LoginAccount::default();
            if reported().any(|s| s.contains("Logged in using ChatGPT")) {
                a.method = Some("chatgpt".into());
                a.credential = Some(Credential::Account);
                a.metering = Some(Metering::Plan);
            } else if reported().any(|s| {
                s.contains("Logged in using an API key") || s.starts_with("API key configured -")
            }) {
                a.method = Some("apiKey".into());
                a.credential = Some(Credential::ApiKey);
            }
            reader.logged_in(a)
        }
        Reader::Codex
            if exit == Some(1)
                && reported().any(|s| {
                    s.strip_prefix("Not logged in").is_some_and(|tail| {
                        !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                    })
                }) =>
        {
            reader.logged_out()
        }
        Reader::Claude | Reader::Qoder
            if exit == Some(0) || (reader == Reader::Claude && exit == Some(1)) =>
        {
            let body: Value = serde_json::from_str(out).unwrap_or_default();
            let key = if reader == Reader::Claude {
                "loggedIn"
            } else {
                "logged_in"
            };
            match body[key].as_bool() {
                Some(false) => reader.logged_out(),
                Some(true) if reader == Reader::Qoder => reader.logged_in(Default::default()),
                Some(true) => {
                    let method = field(&body, "authMethod");
                    let (credential, metering) = match method.as_deref() {
                        Some("claude.ai") => (Some(Credential::Account), Some(Metering::Plan)),
                        Some("api_key") => (Some(Credential::ApiKey), None),
                        _ => (None, None),
                    };
                    reader.logged_in(LoginAccount {
                        method,
                        email: field(&body, "email"),
                        plan: field(&body, "subscriptionType"),
                        credential,
                        metering,
                        ..Default::default()
                    })
                }
                None => reader.unknown("Native status lacks an explicit login flag"),
            }
        }
        Reader::Cursor if exit == Some(0) => {
            let v: Value = serde_json::from_str(out).unwrap_or_default();
            match (v["status"].as_str(), v["isAuthenticated"].as_bool()) {
                (Some("authenticated"), Some(true)) => {
                    let info = &v["userInfo"];
                    reader.logged_in(LoginAccount {
                        credential: Some(Credential::Account),
                        email: field(info, "email"),
                        account_id: field(info, "userId")
                            .or_else(|| info["userId"].as_u64().map(|n| n.to_string())),
                        ..Default::default()
                    })
                }
                (Some("unauthenticated"), Some(false)) => reader.logged_out(),
                (Some("partially-authenticated"), Some(false)) => reader.configured(
                    "A stored Cursor session is missing its refresh token",
                    vec!["stored access token".into()],
                ),
                _ => reader.unknown("Cursor did not report a complete authentication state"),
            }
        }
        Reader::Devin if exit == Some(0) => devin(out),
        Reader::Grok if exit == Some(0) => {
            if reported().any(|s| s.contains("You are not authenticated.")) {
                return reader.logged_out();
            }
            if let Some(method) = reported()
                .find_map(|s| s.strip_prefix("You are logged in with ")?.strip_suffix('.'))
                .filter(|m| !m.chars().any(char::is_whitespace))
                .and_then(clean)
            {
                return reader.logged_in(LoginAccount {
                    method: Some(method),
                    ..Default::default()
                });
            }
            reader.unknown("Grok did not report an authentication state")
        }
        Reader::Antigravity if exit == Some(0) => reader.logged_in(Default::default()),
        Reader::Antigravity
            if exit == Some(1)
                && reported().any(|s| s.to_ascii_lowercase().contains("please sign in")) =>
        {
            reader.logged_out()
        }
        _ => reader.unknown("The native account query did not report an authentication state"),
    }
}

fn devin(out: &str) -> AuthStatus {
    let reader = Reader::Devin;
    if out.lines().any(|s| s.starts_with("Not logged in.")) {
        return reader.logged_out();
    }
    if out
        .lines()
        .any(|s| s.starts_with("Failed to load credentials:"))
    {
        return reader.unknown("Could not read stored credentials");
    }
    static LOGIN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^Logged in(?: +\(via ([^)\r\n]+)\))?\.").unwrap());
    let Some(c) = LOGIN.captures(out) else {
        return reader.unknown("Devin did not report an authentication state");
    };
    let mut unverified = false;
    for line in out.lines().map(|l| l.trim_start().to_ascii_lowercase()) {
        if line.starts_with("failed to fetch from server:") {
            if line.contains("authentication required") || line.contains("invalid api key") {
                return reader.logged_out();
            }
            unverified = true;
        }
        unverified |= line.starts_with("unavailable:");
    }
    if unverified {
        return reader.configured(
            "Stored credentials could not be verified",
            vec!["stored credentials".into()],
        );
    }
    let method = c.get(1).and_then(|m| clean(m.as_str()));
    let mut a = LoginAccount {
        credential: match method.as_deref() {
            Some("Devin") => Some(Credential::Account),
            Some("API key") => Some(Credential::ApiKey),
            _ => None,
        },
        method,
        ..Default::default()
    };
    let mut section = "";
    for line in out.lines() {
        if !line.starts_with(' ') {
            section = line.trim_end();
            continue;
        }
        match section {
            "User:" if a.email.is_none() => {
                a.email = line.trim_start().strip_prefix("Email:").and_then(clean)
            }
            "Account:" if a.plan.is_none() => {
                a.plan = line.trim_start().strip_prefix("Plan:").and_then(clean)
            }
            _ => {}
        }
    }
    reader.logged_in(a)
}

pub(super) fn factory(body: &Value) -> AuthStatus {
    let r = Reader::Factory;
    match body["state"].as_str() {
        Some("unauthenticated") => r.logged_out(),
        Some("authenticated") => {
            let active = body["accounts"]
                .as_array()
                .and_then(|a| a.iter().find(|v| v["active"] == true));
            if active.is_some_and(|v| v["label"] == "Factory API key") {
                return r.configured(
                    "Droid reports this environment key without checking it",
                    vec!["FACTORY_API_KEY".into()],
                );
            }
            r.logged_in(LoginAccount {
                credential: Some(Credential::Account),
                email: active.and_then(|v| field(v, "email")),
                ..Default::default()
            })
        }
        _ => r.unknown("Droid did not report an authentication state"),
    }
}

pub(super) fn codebuddy(body: &Value) -> AuthStatus {
    let r = Reader::Codebuddy;
    match body.get("account") {
        Some(Value::Null) => r.logged_out(),
        Some(Value::Object(_)) => r.logged_in(Default::default()),
        _ => r.unknown("CodeBuddy did not report an account state"),
    }
}
