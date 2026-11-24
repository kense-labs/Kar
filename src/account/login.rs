use super::{
    AccountUnsupported, AuthStatus, LoginAccount, Reader, Unavailable,
    native::{self, Wire},
    profile,
};
use crate::{Error, InstallationOptions, Result, Runtime, installation::Probe};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, time::Duration};
use tokio::{
    sync::watch,
    time::{Instant, timeout_at},
};

mod command;
mod device;

/// A clone may be retained by the UI to cancel an outstanding sign-in.
#[derive(Clone)]
pub struct LoginCancellation(watch::Sender<bool>);
impl Default for LoginCancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}
impl LoginCancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub(crate) async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|s| *s).await;
    }
}

#[derive(Clone, Default)]
pub struct LoginOptions {
    pub installation: InstallationOptions,
    pub timeout: Option<Duration>,
    /// None preserves the vendor default. Some(false) delegates opening the URL to the host.
    pub open_browser: Option<bool>,
    pub kimi_region: Option<KimiRegion>,
    pub cancellation: LoginCancellation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KimiRegion {
    MainlandCn,
    Global,
}
impl KimiRegion {
    fn arg(self) -> &'static str {
        match self {
            Self::MainlandCn => "mainland-cn",
            Self::Global => "global",
        }
    }
}

/// Hand off pairing details directly to the user completing the sign-in.
/// These values are transient and should not be included in application logs.
#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum LoginEvent {
    AuthUrl {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    Info {
        message: String,
    },
}

impl LoginEvent {
    /// Write the event to the terminal selected by the person signing in.
    pub fn present(&self, mut out: impl std::io::Write) -> std::io::Result<()> {
        match self {
            Self::DeviceCode {
                user_code,
                verification_uri,
            } => writeln!(out, "Open {verification_uri} and enter {user_code}"),
            Self::AuthUrl { url, instructions } => {
                if let Some(text) = instructions {
                    writeln!(out, "{text}")?;
                }
                writeln!(out, "{url}")
            }
            Self::Info { message } => writeln!(out, "{message}"),
        }?;
        out.flush()
    }
}

