use super::{
    AuthStatus, AuthStatusOptions, Reader,
    native::{self, BUDDY_ARGS, FACTORY_ARGS, Wire},
};
use crate::{Error, Result, Runtime, installation::Probe, process::OwnedProcess};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::AsyncReadExt,
    time::{Instant, timeout, timeout_at},
};

/// Environment and working directory shared by the native logout and its confirmation.
/// A supplied deadline includes discovery. Confirmation gets at least one second.
pub type LogoutOptions = AuthStatusOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogoutFailure {
    TimedOut,
    Rejected,
    ProcessFailed,
    StillLoggedIn,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogoutResult {
    LoggedOut,
    Failed {
        reason: LogoutFailure,
        detail: String,
    },
    Unsupported {
        reason: super::AccountUnsupported,
        detail: String,
    },
}

impl LogoutResult {
    fn failed(reason: LogoutFailure, detail: &str) -> Self {
        Self::Failed {
            reason,
            detail: detail.into(),
        }
    }
    fn unsupported(reason: super::AccountUnsupported, detail: &str) -> Self {
        Self::Unsupported {
            reason,
            detail: detail.into(),
        }
    }
    fn confirmed(self, reader: Reader, status: Option<AuthStatus>) -> Self {
        let still_in = || {
            Self::failed(
                LogoutFailure::StillLoggedIn,
                "The native account status still reports an active login",
            )
        };
        match status {
            Some(AuthStatus::LoggedOut { .. }) => Self::LoggedOut,
            Some(AuthStatus::LoggedIn { .. })
                if self == Self::LoggedOut || reader == Reader::Factory =>
            {
                still_in()
            }
            Some(AuthStatus::Unknown { configured, .. })
                if reader == Reader::Factory && !configured.is_empty() =>
            {
                Self::failed(
                    LogoutFailure::StillLoggedIn,
                    "Droid still uses FACTORY_API_KEY from the environment",
                )
            }
            _ => self,
        }
    }
}
impl From<super::Unavailable> for LogoutResult {
    fn from(issue: super::Unavailable) -> Self {
        match issue {
            super::Unavailable::Timeout => {
                Self::failed(LogoutFailure::TimedOut, "Executable discovery timed out")
            }
            super::Unavailable::Process => Self::failed(
                LogoutFailure::ProcessFailed,
                "Could not start a verified native executable",
            ),
            super::Unavailable::Unsupported(reason, detail) => Self::unsupported(reason, &detail),
        }
    }
}

impl Runtime {
    fn logout_reader(&self) -> Option<Reader> {
        Reader::from_id(&self.id).filter(|r| !matches!(r, Reader::Qwen | Reader::Qoder))
    }
    /// Whether this adapter has a noninteractive, profile-scoped logout.
    pub fn supports_logout(&self) -> bool {
        self.logout_reader().is_some()
    }

    /// Explicitly sign out of the selected native profile. This is never run during discovery.
    pub async fn logout(&self, options: &LogoutOptions) -> Result<LogoutResult> {
        let Some(reader) = self.logout_reader() else {
            return Err(Error::Unsupported(format!(
                "account logout for {}",
                self.id
            )));
        };
        let probe = Probe::new(&options.installation)?;
        let default = match reader {
            Reader::Factory => 30,
            Reader::Cursor => 20,
            _ => 60,
        };
        let deadline = super::deadline(options.timeout, default)?;
        let floor = match reader {
            Reader::Codex => Some((0, 15, 0)),
            Reader::Claude => Some((2, 1, 41)),
            Reader::Kimi => Some((2, 3, 3)),
            _ => None,
        };
        let program = match super::executable(self, &probe, floor, deadline).await {
            Ok(p) => p,
            Err(e) => return Ok(e.into()),
        };
        if Instant::now() >= deadline {
            return Ok(timed_out());
        }
        let mut cmd = native::command(reader, &probe, &program);
        let native = match reader {
            Reader::Kimi | Reader::Factory | Reader::Antigravity | Reader::Codebuddy => {
                let args: &[&str] = match reader {
                    Reader::Factory => FACTORY_ARGS,
                    Reader::Codebuddy => BUDDY_ARGS,
                    _ => &["acp"],
                };
                cmd.args(args);
                if reader == Reader::Antigravity
                    && let Some(dir) = probe.get("GEMINI_HOME").filter(|d| !d.is_empty())
                {
                    cmd.arg("--gemini-dir").arg(dir);
                }
                let Ok(mut wire) = Wire::open(cmd) else {
                    return Ok(not_started());
                };
                let result = timeout_at(deadline, wire_logout(&mut wire, reader)).await;
                wire.close().await;
                let result = match result {
                    Err(_) => timed_out(),
                    Ok(Err(Error::Refused(_))) => LogoutResult::failed(
                        LogoutFailure::Rejected,
                        "Runtime refused the logout request",
                    ),
                    Ok(Err(_)) => LogoutResult::failed(
                        LogoutFailure::ProcessFailed,
                        "The account protocol ended without a logout response",
                    ),
                    Ok(Ok(result)) => result,
                };
                // Droid's confirmation must read from the same ACP connection as its logout.
                if reader == Reader::Factory || matches!(result, LogoutResult::Unsupported { .. }) {
                    return Ok(result);
                }
                result
            }
            Reader::Codex | Reader::Claude | Reader::Cursor | Reader::Devin | Reader::Grok => {
                let args: &[&str] = if matches!(reader, Reader::Claude | Reader::Devin) {
                    &["auth", "logout"]
                } else {
                    &["logout"]
                };
                cmd.args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
                let Ok(mut child) = OwnedProcess::spawn(cmd) else {
                    return Ok(not_started());
                };
                let result = timeout_at(deadline, command_end(&mut child)).await;
                child.stop().await;
                match result {
                    Ok(Ok(out)) => out,
                    Ok(Err(_)) => LogoutResult::failed(
                        LogoutFailure::ProcessFailed,
                        "Could not read the native logout result",
                    ),
                    Err(_) => timed_out(),
                }
            }
            Reader::Qoder | Reader::Qwen => unreachable!(),
        };
        let verification = if options.timeout.is_some() {
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1))
        } else {
            reader.query_timeout()
        };
        Ok(native.confirmed(
            reader,
            confirmed_status(reader, &probe, &program, verification).await,
        ))
    }
}
fn timed_out() -> LogoutResult {
    LogoutResult::failed(LogoutFailure::TimedOut, "Native logout timed out")
}
fn not_started() -> LogoutResult {
    LogoutResult::failed(
        LogoutFailure::ProcessFailed,
        "Native logout could not start",
    )
}

