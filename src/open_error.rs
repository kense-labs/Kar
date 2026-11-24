use crate::{Error, TurnFailure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeCause {
    pub method: String,
    pub native: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Archived,
    InUse,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ResumeCwdReason {
    #[error("the native session list omitted the requested ID")]
    NotListed,
    #[error("the runtime could not supply a valid session list")]
    ListingUnavailable,
    #[error("the runtime did not report the saved directory")]
    CwdUnreported,
    #[error("the native session belongs to {saved_cwd}")]
    OtherCwd { saved_cwd: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailureCode {
    RuntimeConfigInvalid,
    RuntimeOutdated,
}

/// Evidence established before a turn exists. A listing omission is scoped to
/// the directory check, while a missing target needs the vendor's own signal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpenError {
    #[error("{message}")]
    ModelConfirmation {
        message: String,
        requested: String,
        reported: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catalog_selector_ids: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<NativeCause>,
    },
    #[error("{message}")]
    SessionNotFound {
        session_id: String,
        message: String,
        cause: NativeCause,
    },
    #[error("{message}")]
    SessionUnavailable {
        session_id: String,
        reason: UnavailableReason,
        message: String,
        cause: NativeCause,
    },
    #[error("cannot confirm workspace {requested_cwd} for session {session_id}: {reason}")]
    ResumeCwd {
        session_id: String,
        requested_cwd: String,
        #[serde(flatten)]
        reason: ResumeCwdReason,
    },
    #[error("{}", failure.reason)]
    RuntimeFailure {
        #[serde(flatten)]
        failure: TurnFailure,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<RuntimeFailureCode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<NativeCause>,
    },
}

impl From<OpenError> for Error {
    fn from(error: OpenError) -> Self {
        Self::Open(Box::new(error))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl OpenError {
    pub(crate) fn model_confirmation(
        message: String,
        requested: &str,
        reported: Option<&str>,
    ) -> Self {
        Self::ModelConfirmation {
            message,
            requested: requested.into(),
            reported: reported.map(str::to_owned),
            catalog_selector_ids: None,
            cause: None,
        }
    }
    pub(crate) fn cwd(id: &str, cwd: &std::path::Path, reason: ResumeCwdReason) -> Self {
        Self::ResumeCwd {
            session_id: id.into(),
            requested_cwd: cwd.to_string_lossy().into_owned(),
            reason,
        }
    }

    pub(crate) fn redacted(mut self, secrets: &[String]) -> Error {
        let clean = |s: &mut String| {
            for secret in secrets.iter().filter(|s| !s.is_empty()) {
                if s.contains(secret) {
                    *s = s.replace(secret, "[redacted]");
                }
            }
        };
        match &mut self {
            Self::ModelConfirmation {
                message,
                requested,
                reported,
                catalog_selector_ids,
                cause,
            } => {
                clean(message);
                clean(requested);
                if let Some(model) = reported {
                    clean(model);
                }
                if let Some(ids) = catalog_selector_ids {
                    ids.iter_mut().for_each(clean);
                }
                if let Some(cause) = cause {
                    crate::transport::redact_native_error(&mut cause.native, secrets);
                }
            }
            Self::SessionNotFound {
                session_id,
                message,
                cause,
            }
            | Self::SessionUnavailable {
                session_id,
                message,
                cause,
                ..
            } => {
                clean(session_id);
                clean(message);
                crate::transport::redact_native_error(&mut cause.native, secrets);
            }
            Self::RuntimeFailure { failure, cause, .. } => {
                clean(&mut failure.reason);
                if let Some(cause) = cause {
                    crate::transport::redact_native_error(&mut cause.native, secrets);
                }
            }
            Self::ResumeCwd {
                session_id,
                requested_cwd,
                reason,
            } => {
                clean(session_id);
                clean(requested_cwd);
                if let ResumeCwdReason::OtherCwd { saved_cwd } = reason {
                    clean(saved_cwd);
                }
            }
        }
        self.into()
    }
}
