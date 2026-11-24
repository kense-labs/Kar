//! Isolated vendor account operations. Account traffic stays outside session journals.
pub(crate) mod binding;
pub use binding::AccountAdmission;
mod antigravity;
mod login;
pub use login::{
    KimiRegion, LoginCancellation, LoginEvent, LoginFailure, LoginInteraction, LoginOptions,
    LoginResult,
};
mod logout;
mod native;
pub use logout::{LogoutFailure, LogoutOptions, LogoutResult};
pub(crate) use native::Wire as AccountWire;
mod profile;
pub(crate) use profile::{
    default_gemini_directory, home as profile_home, read_file as read_profile,
};
mod projection;
pub mod quota;
pub(crate) mod qwen;
pub use quota::{
    AccountUsageOptions, AccountUsageSnapshot, LimitResetInspectOptions, LimitResetOptions,
    LimitResetReport, LimitResetResult,
};

use crate::{
    InstallationIssue, InstallationOptions, InstallationSnapshot, Result, Runtime,
    installation::Probe,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Default, Clone)]
pub struct AuthStatusOptions {
    pub installation: InstallationOptions,
    /// Deadline for discovery, execution, and any local profile lookup together.
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AuthStatus {
    LoggedIn {
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        account: Option<LoginAccount>,
    },
    LoggedOut {
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_account: Option<LastAccount>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Unknown {
        source: String,
        detail: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        configured: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<InstallationIssue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        installed_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        required_version: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginAccount {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<Credential>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metering: Option<Metering>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastAccount {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    Account,
    ApiKey,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metering {
    Plan,
    Usage,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Reader {
    Cursor,
    Claude,
    Codex,
    Qoder,
    Grok,
    Devin,
    Antigravity,
    Kimi,
    Factory,
    Codebuddy,
    Qwen,
}
impl Reader {
    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "cursor" => Self::Cursor,
            "qoder" => Self::Qoder,
            "grok" => Self::Grok,
            "devin" => Self::Devin,
            "antigravity" => Self::Antigravity,
            "kimi" => Self::Kimi,
            "factory" => Self::Factory,
            "codebuddy" => Self::Codebuddy,
            "qwen" => Self::Qwen,
            _ => return None,
        })
    }
    fn source(self) -> &'static str {
        match self {
            Self::Claude => "claude auth status --json",
            Self::Codex => "codex login status",
            Self::Cursor => "agent status --format json",
            Self::Qoder => "qodercli status --output json",
            Self::Grok => "grok models",
            Self::Devin => "devin auth status",
            Self::Antigravity => "agy models",
            Self::Kimi => "kimi acp authenticate",
            Self::Factory => "droid exec --output-format acp: auth/status",
            Self::Codebuddy => "codebuddy stream-json initialize.account",
            Self::Qwen => "Qwen user/system credential configuration",
        }
    }
    fn logged_in(self, account: LoginAccount) -> AuthStatus {
        let account = (account != LoginAccount::default()).then_some(account);
        AuthStatus::LoggedIn {
            source: self.source().into(),
            account,
        }
    }
    fn logged_out(self) -> AuthStatus {
        AuthStatus::LoggedOut {
            source: self.source().into(),
            detail: None,
            last_account: None,
        }
    }
    fn unknown(self, detail: &str) -> AuthStatus {
        self.configured(detail, vec![])
    }
    fn configured(self, detail: &str, configured: Vec<String>) -> AuthStatus {
        AuthStatus::Unknown {
            source: self.source().into(),
            detail: detail.into(),
            configured,
            code: None,
            installed_version: None,
            required_version: None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountUnsupported {
    BrowserSuppressionUnavailable,
    UnsupportedInstallation,
    VersionUnsupported,
}

enum Unavailable {
    Timeout,
    Process,
    Unsupported(AccountUnsupported, String),
}

fn deadline(duration: Option<Duration>, default_secs: u64) -> Result<tokio::time::Instant> {
    tokio::time::Instant::now()
        .checked_add(duration.unwrap_or(Duration::from_secs(default_secs)))
        .ok_or_else(|| crate::Error::Protocol("Account timeout exceeds the supported range".into()))
}

async fn executable(
    rt: &Runtime,
    probe: &Probe,
    floor: Option<(u64, u64, u64)>,
    deadline: tokio::time::Instant,
) -> std::result::Result<std::path::PathBuf, Unavailable> {
    use InstallationSnapshot::*;
    let snapshot = tokio::time::timeout_at(deadline, probe.discover(rt))
        .await
        .map_err(|_| Unavailable::Timeout)?;
    match snapshot {
        Available {
            command: Some(path),
            version,
            ..
        } => {
            if let (Some(required), Some(installed)) = (floor, version.as_deref().and_then(release))
                && installed < required
            {
                let (a, b, c) = required;
                return Err(Unavailable::Unsupported(
                    AccountUnsupported::VersionUnsupported,
                    format!("{} {a}.{b}.{c} or later is required", rt.id),
                ));
            }
            Ok(path)
        }
        Failed {
            code: InstallationIssue::VersionUnsupported,
            ..
        } => Err(Unavailable::Unsupported(
            AccountUnsupported::VersionUnsupported,
            "This installed version is unsupported".into(),
        )),
        Available { .. } => Err(Unavailable::Unsupported(
            AccountUnsupported::UnsupportedInstallation,
            "Native account management requires an executable".into(),
        )),
        _ => Err(Unavailable::Process),
    }
}
pub(crate) fn release(raw: &str) -> Option<(u64, u64, u64)> {
    static VERSION: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\bv?(\d+)\.(\d+)\.(\d+)\b").unwrap());
    let c = VERSION.captures(raw)?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?))
}
impl Reader {
    fn query_timeout(self) -> Duration {
        Duration::from_secs(if matches!(self, Self::Codex | Self::Claude | Self::Kimi) {
            20
        } else {
            30
        })
    }
}
struct AccountSecrets<'a>(Vec<&'a str>);

impl<'a> AccountSecrets<'a> {
    fn new(probe: &'a Probe, runtime: &str) -> Self {
        Self(
            crate::credentials::for_runtime(runtime)
                .into_iter()
                .flat_map(|shape| &shape.env)
                .filter(|v| v.secret)
                .filter_map(|v| probe.get(&v.name)?.to_str())
                .filter(|v| !v.is_empty())
                .collect(),
        )
    }
    fn contains(&self, value: &str) -> bool {
        self.0
            .iter()
            .any(|s| value == *s || (s.len() >= 4 && value.contains(*s)))
    }
    fn clear(&self, field: &mut Option<String>) {
        if field.as_deref().is_some_and(|v| self.contains(v)) {
            *field = None;
        }
    }
    fn last_account(&self, last: &mut Option<LastAccount>) {
        if let Some(a) = last {
            self.clear(&mut a.email);
            self.clear(&mut a.display_name);
            if a.email.is_none() && a.display_name.is_none() {
                *last = None;
            }
        }
    }
}

impl AuthStatus {
    // Optional account labels may come from vendor-owned configuration or a custom endpoint.
    fn omit_secret_fields(&mut self, probe: &Probe, runtime: &str) {
        let secrets = AccountSecrets::new(probe, runtime);
        match self {
            Self::LoggedIn {
                account: Some(a), ..
            } => {
                for field in [
                    &mut a.method,
                    &mut a.email,
                    &mut a.account_id,
                    &mut a.plan,
                    &mut a.expires_at,
                ] {
                    secrets.clear(field);
                }
            }
            Self::LoggedOut { last_account, .. } => {
                secrets.last_account(last_account);
            }
            _ => {}
        }
    }
}
impl Runtime {
    /// Whether this adapter declares a native account status reader.
    pub fn has_auth_status(&self) -> bool {
        Reader::from_id(&self.id).is_some()
    }

    /// Kar's Qwen reader inspects only user/system credential metadata.
    /// Cursor's native CLI status is not the upstream SDK's local file reader.
    pub fn auth_status_local_only(&self) -> bool {
        Reader::from_id(&self.id) == Some(Reader::Qwen)
    }

    /// Qwen only inspects local configuration. Other providers may contact their auth service.
    pub async fn auth_status(&self, options: &AuthStatusOptions) -> Result<AuthStatus> {
        let reader = Reader::from_id(&self.id)
            .ok_or_else(|| crate::Error::Unsupported(format!("account status for {}", self.id)))?;
        let probe = Probe::new(&options.installation)?;
        let timeout = options.timeout.unwrap_or_else(|| reader.query_timeout());
        let until = deadline(Some(timeout), 30)?;
        let read = async {
            if reader == Reader::Qwen {
                return tokio::task::spawn_blocking(move || qwen::inspect(&probe))
                    .await
                    .ok()
                    .and_then(std::result::Result::ok)
                    .unwrap_or_else(|| {
                        reader.unknown("Could not inspect user/system credential configuration")
                    });
            }
            let command = match probe.discover(self).await {
                InstallationSnapshot::Available {
                    command: Some(program),
                    ..
                } => program,
                InstallationSnapshot::Failed {
                    code: InstallationIssue::VersionUnsupported,
                    installed_version,
                    required_version,
                    ..
                } => {
                    return AuthStatus::Unknown {
                        source: reader.source().into(),
                        detail: "Unsupported runtime version".into(),
                        configured: vec![],
                        code: Some(InstallationIssue::VersionUnsupported),
                        installed_version,
                        required_version,
                    };
                }
                _ => {
                    return reader
                        .unknown("Native executable is missing or failed its readiness check");
                }
            };
            let mut status = native::query(reader, &probe, &command)
                .await
                .unwrap_or_else(|_| {
                    reader.unknown("The native account query ended without an answer")
                });
            if reader == Reader::Antigravity
                && let AuthStatus::LoggedIn { account, .. } = &mut status
                && let Some(plan) = antigravity::plan(&probe, &command, until).await
            {
                account.get_or_insert_with(Default::default).plan = Some(plan);
            }
            status.omit_secret_fields(&probe, &self.id);
            status
        };
        Ok(tokio::time::timeout_at(until, read)
            .await
            .unwrap_or_else(|_| reader.unknown("Account query timed out")))
    }

    pub fn credentials(&self) -> Option<&'static crate::credentials::RuntimeCredentials> {
        crate::credentials::for_runtime(&self.id)
    }
}

pub(crate) async fn devin_auth_status(probe: &Probe, path: &std::path::Path) -> Result<AuthStatus> {
    native::query(Reader::Devin, probe, path).await
}

pub(crate) async fn antigravity_auth_status(
    probe: &Probe,
    path: &std::path::Path,
) -> Result<AuthStatus> {
    native::query(Reader::Antigravity, probe, path).await
}