pub trait LoginInteraction {
    fn on_event(&mut self, event: LoginEvent) -> Result<()>;
}
impl<F: FnMut(LoginEvent) -> Result<()>> LoginInteraction for F {
    fn on_event(&mut self, event: LoginEvent) -> Result<()> {
        self(event)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginFailure {
    TimedOut,
    Rejected,
    ProcessFailed,
    InteractionFailed,
    NotLoggedIn,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoginResult {
    LoggedIn {
        #[serde(skip_serializing_if = "Option::is_none")]
        account: Option<Box<LoginAccount>>,
    },
    Cancelled,
    Failed {
        reason: LoginFailure,
        detail: String,
    },
    Unsupported {
        reason: AccountUnsupported,
        detail: String,
    },
}
impl LoginResult {
    pub fn into_result(self) -> Result<()> {
        match self {
            Self::LoggedIn { .. } => Ok(()),
            Self::Cancelled => Err(Error::Refused("Sign-in cancelled".into())),
            Self::Unsupported { detail, .. } => Err(Error::Unsupported(detail)),
            Self::Failed {
                reason: LoginFailure::TimedOut,
                detail,
            } => Err(Error::Timeout(detail)),
            Self::Failed { detail, .. } => Err(Error::Refused(detail)),
        }
    }

    fn failed(reason: LoginFailure, detail: &str) -> Self {
        Self::Failed {
            reason,
            detail: detail.into(),
        }
    }
    fn success() -> Self {
        Self::LoggedIn { account: None }
    }
    fn timed_out() -> Self {
        Self::failed(LoginFailure::TimedOut, "Native sign-in timed out")
    }
}
impl From<Unavailable> for LoginResult {
    fn from(e: Unavailable) -> Self {
        match e {
            Unavailable::Timeout => Self::timed_out(),
            Unavailable::Process => Self::failed(
                LoginFailure::ProcessFailed,
                "Could not start a verified native executable",
            ),
            Unavailable::Unsupported(reason, detail) => Self::Unsupported { reason, detail },
        }
    }
}
impl From<Error> for LoginResult {
    fn from(e: Error) -> Self {
        match e {
            Error::Refused(_) => Self::failed(
                LoginFailure::Rejected,
                "The runtime refused the sign-in request",
            ),
            _ => Self::failed(
                LoginFailure::ProcessFailed,
                "The runtime ended without a valid sign-in response",
            ),
        }
    }
}
impl Runtime {
    /// Whether programmatic sign-in is exposed by this adapter.
    pub fn supports_login(&self) -> bool {
        matches!(
            self.id.as_str(),
            "codex" | "factory" | "qoder" | "grok" | "kimi" | "codebuddy" | "cursor"
        )
    }

    /// Run the vendor's sign-in flow without creating a chat.
    /// Dropping this future stops its owned native process.
    pub async fn login(
        &self,
        options: &LoginOptions,
        interaction: &mut (impl LoginInteraction + Send),
    ) -> Result<LoginResult> {
        if !self.supports_login() {
            return Err(Error::Unsupported(format!("hosted login for {}", self.id)));
        }
        if options.cancellation.is_cancelled() {
            return Ok(LoginResult::Cancelled);
        }
        let probe = Probe::new(&options.installation)?;
        let reader = Reader::from_id(&self.id).expect("supported reader");
        if options.kimi_region.is_some() && reader != Reader::Kimi {
            return Err(Error::Unsupported(
                "Login region applies only to Kimi".into(),
            ));
        }
        let (floor, duration, args): (_, _, &[&str]) = match reader {
            Reader::Codex => (
                Some((0, 118, 0)),
                16 * 60,
                &["app-server", "--listen", "stdio://"],
            ),
            Reader::Factory => (None, 15 * 60, native::FACTORY_ARGS),
            Reader::Cursor => (None, 15 * 60, &["login"]),
            Reader::Kimi => (Some((2, 3, 3)), 16 * 60, &["login"]),
            Reader::Qoder => (None, 5 * 60, &["login"]),
            Reader::Grok => (None, 15 * 60, &["login", "--device-auth"]),
            Reader::Codebuddy => (None, 5 * 60, native::BUDDY_ARGS),
            _ => unreachable!(),
        };
        let end = super::deadline(options.timeout, duration)?;
        let path = tokio::select! {biased;
            _=options.cancellation.cancelled()=>return Ok(LoginResult::Cancelled),
            found=super::executable(self,&probe,floor,end)=>match found{Ok(p)=>p,Err(e)=>return Ok(e.into())}
        };
        if options.cancellation.is_cancelled() {
            return Ok(LoginResult::Cancelled);
        }
        if Instant::now() >= end {
            return Ok(LoginResult::timed_out());
        }
        let mut cmd = native::command(reader, &probe, &path);
        cmd.args(args);
        if matches!(
            reader,
            Reader::Qoder | Reader::Kimi | Reader::Grok | Reader::Cursor
        ) {
            return Ok(command::login(reader, cmd, &probe, &path, options, interaction, end).await);
        }
        if reader == Reader::Codebuddy && options.open_browser == Some(false) {
            match command::inert_browser() {
                Some(p) => {
                    cmd.env("BROWSER", p);
                }
                None => return Ok(command::no_browser()),
            }
        }
        let mut wire = match Wire::open(cmd) {
            Ok(w) => w,
            Err(e) => return Ok(e.into()),
        };
        let mut reported_success = false;
        let authenticate = async {
            match reader {
                Reader::Codex => device::codex(&mut wire, interaction, &options.cancellation).await,
                Reader::Factory => {
                    device::factory(&mut wire, interaction, &options.cancellation).await
                }
                Reader::Codebuddy => {
                    device::codebuddy(
                        &mut wire,
                        interaction,
                        &options.cancellation,
                        &mut reported_success,
                    )
                    .await
                }
                _ => unreachable!(),
            }
        };
        let auth = bounded(authenticate, options, end).await;
        if reader == Reader::Codebuddy && (reported_success || matches!(auth, Ok(Ok(())))) {
            wire.close().await;
            return Ok(confirm_external(reader, &probe, &path, options, end).await);
        }
        let result = match auth {
            Ok(Ok(())) => {
                // Once the vendor has reported success, the credential is stored. A missing
                // confirmation (including a late cancellation) cannot undo that success.
                let confirmation_end = end.min(Instant::now() + reader.query_timeout());
                bounded(
                    confirm(reader, &mut wire, &probe),
                    options,
                    confirmation_end,
                )
                .await
                .unwrap_or_else(|_| LoginResult::success())
            }
            Ok(Err(result)) | Err(result) => result,
        };
        wire.close().await;
        Ok(result)
    }
}
async fn bounded<T>(
    f: impl Future<Output = T>,
    options: &LoginOptions,
    end: Instant,
) -> std::result::Result<T, LoginResult> {
    tokio::select! {biased;
        result=timeout_at(end,f)=>result.map_err(|_|LoginResult::timed_out()),
        _=options.cancellation.cancelled()=>Err(LoginResult::Cancelled),
    }
}
async fn confirm(reader: Reader, wire: &mut Wire, probe: &Probe) -> LoginResult {
    let mut status = match reader {
        Reader::Codex => {
            if wire
                .send(json!({"id":3,"method":"account/read","params":{}}))
                .await
                .is_err()
            {
                return LoginResult::success();
            }
            let Ok(reply) = device::account_reply(wire, 3).await else {
                return LoginResult::success();
            };
            let Some(a) = reply.get("account") else {
                return LoginResult::success();
            };
            if a.is_null() {
                reader.logged_out()
            } else if a.is_object() {
                reader.logged_in(super::projection::codex_account(a))
            } else {
                return LoginResult::success();
            }
        }
        Reader::Factory => {
            let Ok(f) = wire.rpc(3, "auth/status", json!({})).await else {
                return LoginResult::success();
            };
            if !f["error"].is_null() {
                return LoginResult::success();
            }
            super::projection::factory(&f["result"])
        }
        _ => unreachable!(),
    };
    if reader == Reader::Codex {
        profile::enrich(reader, probe, "", &mut status).await;
    }
    status.omit_secret_fields(
        probe,
        if reader == Reader::Codex {
            "codex"
        } else {
            "factory"
        },
    );
    confirmed(reader, status)
}
async fn confirm_external(
    reader: Reader,
    probe: &Probe,
    path: &std::path::Path,
    options: &LoginOptions,
    end: Instant,
) -> LoginResult {
    let end = end.min(Instant::now() + reader.query_timeout());
    match bounded(native::query(reader, probe, path), options, end).await {
        Ok(Ok(status)) => confirmed(reader, status),
        _ => LoginResult::success(),
    }
}
fn confirmed(reader: Reader, status: AuthStatus) -> LoginResult {
    match status {
        AuthStatus::LoggedIn { account, .. } => LoginResult::LoggedIn {
            account: account.map(Box::new),
        },
        AuthStatus::LoggedOut { .. } => LoginResult::failed(
            LoginFailure::NotLoggedIn,
            "The runtime completed sign-in but its account query reports logged out",
        ),
        AuthStatus::Unknown { configured, .. }
            if reader == Reader::Factory && !configured.is_empty() =>
        {
            LoginResult::failed(
                LoginFailure::NotLoggedIn,
                "Droid selected an environment key instead of the new sign-in",
            )
        }
        _ => LoginResult::success(),
    }
}
fn emit(
    host: &mut impl LoginInteraction,
    cancel: &LoginCancellation,
    event: LoginEvent,
) -> std::result::Result<(), LoginResult> {
    if cancel.is_cancelled() {
        return Err(LoginResult::Cancelled);
    }
    host.on_event(event).map_err(|_| {
        LoginResult::failed(
            LoginFailure::InteractionFailed,
            "The host could not present the sign-in instructions",
        )
    })?;
    if cancel.is_cancelled() {
        Err(LoginResult::Cancelled)
    } else {
        Ok(())
    }
}
fn link(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    let url = reqwest::Url::parse(text).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && !url.cannot_be_a_base()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| text.to_owned())
}
