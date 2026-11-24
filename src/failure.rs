use serde::{Deserialize, Serialize};
mod classify;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use classify::context_overflow;
pub(crate) use classify::prose;
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod durable;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Auth,
    Quota,
    RateLimited,
    Billing,
    ModelUnavailable,
    InputTooLarge,
    ModelRefused,
    InvalidRequest,
    Overloaded,
    Provider,
    RuntimeExited,
    LimitReached,
    OutputSchema,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialProblem {
    Missing,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryTiming {
    Now,
    Later,
    No,
}

/// Recovery timing only. A failed turn may already have changed the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureAdvice {
    pub retry: RetryTiming,
    pub user_action: bool,
}

impl FailureClass {
    pub fn advice(self) -> FailureAdvice {
        use FailureClass::*;
        let (retry, user_action) = match self {
            Auth | Billing | ModelUnavailable | InputTooLarge => (RetryTiming::No, true),
            RateLimited | Overloaded | Provider | RuntimeExited => (RetryTiming::Now, false),
            Quota => (RetryTiming::Later, false),
            InvalidRequest | ModelRefused | LimitReached | OutputSchema | Unknown => {
                (RetryTiming::No, false)
            }
        };
        FailureAdvice { retry, user_action }
    }
}

/// Native evidence for a failed turn. Optional facts are absent when the runtime did not report them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFailure {
    pub class: FailureClass,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialProblem>,
    /// Milliseconds since the Unix epoch, only for a reset attributed to this quota failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_type: Option<String>,
}

impl TurnFailure {
    pub(crate) fn new(class: FailureClass, reason: impl Into<String>) -> Self {
        Self {
            class,
            reason: reason.into(),
            status: None,
            credential: None,
            resets_at_ms: None,
            rate_limit_type: None,
        }
    }
}
