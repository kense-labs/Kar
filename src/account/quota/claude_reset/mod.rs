//! Explicit Claude reset inspection and redemption through its interactive terminal.
mod in_flight;
mod onboarding;
mod receipts;
mod screen;
mod types;
mod ui;
use super::{
    AccountUsageSnapshot, InstallationOptions, InstallationSnapshot, LimitResetResult, Probe,
    Reader, ReauthReason, Result, Runtime, UnsupportedReason, deadline, protocol,
};
use receipts::Receipts;
use screen::{Line, Shown};
use std::{path::Path, time::Duration};
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout_at},
};
pub use types::*;
use ui::Ui;
const READ_WINDOW: Duration = Duration::from_secs(20);
const SUBMIT_WINDOW: Duration = Duration::from_secs(50);
const CLAIM_WINDOW: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub(super) enum Operation<'a> {
    Inspect { include_five_hour: bool },
    Redeem(ResetKind, &'a str),
}
fn unsupported(reason: UnsupportedReason) -> LimitResetResult {
    LimitResetResult::Unsupported { reason }
}
fn refused(reason: NotAttemptedReason) -> LimitResetResult {
    LimitResetResult::NotAttempted {
        reason,
        startup: None,
    }
}
fn unknown() -> LimitResetResult {
    LimitResetResult::Unknown {
        five_hour_spent: false,
    }
}

