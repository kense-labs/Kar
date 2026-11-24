use crate::wire::failure::AcpFailures;
use crate::{
    FailureClass, NativeCause, OpenError, Protocol, Result, ResumeCwdReason, RuntimeFailureCode,
    Session, TurnFailure, UnavailableReason,
};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};

/// Qwen sends this root terminal result before replying to initialize. Similar
/// text in a model response or in a child result is not an account diagnosis.
pub(crate) fn qwen_missing_auth(result: &Value) -> Option<OpenError> {
    const MESSAGE: &str = "No auth type is selected. Please configure an auth type (e.g. via settings or `--auth-type`) before running in non-interactive mode.";
    if result["type"] != "result"
        || result["subtype"] != "error_during_execution"
        || result["is_error"] != true
        || result["num_turns"].as_u64() != Some(0)
        || !result["parent_tool_use_id"].is_null()
        || result.pointer("/error/message")?.as_str()? != MESSAGE
    {
        return None;
    }
    Some(OpenError::RuntimeFailure {
        failure: TurnFailure {
            credential: Some(crate::CredentialProblem::Missing),
            ..TurnFailure::new(FailureClass::Auth, MESSAGE)
        },
        code: None,
        cause: Some(NativeCause {
            method: "initialize".into(),
            native: json!({"type":"result", "subtype":"error_during_execution", "num_turns":0, "is_error":true, "error":{"message":MESSAGE}}),
        }),
    })
}

#[derive(Clone, Debug)]
pub(crate) enum OpenStep {
    Open,
    Model,
    Target(String),
    Codex {
        target: Option<String>,
        service_tier: Option<String>,
    },
}

