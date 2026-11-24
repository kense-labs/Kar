//! Provider-specific credentials, separate from a vendor CLI's own account.
mod opencode;
pub(crate) use opencode::workspace_skills as opencode_workspace_skills;
pub(crate) use opencode::{fork as opencode_fork, recovery as opencode_recovery};
pub(crate) mod pi;
mod qwen;
pub use opencode::OpenCodeProviders;
pub use pi::{
    CatalogCancellation, CatalogModel, CatalogProvider, CatalogRefreshOptions,
    CatalogRefreshResult, PiLoginOptions, PiModelCatalog, PiModelRuntimePaths, PiModelsPath,
    PiProviders,
};
pub use qwen::{
    QwenConnect, QwenConnected, QwenProtocol, QwenProvider, QwenProviders, QwenScope, QwenWireApi,
};

use crate::account::LoginCancellation;
use crate::{Error, InstallationOptions, Result};
use serde::{Deserialize, Serialize};
use std::{future::Future, time::Duration};

#[derive(Clone)]
pub struct ProviderAuthOptions {
    pub installation: InstallationOptions,
    pub timeout: Duration,
    pub login_timeout: Duration,
    pub idle_timeout: Duration,
    /// Reject a browser-only redirect to the server machine before showing the URL.
    pub refuse_loopback: bool,
}
impl Default for ProviderAuthOptions {
    fn default() -> Self {
        Self {
            installation: Default::default(),
            timeout: Duration::from_secs(30),
            login_timeout: Duration::from_secs(600),
            idle_timeout: Duration::from_secs(5),
            refuse_loopback: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Oauth,
    ApiKey,
}
/// Actionable provider-plane failures. Diagnostics do not contain native response bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    UnsupportedInstallation,
    VersionUnsupported,
    StartupFailed,
    ServerFailed,
    AuthenticationFailed,
    TimedOut,
    UnknownProvider,
    MethodUnavailable,
    Rejected,
    Expired,
    LoopbackRequired,
    Cancelled,
    InteractionFailed,
}
#[derive(Debug, thiserror::Error, Serialize)]
#[error("{detail}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}
impl ProviderErrorKind {
    pub(crate) fn error(self, detail: &str) -> Error {
        ProviderError {
            kind: self,
            detail: detail.into(),
            status: None,
        }
        .into()
    }
    fn http(self, detail: &str, status: reqwest::StatusCode) -> Error {
        ProviderError {
            kind: self,
            detail: format!("{} (HTTP {})", detail, status.as_u16()),
            status: Some(status.as_u16()),
        }
        .into()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    pub provider_id: String,
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<Method>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginMethod {
    pub method: Method,
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ambient: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_label: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProvider {
    pub provider_id: String,
    pub name: String,
    pub methods: Vec<LoginMethod>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Prompt {
    Text {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    ManualCode {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<Choice>,
    },
}
/// Dropping the prompt future dismisses a pending request (including when OAuth completes elsewhere).
pub type ProviderEventSink = Box<dyn FnMut(ProviderEvent) -> Result<()> + Send>;
pub trait ProviderInteraction: Send {
    /// Optional independent event sink while a prompt borrows the host. Without
    /// one, progress notifications are delivered when the outstanding prompt settles.
    fn concurrent_events(&self) -> Option<ProviderEventSink> {
        None
    }
    fn event(&mut self, event: ProviderEvent) -> Result<()>;
    fn prompt(&mut self, prompt: Prompt) -> impl Future<Output = Result<String>> + Send;
}
#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProviderEvent {
    AuthUrl {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_seconds: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_in_seconds: Option<u64>,
    },
    Info {
        message: String,
    },
}
pub(crate) fn cancelled() -> Error {
    ProviderErrorKind::Cancelled.error("Provider sign-in cancelled")
}
async fn until<T>(deadline: tokio::time::Instant, f: impl Future<Output = Result<T>>) -> Result<T> {
    if tokio::time::Instant::now() >= deadline {
        return Err(ProviderErrorKind::TimedOut.error("Provider sign-in timed out"));
    }
    tokio::time::timeout_at(deadline, f)
        .await
        .map_err(|_| ProviderErrorKind::TimedOut.error("Provider sign-in timed out"))?
}
pub(crate) async fn within<T>(
    budget: Duration,
    name: &'static str,
    f: impl Future<Output = Result<T>>,
) -> Result<T> {
    if budget.is_zero() || tokio::time::Instant::now().checked_add(budget).is_none() {
        return Err(Error::Protocol(format!("Invalid timeout for {name}")));
    }
    tokio::time::timeout(budget, f)
        .await
        .map_err(|_| ProviderErrorKind::TimedOut.error(&format!("{name} timed out")))?
}
pub(crate) async fn interruptible<T>(
    cancel: &LoginCancellation,
    f: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {biased; _=cancel.cancelled()=>Err(cancelled()), v=f=>v}
}