async fn newer_program(probe: &Probe) -> bool {
    let Some(dir) = crate::account::profile::path(probe, "CLAUDE_CONFIG_DIR")
        .or_else(|| crate::account::profile_home(probe))
    else {
        return false;
    };
    tokio::task::spawn_blocking(move || {
        let raw =
            crate::account::read_profile(&dir.join(".claude.json"), 16 * 1024 * 1024).ok()??;
        let obj: serde_json::Value = serde_json::from_str(&raw).ok()?;
        obj["cachedGrowthBookFeatures"]["tengu_cedar_ember"]["enabled"].as_bool()
    })
    .await
    .ok()
    .flatten()
        == Some(true)
}
async fn pre_submit(
    ui: &Ui,
    probe: &Probe,
    path: &Path,
    read_usage: bool,
    window: Duration,
) -> Option<LimitResetResult> {
    if !ui.remains(window) {
        return Some(refused(NotAttemptedReason::Deadline));
    }
    if read_usage {
        let usage =
            tokio::time::timeout(Duration::from_secs(15), super::claude::read(probe, path)).await;
        let Ok(Ok(AccountUsageSnapshot::Available(account))) = usage else {
            return Some(refused(NotAttemptedReason::UsageUnread));
        };
        let mut windows = account
            .windows
            .iter()
            .filter(|w| w.id.as_deref() == Some("five_hour"));
        let first = windows.next();
        match (first, windows.next()) {
            (Some(w), None) if w.used_ratio >= 1.0 => {
                return Some(unsupported(UnsupportedReason::ConsentRequired));
            }
            (Some(_), None) => {}
            _ => return Some(refused(NotAttemptedReason::UsageUnread)),
        }
    }
    (!ui.remains(window)).then(|| refused(NotAttemptedReason::Deadline))
}
async fn redeem(
    ui: &mut Ui,
    db: &Receipts,
    probe: &Probe,
    path: &Path,
    consent: bool,
    kind: ResetKind,
    key: &str,
) -> Result<LimitResetResult> {
    if let Some(refusal) = pre_submit(
        ui,
        probe,
        path,
        !consent && kind != ResetKind::FiveHour,
        SUBMIT_WINDOW,
    )
    .await
    {
        return Ok(refusal);
    }
    let cmd = kind.command();
    if !ui.prepare(&cmd).await {
        return Ok(unknown());
    }
    if !ui.remains(SUBMIT_WINDOW) {
        return Ok(refused(NotAttemptedReason::Deadline));
    }
    // An interrupted process may have handed the next Enter to the native UI.
    db.save(
        key,
        kind,
        &LimitResetReport {
            result: unknown(),
            onboarding_completed: ui.onboarding_completed,
        },
    )?;
    if !ui.submit(&cmd) {
        db.forget(key)?;
        return Ok(unknown());
    }
    let result = match ui.result(&cmd, READ_WINDOW).await {
        Some(Shown::NoCommand) => unsupported(UnsupportedReason::EndpointUnavailable),
        Some(Shown::Result(Line::None)) => LimitResetResult::NoReset,
        Some(Shown::Result(Line::LegacyUnavailable)) if kind != ResetKind::FiveHour => {
            LimitResetResult::NoReset
        }
        Some(Shown::Result(Line::NeedsLimit | Line::LegacyUnavailable)) => {
            LimitResetResult::NothingToReset
        }
        Some(Shown::Result(Line::LegacySpent)) if kind == ResetKind::FiveHour => {
            LimitResetResult::Reset
        }
        Some(Shown::Result(Line::LegacySpent)) => LimitResetResult::Unknown {
            five_hour_spent: true,
        },
        Some(Shown::Confirm(d)) if d.kind == kind && ui.stable_dialog(kind).await => {
            if !ui.remains(CLAIM_WINDOW) {
                if ui.cancel(&cmd).await {
                    db.forget(key)?;
                    return Ok(refused(NotAttemptedReason::Deadline));
                }
                unknown()
            } else if ui.confirm(kind) && ui.claim_result(&cmd).await == Some(Line::Reset(kind)) {
                LimitResetResult::Reset
            } else {
                unknown()
            }
        }
        Some(Shown::Confirm(_) | Shown::OtherDialog) => {
            ui.cancel(&cmd).await;
            unknown()
        }
        _ => unknown(),
    };
    db.save(
        key,
        kind,
        &LimitResetReport {
            result: result.clone(),
            onboarding_completed: ui.onboarding_completed,
        },
    )?;
    Ok(result)
}
async fn inspect(
    ui: &mut Ui,
    probe: &Probe,
    path: &Path,
    consent: bool,
    five: bool,
) -> LimitResetResult {
    use ResetState::{Available, NotChecked, NotOffered, Unknown};
    let mut resets = ResetKind::ALL.map(|k| ResetKindStatus::new(k, NotChecked));
    let mut stopped = false;
    for (idx, kind) in [
        ResetKind::Weekly,
        ResetKind::Fable,
        ResetKind::Full,
        ResetKind::FiveHour,
    ]
    .into_iter()
    .enumerate()
    {
        if kind == ResetKind::FiveHour && !five {
            break;
        }
        let entry = resets.iter_mut().find(|r| r.id == kind).unwrap();
        if stopped {
            entry.state = Unknown;
            continue;
        }
        if let Some(result) = pre_submit(ui, probe, path, !consent && idx == 0, READ_WINDOW).await {
            if idx == 0 {
                return result;
            }
            break;
        }
        let command = kind.command();
        if !ui.prepare(&command).await || !ui.remains(READ_WINDOW) || !ui.submit(&command) {
            entry.state = Unknown;
            stopped = true;
            continue;
        }
        match ui.result(&command, READ_WINDOW).await {
            Some(Shown::NoCommand) => return unsupported(UnsupportedReason::EndpointUnavailable),
            Some(Shown::Result(line @ (Line::LegacySpent | Line::LegacyUnavailable))) => {
                return LimitResetResult::Inspected {
                    resets: ResetKind::ALL
                        .map(|k| {
                            ResetKindStatus::new(
                                k,
                                if k == ResetKind::FiveHour {
                                    Unknown
                                } else {
                                    NotOffered
                                },
                            )
                        })
                        .to_vec(),
                    version: ui.version.clone(),
                    program: Some(ResetProgram::Legacy),
                    five_hour_spent: line == Line::LegacySpent,
                };
            }
            Some(Shown::Result(Line::None)) => entry.state = ResetState::None,
            Some(Shown::Result(Line::NeedsLimit)) => entry.state = Available,
            Some(Shown::Confirm(d)) if d.kind == kind => {
                entry.state = Available;
                entry.resets_left = Some(d.left);
                entry.expires_text = Some(d.expiry);
                stopped = !ui.cancel(&command).await;
            }
            other => {
                entry.state = Unknown;
                stopped = true;
                if matches!(other, Some(Shown::Confirm(_) | Shown::OtherDialog)) {
                    ui.cancel(&command).await;
                }
            }
        }
    }
    LimitResetResult::Inspected {
        resets: resets.to_vec(),
        version: ui.version.clone(),
        program: None,
        five_hour_spent: false,
    }
}
pub(super) async fn run(
    runtime: &Runtime,
    options: &InstallationOptions,
    timeout: Option<Duration>,
    consent: bool,
    op: Operation<'_>,
) -> Result<LimitResetReport> {
    let end = deadline(timeout, 75)?;
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Ok(unsupported(UnsupportedReason::UnsupportedPlatform).into());
    }
    let probe = Probe::new(options)?;
    if let Operation::Redeem(kind, key) = op {
        let dir = Receipts::directory(&probe)?;
        loop {
            match in_flight::claim(dir.clone(), key, kind)? {
                in_flight::Claim::First(active) => {
                    let result = exclusive(runtime, &probe, end, consent, op).await;
                    active.finish(&result);
                    return result;
                }
                in_flight::Claim::Existing(rx) => {
                    if let Some(report) = in_flight::wait(rx, end).await? {
                        return Ok(report);
                    }
                }
            }
        }
    }
    exclusive(runtime, &probe, end, consent, op).await
}
async fn exclusive(
    runtime: &Runtime,
    probe: &Probe,
    end: Instant,
    consent: bool,
    op: Operation<'_>,
) -> Result<LimitResetReport> {
    static PLACES: Semaphore = Semaphore::const_new(3);
    static SCREEN: Semaphore = Semaphore::const_new(1);
    let Ok(_place) = PLACES.try_acquire() else {
        return Ok(refused(NotAttemptedReason::Busy).into());
    };
    let timeout_report = || LimitResetReport::from(refused(NotAttemptedReason::Deadline));
    let Ok(Ok(_screen)) = timeout_at(end, SCREEN.acquire()).await else {
        return Ok(timeout_report());
    };
    let Some(db) = Receipts::open(probe, end).await? else {
        return Ok(timeout_report());
    };
    if let Operation::Redeem(kind, key) = op {
        if let Some(previous) = db.find(key, kind)? {
            return Ok(previous);
        }
        if kind != ResetKind::FiveHour && !consent {
            match timeout_at(end, newer_program(probe)).await {
                Ok(true) => {}
                Ok(false) => return Ok(unsupported(UnsupportedReason::ConsentRequired).into()),
                Err(_) => return Ok(timeout_report()),
            }
        }
    }
    let min = match op {
        Operation::Inspect { .. } => READ_WINDOW,
        Operation::Redeem(..) => SUBMIT_WINDOW + Duration::from_secs(3),
    };
    if end.saturating_duration_since(Instant::now()) < min {
        return Ok(timeout_report());
    }
    let path = match timeout_at(end, probe.discover(runtime)).await {
        Ok(InstallationSnapshot::Available {
            command: Some(p), ..
        }) => p,
        Ok(InstallationSnapshot::Failed {
            code: crate::InstallationIssue::VersionUnsupported,
            ..
        }) => return Ok(unsupported(UnsupportedReason::VersionUnsupported).into()),
        Ok(_) => return Ok(unsupported(UnsupportedReason::UnsupportedInstallation).into()),
        Err(_) => return Ok(timeout_report()),
    };
    let mut ui = match Ui::open(probe, &path, end) {
        Ok(ui) => ui,
        Err(_) => {
            return Ok(LimitResetResult::NotAttempted {
                reason: NotAttemptedReason::StartupFailed,
                startup: Some(StartupDiagnostic {
                    screen: StartupScreen::Exited,
                    version: None,
                }),
            }
            .into());
        }
    };
    let result = match ui.startup().await {
        Err(result) => Ok(result),
        Ok(()) => match op {
            Operation::Inspect { include_five_hour } => {
                Ok(inspect(&mut ui, probe, &path, consent, include_five_hour).await)
            }
            Operation::Redeem(kind, key) => {
                redeem(&mut ui, &db, probe, &path, consent, kind, key).await
            }
        },
    };
    ui.close().await;
    Ok(LimitResetReport {
        result: result?,
        onboarding_completed: ui.onboarding_completed,
    })
}
