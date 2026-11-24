//! Account quota snapshots and user-initiated native limit resets.
mod antigravity;
mod billing;
mod claude;
mod claude_reset;
pub use claude_reset::{
    LimitResetInspectOptions, LimitResetReport, NotAttemptedReason, ResetKind, ResetKindStatus,
    ResetProgram, ResetState, StartupDiagnostic, StartupScreen,
};
mod codex;
mod factory;
mod grok;
mod kimi;
use super::{AccountSecrets, LastAccount, Reader, deadline, native};
use crate::{
    Error, InstallationIssue, InstallationOptions, InstallationSnapshot, Result, Runtime,
    installation::Probe,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::time::Instant;

#[derive(Clone)]
pub struct AccountUsageOptions {
    pub installation: InstallationOptions,
    pub timeout: Option<Duration>,
    /// Codex may skip the per-reset detail lookup when only the count is needed.
    pub limit_reset_details: bool,
}
impl Default for AccountUsageOptions {
    fn default() -> Self {
        Self {
            installation: Default::default(),
            timeout: None,
            limit_reset_details: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AccountUsageSnapshot {
    Available(AccountQuota),
    ReauthRequired {
        reason: ReauthReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_account: Option<LastAccount>,
    },
    Unavailable {
        reason: UnavailableReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_account: Option<LastAccount>,
    },
    Unsupported {
        reason: UnsupportedReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        installed_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        required_version: Option<String>,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReauthReason {
    NotAuthenticated,
    CredentialsMissing,
    CredentialsRejected,
    ScopeMissing,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    AccessTokenExpired,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedReason {
    VersionUnsupported,
    CapabilityUnavailable,
    UnsupportedInstallation,
    UnsupportedAuthMode,
    UnsupportedAuthStorage,
    AuthConfigurationUnavailable,
    EndpointUnavailable,
    QuotaUnavailable,
    UnsupportedPlatform,
    UnsupportedVersion,
    ResetIdRequired,
    ConsentRequired,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub rate_limited: bool,
    pub windows: Vec<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_resets: Option<LimitResets>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    pub label: String,
    pub used_ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<QuotaScope>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum QuotaScope {
    Account,
    Model { name: String },
    Models { name: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credits {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unlimited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitResets {
    pub available: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets: Option<Vec<LimitReset>>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitReset {
    pub id: String,
    pub status: ResetStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetStatus {
    Available,
    Redeeming,
    Redeemed,
    Unknown,
}

fn unsupported(reason: UnsupportedReason) -> AccountUsageSnapshot {
    AccountUsageSnapshot::Unsupported {
        reason,
        installed_version: None,
        required_version: None,
    }
}
fn signed_out(last_account: Option<LastAccount>) -> AccountUsageSnapshot {
    AccountUsageSnapshot::ReauthRequired {
        reason: ReauthReason::NotAuthenticated,
        last_account,
    }
}
fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn ratio(fraction: f64) -> f64 {
    if fraction >= 1.0 {
        1.0
    } else {
        (fraction.max(0.0) * 1e6).round().min(999_999.0) / 1e6
    }
}
fn instant(v: &Value) -> Option<String> {
    let t: jiff::Timestamp = v.as_str()?.parse().ok()?;
    Some(format!(
        "{:.3}",
        jiff::Timestamp::from_millisecond(t.as_millisecond()).ok()?
    ))
}
fn protocol(message: &str) -> Error {
    Error::Protocol(message.into())
}
async fn output(reader: Reader, probe: &Probe, path: &Path, args: &[&str]) -> Result<Output> {
    let mut cmd = native::command(reader, probe, path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(crate::process::OwnedProcess::spawn(cmd)?.output().await?)
}

impl AccountUsageSnapshot {
    fn omit_secret_fields(&mut self, secrets: &AccountSecrets<'_>) {
        match self {
            Self::ReauthRequired { last_account, .. } | Self::Unavailable { last_account, .. } => {
                secrets.last_account(last_account)
            }
            Self::Unsupported {
                installed_version, ..
            } => secrets.clear(installed_version),
            Self::Available(quota) => {
                for field in [&mut quota.email, &mut quota.plan, &mut quota.display_name] {
                    secrets.clear(field);
                }
                for window in &mut quota.windows {
                    if secrets.contains(&window.label) {
                        window.label = "Usage limit".to_owned();
                    }
                    secrets.clear(&mut window.id);
                    if matches!(&window.scope, Some(QuotaScope::Model {name} | QuotaScope::Models {name}) if secrets.contains(name))
                    {
                        window.scope = None;
                    }
                }
                if let Some(credits) = &mut quota.credits {
                    secrets.clear(&mut credits.balance);
                }
                if let Some(resets) = quota.limit_resets.as_mut().and_then(|r| r.resets.as_mut()) {
                    resets.retain(|r| !secrets.contains(&r.id));
                    for r in resets {
                        secrets.clear(&mut r.title);
                        secrets.clear(&mut r.description);
                    }
                }
            }
        }
    }
}

impl Runtime {
    /// Whether this runtime has an account quota reader. A query also checks the installation and login.
    pub fn supports_account_usage(&self) -> bool {
        matches!(
            self.id.as_str(),
            "codex" | "claude" | "grok" | "antigravity" | "factory" | "kimi"
        )
    }

    /// Read account-wide limits from the installed provider. No native conversation is required.
    pub async fn account_usage(
        &self,
        options: &AccountUsageOptions,
    ) -> Result<AccountUsageSnapshot> {
        if !self.supports_account_usage() {
            return Ok(unsupported(UnsupportedReason::CapabilityUnavailable));
        }
        let reader = Reader::from_id(&self.id).expect("registered quota reader");
        let probe = Probe::new(&options.installation)?;
        if matches!(reader, Reader::Factory | Reader::Kimi) {
            billing::check_http_environment(&probe)?;
        }
        let until = deadline(
            options.timeout,
            if reader == Reader::Codex { 8 } else { 30 },
        )?;
        let read = async {
            let (path, version) = match probe.discover(self).await {
                InstallationSnapshot::Available {
                    command: Some(path),
                    version,
                    ..
                } => (path, version),
                InstallationSnapshot::Failed {
                    code: InstallationIssue::VersionUnsupported,
                    installed_version,
                    required_version,
                    ..
                } => {
                    return Ok(AccountUsageSnapshot::Unsupported {
                        reason: UnsupportedReason::VersionUnsupported,
                        installed_version,
                        required_version,
                    });
                }
                _ => return Ok(unsupported(UnsupportedReason::UnsupportedInstallation)),
            };
            match reader {
                Reader::Kimi => kimi::read(&probe, &path, until).await,
                Reader::Factory => factory::read(&probe, version.as_deref()).await,
                Reader::Codex => codex::read(&probe, &path, options.limit_reset_details).await,
                Reader::Claude => claude::read(&probe, &path).await,
                Reader::Antigravity => antigravity::read(&probe, &path, until).await,
                Reader::Grok => grok::read(&probe, &path, until).await,
                _ => unreachable!(),
            }
        };
        let mut snapshot = tokio::time::timeout_at(until, read)
            .await
            .map_err(|_| Error::Timeout(format!("{} account usage", self.id)))??;
        snapshot.omit_secret_fields(&AccountSecrets::new(&probe, &self.id));
        Ok(snapshot)
    }
}

/// The host persists one key per user action and reuses it if its outcome is unknown.
#[derive(Clone)]
pub struct LimitResetOptions {
    /// Explicit consent to Claude’s legacy program spending its five-hour reset.
    pub five_hour_limit_consent: bool,
    pub idempotency_key: String,
    pub reset_id: Option<String>,
    pub installation: InstallationOptions,
    pub timeout: Option<Duration>,
}

impl LimitResetOptions {
    pub fn new(idempotency_key: impl Into<String>) -> Self {
        Self {
            five_hour_limit_consent: false,
            idempotency_key: idempotency_key.into(),
            reset_id: None,
            installation: Default::default(),
            timeout: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum LimitResetResult {
    Reset,
    NothingToReset,
    NoReset,
    AlreadyRedeemed,
    NotAttempted {
        reason: NotAttemptedReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        startup: Option<StartupDiagnostic>,
    },
    Unknown {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        five_hour_spent: bool,
    },
    Inspected {
        resets: Vec<ResetKindStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        program: Option<ResetProgram>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        five_hour_spent: bool,
    },
    ReauthRequired {
        reason: ReauthReason,
    },
    Unsupported {
        reason: UnsupportedReason,
    },
}

impl Runtime {
    /// Redeem one user-requested native reset.
    pub async fn redeem_limit_reset(
        &self,
        options: &LimitResetOptions,
    ) -> Result<LimitResetResult> {
        Ok(self.redeem_limit_reset_report(options).await?.result)
    }
    /// Includes interactive onboarding information when Claude completed it.
    pub async fn redeem_limit_reset_report(
        &self,
        options: &LimitResetOptions,
    ) -> Result<LimitResetReport> {
        if options.idempotency_key.is_empty()
            || options.reset_id.as_deref() == Some("") && self.id != "claude"
        {
            return Err(protocol("Limit reset identifiers must not be empty"));
        }
        if self.id == "claude" {
            let Some(kind) = options.reset_id.as_deref().and_then(ResetKind::parse) else {
                return Ok(LimitResetResult::Unsupported {
                    reason: UnsupportedReason::ResetIdRequired,
                }
                .into());
            };
            return claude_reset::run(
                self,
                &options.installation,
                options.timeout,
                options.five_hour_limit_consent,
                claude_reset::Operation::Redeem(kind, &options.idempotency_key),
            )
            .await;
        }
        Ok(self.codex_reset(options).await?.into())
    }
    /// Open the selected Claude profile's interactive reset command on the owner's explicit request.
    pub async fn inspect_limit_resets(
        &self,
        options: &LimitResetInspectOptions,
    ) -> Result<LimitResetReport> {
        if self.id != "claude" {
            return Ok(LimitResetResult::Unsupported {
                reason: UnsupportedReason::CapabilityUnavailable,
            }
            .into());
        }
        claude_reset::run(
            self,
            &options.installation,
            options.timeout,
            options.five_hour_limit_consent,
            claude_reset::Operation::Inspect {
                include_five_hour: options.include_five_hour,
            },
        )
        .await
    }
    async fn codex_reset(&self, options: &LimitResetOptions) -> Result<LimitResetResult> {
        if self.id != "codex" {
            return Ok(LimitResetResult::Unsupported {
                reason: UnsupportedReason::CapabilityUnavailable,
            });
        }
        let until = deadline(options.timeout, 20)?;
        let probe = Probe::new(&options.installation)?;
        let installation = tokio::time::timeout_at(until, probe.discover(self))
            .await
            .map_err(|_| Error::Timeout("Codex installation check before limit reset".into()))?;
        let path = match installation {
            InstallationSnapshot::Available {
                command: Some(path),
                ..
            } => path,
            InstallationSnapshot::Failed {
                code: InstallationIssue::VersionUnsupported,
                ..
            } => {
                return Ok(LimitResetResult::Unsupported {
                    reason: UnsupportedReason::VersionUnsupported,
                });
            }
            _ => {
                return Ok(LimitResetResult::Unsupported {
                    reason: UnsupportedReason::UnsupportedInstallation,
                });
            }
        };
        match tokio::time::timeout_at(until, codex::redeem(&probe, &path, options)).await {
            Ok(Ok(result)) => Ok(result),
            _ => Err(protocol(
                "Codex limit reset outcome is unknown. Retry with the same idempotency key.",
            )),
        }
    }
}