impl OpenStep {
    pub(crate) fn error(
        &self,
        protocol: Protocol,
        acp: &AcpFailures,
        method: &str,
        native: &Value,
    ) -> Option<OpenError> {
        let code = native["code"].as_i64()?;
        let msg = native["message"]
            .as_str()
            .unwrap_or("runtime refused request");
        let cause = || NativeCause {
            method: method.into(),
            native: native.clone(),
        };
        let runtime = |class, code| OpenError::RuntimeFailure {
            failure: TurnFailure::new(class, msg),
            code,
            cause: Some(cause()),
        };
        if protocol == Protocol::Acp {
            if code == -32000 {
                return Some(runtime(FailureClass::Auth, None));
            }
            if matches!(self, Self::Model) && code == -32602 {
                return Some(runtime(FailureClass::ModelUnavailable, None));
            }
            if matches!(acp, AcpFailures::Devin) {
                let lower = msg.to_ascii_lowercase();
                if [
                    "authentication required",
                    "invalid api key",
                    "not logged in",
                ]
                .iter()
                .any(|s| lower.contains(s))
                {
                    return Some(runtime(FailureClass::Auth, None));
                }
            }
        }
        if protocol == Protocol::Codex
            && code == -32600
            && msg.starts_with("failed to load configuration: ")
        {
            return Some(runtime(
                FailureClass::InvalidRequest,
                Some(RuntimeFailureCode::RuntimeConfigInvalid),
            ));
        }
        if protocol == Protocol::Codex && code == -32600 {
            let old_settings = method == "thread/settings/update"
                && msg.starts_with("Invalid request: unknown variant `thread/settings/update`");
            let old_tier = matches!(self, Self::Codex { service_tier: Some(tier), .. } if matches!(tier.as_str(),"default" | "priority") &&
                msg.starts_with(&format!("Invalid request: unknown variant `{tier}`, expected `fast` or `flex`")));
            if old_settings || old_tier {
                return Some(runtime(
                    FailureClass::InvalidRequest,
                    Some(RuntimeFailureCode::RuntimeOutdated),
                ));
            }
        }
        let id = match self {
            Self::Target(id)
            | Self::Codex {
                target: Some(id), ..
            } => id,
            _ => return None,
        };
        let missing = match protocol {
            Protocol::Codex
                if code == -32600 && matches!(method, "thread/resume" | "thread/fork") =>
            {
                let reason = if msg
                    .strip_prefix("session ")
                    .and_then(|s| s.split_once(" is archived."))
                    .is_some_and(|(id, _)| !id.is_empty() && !id.contains(char::is_whitespace))
                {
                    Some(UnavailableReason::Archived)
                } else if msg
                    .strip_prefix("thread ")
                    .and_then(|s| s.strip_suffix(" already has an active writer"))
                    .is_some_and(|id| !id.is_empty() && !id.contains(char::is_whitespace))
                {
                    Some(UnavailableReason::InUse)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Some(OpenError::SessionUnavailable {
                        session_id: id.clone(),
                        reason,
                        message: msg.into(),
                        cause: cause(),
                    });
                }
                msg.starts_with("no rollout found for thread id")
            }
            Protocol::Factory => method == "droid.load_session" && code == -32004,
            Protocol::Acp
                if matches!(method, "session/resume" | "session/load" | "session/fork") =>
            {
                match acp {
                    AcpFailures::Cursor if method == "session/load" && code == -32602 => {
                        if let Some(detail) = native["data"]["message"].as_str()
                            && detail == format!("Session \"{id}\" not found")
                        {
                            return Some(OpenError::SessionNotFound {
                                session_id: id.clone(),
                                message: detail.into(),
                                cause: cause(),
                            });
                        }
                        false
                    }
                    AcpFailures::Grok { .. } => native["data"]["code"] == "FS_NOT_FOUND",
                    AcpFailures::Devin => {
                        code == -32016
                            && native["data"]["cognition.ai/errorKind"] == "session_not_found"
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        missing.then(|| OpenError::SessionNotFound {
            session_id: id.clone(),
            message: msg.into(),
            cause: cause(),
        })
    }
}

pub(crate) fn advertised(caps: &Value, name: &str) -> bool {
    caps[name].is_object() || caps[name] == true
}

pub(crate) fn same_directory(saved: &str, requested: &Path) -> bool {
    #[cfg(windows)]
    let saved = match saved.as_bytes() {
        [b'/', letter, b':', b'/', ..] if letter.is_ascii_alphabetic() => &saved[1..],
        _ => saved,
    };
    let saved = Path::new(saved);
    saved == requested || std::fs::canonicalize(saved).is_ok_and(|p| p == requested)
}

/// These ACP agents always run a loaded conversation in its original directory.
/// Read the unfiltered native list, including all cursor pages, before loading it.
pub(crate) async fn verify_acp_cwd(
    session: &Session,
    caps: &Value,
    id: &str,
    cwd: &Path,
    secrets: &[String],
) -> Result<()> {
    let refuse = |reason| OpenError::cwd(id, cwd, reason).redacted(secrets);
    if !advertised(caps, "list") {
        return Err(refuse(ResumeCwdReason::ListingUnavailable));
    }
    let mut params = json!({});
    let mut visited = HashSet::new();
    loop {
        let page = session
            .open_rpc("session/list", params, OpenStep::Open)
            .await?;
        let sessions = page["sessions"]
            .as_array()
            .filter(|rows| {
                rows.iter()
                    .all(|r| r["sessionId"].as_str().is_some_and(|s| !s.is_empty()))
            })
            .ok_or_else(|| refuse(ResumeCwdReason::ListingUnavailable))?;
        if let Some(entry) = sessions.iter().find(|r| r["sessionId"] == id) {
            let saved = entry["cwd"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| refuse(ResumeCwdReason::CwdUnreported))?;
            if !same_directory(saved, cwd) {
                return Err(refuse(ResumeCwdReason::OtherCwd {
                    saved_cwd: saved.into(),
                }));
            }
            return Ok(());
        }
        match &page["nextCursor"] {
            Value::Null => return Err(refuse(ResumeCwdReason::NotListed)),
            Value::String(s) if s.is_empty() => return Err(refuse(ResumeCwdReason::NotListed)),
            Value::String(s) if visited.insert(s.clone()) => params = json!({"cursor":s}),
            _ => return Err(refuse(ResumeCwdReason::ListingUnavailable)),
        }
    }
}

#[cfg(test)]
mod qwen_tests {
    use super::*;

    #[test]
    fn missing_auth_requires_the_exact_native_root_startup_result() {
        let message = "No auth type is selected. Please configure an auth type (e.g. via settings or `--auth-type`) before running in non-interactive mode.";
        let frame = json!({"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"error":{"message":message},"other":"unrelated private data"});
        let Some(OpenError::RuntimeFailure {
            cause: Some(cause), ..
        }) = qwen_missing_auth(&frame)
        else {
            panic!("expected a typed opening failure")
        };
        assert!(cause.native.get("other").is_none());
        for (k, v) in [
            ("type", json!("assistant")),
            ("subtype", json!("success")),
            ("parent_tool_use_id", json!("child")),
            ("num_turns", json!(1)),
            ("is_error", json!(false)),
            ("error", json!({"message":"unrelated auth failure"})),
        ] {
            let mut other = frame.clone();
            other[k] = v;
            assert!(qwen_missing_auth(&other).is_none(), "nonmatching field {k}");
        }
    }
}