async fn command_end(child: &mut OwnedProcess) -> std::io::Result<LogoutResult> {
    let mut pipe = child.inner().stderr.take().expect("piped stderr");
    let mut diagnostic = false;
    let mut buf = [0_u8; 4096];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        diagnostic |= buf[..n].iter().any(|c| !c.is_ascii_whitespace());
    }
    Ok(match child.wait().await?.code() {
        Some(0) => LogoutResult::LoggedOut,
        Some(1) if diagnostic => LogoutResult::failed(
            LogoutFailure::Rejected,
            "The native logout command reported a refusal",
        ),
        _ => LogoutResult::failed(
            LogoutFailure::ProcessFailed,
            "The native logout command did not complete successfully",
        ),
    })
}

async fn wire_logout(wire: &mut Wire, reader: Reader) -> Result<LogoutResult> {
    if reader == Reader::Codebuddy {
        wire.control(json!({"subtype":"logout"})).await?;
        return Ok(LogoutResult::LoggedOut);
    }
    let init = wire.initialize(Wire::capabilities()).await?;
    if init
        .pointer("/agentCapabilities/auth/logout")
        .is_none_or(|v| v.is_null() || v == false)
    {
        return Ok(if reader == Reader::Factory {
            LogoutResult::failed(
                LogoutFailure::ProcessFailed,
                "Droid did not declare ACP logout",
            )
        } else {
            LogoutResult::unsupported(
                super::AccountUnsupported::VersionUnsupported,
                "This build does not declare ACP logout",
            )
        });
    }
    let reply = wire.rpc(2, "logout", json!({})).await?;
    let native = if !reply["error"].is_null() {
        if reader != Reader::Factory && reply["error"]["code"] == -32601 {
            return Ok(LogoutResult::unsupported(
                super::AccountUnsupported::VersionUnsupported,
                "This build does not implement ACP logout",
            ));
        }
        LogoutResult::failed(
            if reader == Reader::Factory {
                LogoutFailure::Rejected
            } else {
                LogoutFailure::ProcessFailed
            },
            "The runtime refused ACP logout",
        )
    } else if reply.get("result").is_some() {
        LogoutResult::LoggedOut
    } else {
        LogoutResult::failed(
            LogoutFailure::ProcessFailed,
            "The runtime omitted its ACP logout result",
        )
    };
    if reader != Reader::Factory {
        return Ok(native);
    }
    let after = wire.rpc(3, "auth/status", json!({})).await.ok();
    let status = after
        .as_ref()
        .filter(|v| v["error"].is_null())
        .and_then(|v| v.get("result"))
        .map(super::projection::factory);
    Ok(native.confirmed(reader, status))
}

async fn confirmed_status(
    reader: Reader,
    probe: &Probe,
    program: &Path,
    duration: Duration,
) -> Option<AuthStatus> {
    timeout(duration, native::query(reader, probe, program))
        .await
        .ok()?
        .ok()
}
