//! Live SDK contract-violation detector (`Finding` & `ContractAuditor`) and QA operations (`HarborOp`).

use super::budget::session_reported_tokens;
use crate::{
    Capabilities, CompactionOptions, ContextUsage, Event, ObservedEvent, Outcome, Session,
    SessionCapabilityDeclaration, SessionUsage, TurnFailure, observe::AgentStatus,
    stream::timestamp_ms,
};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, fs, path::Path, time::Duration};

/// Default grace period for an accepted `Session::abort()` to produce a root `TurnEnded` event.
pub const DEFAULT_ABORT_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Classified SDK contract or lifecycle violation recorded by Harbor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    TurnFailed,
    Stalled,
    DeclarationMismatch,
    ContextUsageMissing,
    UsageDecreased,
    AbortWithoutTurnEnd,
    AbortRejectedWhileRunning,
    UnexpectedExit,
    LeftoverProcesses,
    InputDropped,
    ResumeLoop,
    ResumeFailed,
    OpenFailed,
    GaveUp,
    DisposeFailed,
    NoReply,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnFailed => "turn_failed",
            Self::Stalled => "stalled",
            Self::DeclarationMismatch => "declaration_mismatch",
            Self::ContextUsageMissing => "context_usage_missing",
            Self::UsageDecreased => "usage_decreased",
            Self::AbortWithoutTurnEnd => "abort_without_turn_end",
            Self::AbortRejectedWhileRunning => "abort_rejected_while_running",
            Self::UnexpectedExit => "unexpected_exit",
            Self::LeftoverProcesses => "leftover_processes",
            Self::InputDropped => "input_dropped",
            Self::ResumeLoop => "resume_loop",
            Self::ResumeFailed => "resume_failed",
            Self::OpenFailed => "open_failed",
            Self::GaveUp => "gave_up",
            Self::DisposeFailed => "dispose_failed",
            Self::NoReply => "no_reply",
        }
    }
}

/// Structured contract-audit finding emitted by [`ContractAuditor`] or [`super::daemon::HarborAgent`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub check: FindingKind,
    pub agent: String,
    pub session: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voyage: Option<String>,
    pub detail: Value,
}

impl Finding {
    pub fn to_journal_payload(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("check".into(), Value::String(self.check.as_str().into()));
        obj.insert("session".into(), json!(self.session));
        if let Some(v) = &self.voyage {
            obj.insert("voyage".into(), Value::String(v.clone()));
        }
        if let Value::Object(extra) = &self.detail {
            for (k, val) in extra {
                obj.insert(k.clone(), val.clone());
            }
        } else if !self.detail.is_null() {
            obj.insert("detail".into(), self.detail.clone());
        }
        Value::Object(obj)
    }
}

/// Interactive QA control operations executable via `kar harbor op <agent> <op> [arg]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum HarborOp {
    Abort,
    Steer,
    Compact,
    StopTask,
    Usage,
}

impl HarborOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Steer => "steer",
            Self::Compact => "compact",
            Self::StopTask => "stop-task",
            Self::Usage => "usage",
        }
    }
}

/// Stateful per-agent auditor verifying Kar's SDK capability, usage, abort, stall, and process contracts.
#[derive(Debug)]
pub struct ContractAuditor {
    agent: String,
    declared: SessionCapabilityDeclaration,
    last_tokens: u64,
    compacted_this_turn: bool,
    reported_stalls: HashSet<(u64, u64)>,
    quick_exits: u32,
    voyage: Option<String>,
}

impl ContractAuditor {
    pub fn new(agent: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            declared: SessionCapabilityDeclaration::default(),
            last_tokens: 0,
            compacted_this_turn: false,
            reported_stalls: HashSet::new(),
            quick_exits: 0,
            voyage: None,
        }
    }

    pub fn set_voyage(&mut self, voyage: Option<String>) {
        self.voyage = voyage;
    }

    pub fn make_finding(&self, session: u64, check: FindingKind, detail: Value) -> Finding {
        Finding {
            check,
            agent: self.agent.clone(),
            session,
            voyage: self.voyage.clone(),
            detail,
        }
    }

    /// Audit an opened session's live [`Capabilities`] against its pre-open [`SessionCapabilityDeclaration`].
    pub fn audit_opened(
        &mut self,
        session: u64,
        capabilities: &Capabilities,
        declared: SessionCapabilityDeclaration,
    ) -> Vec<Finding> {
        let checks = [
            ("steering", declared.steering, capabilities.steering),
            ("stop_task", declared.stop_task, capabilities.stop_task),
            (
                "image_input",
                declared.image_input,
                capabilities.image_input,
            ),
            (
                "manual_compaction",
                declared.manual_compaction.map(|m| m.is_supported()),
                capabilities.manual_compaction,
            ),
        ];
        self.declared = declared;
        self.last_tokens = 0;
        self.compacted_this_turn = false;

        let mut findings = Vec::new();
        for (field, expected, actual) in checks {
            if let Some(expected) = expected
                && expected != actual
            {
                findings.push(self.make_finding(
                    session,
                    FindingKind::DeclarationMismatch,
                    json!({
                        "field": field,
                        "declared": expected,
                        "opened": actual,
                    }),
                ));
            }
        }
        findings
    }

    /// Audit a root-agent [`ObservedEvent`] for monotonic token usage, promised context occupancy, and turn failures.
    pub fn audit_event(
        &mut self,
        session: u64,
        observed: &ObservedEvent,
        usage: &SessionUsage,
        context_usage: Option<&ContextUsage>,
        last_failure: Option<&TurnFailure>,
    ) -> Vec<Finding> {
        if !observed.agent_path.is_empty() {
            return Vec::new();
        }
        let mut findings = Vec::new();
        match &observed.event {
            Event::Compaction { state } if state != "started" => {
                self.compacted_this_turn = true;
            }
            Event::ManualCompaction { .. } => {
                self.compacted_this_turn = true;
            }
            Event::TurnEnded { outcome } => {
                let seq = observed.cursor.seq;
                let tokens = session_reported_tokens(usage);
                if tokens < self.last_tokens {
                    findings.push(self.make_finding(
                        session,
                        FindingKind::UsageDecreased,
                        json!({
                            "seq": seq,
                            "before": self.last_tokens,
                            "after": tokens,
                        }),
                    ));
                }
                self.last_tokens = self.last_tokens.max(tokens);

                match outcome {
                    Outcome::Failed => {
                        let reason = last_failure
                            .map(|f| f.reason.clone())
                            .unwrap_or_else(|| "turn failed".into());
                        let class = last_failure.map(|f| f.class);
                        findings.push(self.make_finding(
                            session,
                            FindingKind::TurnFailed,
                            json!({
                                "seq": seq,
                                "reason": reason,
                                "failure_class": class,
                            }),
                        ));
                    }
                    Outcome::Completed => {
                        if let Some(promised) = self.declared.context_usage {
                            if promised.tokens
                                && !self.compacted_this_turn
                                && context_usage.and_then(|c| c.tokens).is_none()
                            {
                                findings.push(self.make_finding(
                                    session,
                                    FindingKind::ContextUsageMissing,
                                    json!({
                                        "seq": seq,
                                        "field": "tokens",
                                    }),
                                ));
                            }
                            if promised.context_window
                                && context_usage.and_then(|c| c.context_window).is_none()
                            {
                                findings.push(self.make_finding(
                                    session,
                                    FindingKind::ContextUsageMissing,
                                    json!({
                                        "seq": seq,
                                        "field": "context_window",
                                    }),
                                ));
                            }
                        }
                    }
                    Outcome::Aborted => {}
                }
                self.compacted_this_turn = false;
            }
            _ => {}
        }
        findings
    }

    /// Periodic watchdog check reporting a running turn silent at or past `stall_threshold` once per `(session, since_seq)`.
    pub fn audit_stall(
        &mut self,
        session: u64,
        status: &AgentStatus,
        now_ms: u64,
        stall_threshold: Duration,
    ) -> Option<Finding> {
        let AgentStatus::Running {
            since_seq, phase, ..
        } = status
        else {
            return None;
        };
        let silent_ms = status.silent_for(now_ms, stall_threshold)?;
        if !self.reported_stalls.insert((session, *since_seq)) {
            return None;
        }
        Some(self.make_finding(
            session,
            FindingKind::Stalled,
            json!({
                "since_seq": since_seq,
                "silent_ms": silent_ms,
                "phase": phase,
            }),
        ))
    }

    /// Record an unrequested `Event::Exited`, returning `(findings, force_fresh_reopen)`.
    pub fn audit_unexpected_exit(
        &mut self,
        session: u64,
        seq: u64,
        reason: &str,
        opened_at_ms: u64,
        now_ms: u64,
    ) -> (Vec<Finding>, bool) {
        let mut findings = vec![self.make_finding(
            session,
            FindingKind::UnexpectedExit,
            json!({
                "seq": seq,
                "reason": reason,
            }),
        )];
        if now_ms.saturating_sub(opened_at_ms) < 30_000 {
            self.quick_exits += 1;
        } else {
            self.quick_exits = 0;
        }
        let force_fresh = self.quick_exits >= 2;
        if force_fresh {
            findings.push(self.make_finding(
                session,
                FindingKind::ResumeLoop,
                json!({
                    "exits": self.quick_exits,
                    "note": "session exited within 30s of opening twice; reopening fresh",
                }),
            ));
            self.quick_exits = 0;
        }
        (findings, force_fresh)
    }

    /// Check whether any foreign OS processes remain rooted inside `cwd` after `Session::close()`.
    pub fn audit_leftover_processes(&self, session: u64, cwd: &Path) -> Option<Finding> {
        self.audit_leftover_pids(session, processes_in_cwd(cwd))
    }

    pub fn audit_leftover_pids(&self, session: u64, pids: Vec<u32>) -> Option<Finding> {
        if pids.is_empty() {
            None
        } else {
            Some(self.make_finding(
                session,
                FindingKind::LeftoverProcesses,
                json!({ "pids": pids }),
            ))
        }
    }

    /// Execute a QA [`HarborOp`] against `session` and verify SDK abort/settle promises.
    pub async fn execute_op(
        &mut self,
        session_num: u64,
        session: &Session,
        op: HarborOp,
        arg: Option<&str>,
        abort_settle_timeout: Duration,
    ) -> (Value, Vec<Finding>) {
        let started_ms = timestamp_ms();
        let before_running = session.status().is_running();
        let mut findings = Vec::new();

        let body = match op {
            HarborOp::Abort => {
                let pre_snap = session.snapshot();
                let mut sub = session.events(None).ok();
                // Drain events already present before issuing abort so we observe the new TurnEnded.
                let pre_seq = pre_snap.cursor.map(|c| c.after_seq);
                match session.abort().await {
                    Ok(()) => {
                        let settled = if let Some(ref mut events) = sub {
                            tokio::time::timeout(abort_settle_timeout, async {
                                while let Some(ev) = events.next().await {
                                    if pre_seq.is_some_and(|cut| ev.cursor.seq <= cut) {
                                        continue;
                                    }
                                    if ev.agent_path.is_empty()
                                        && matches!(ev.event, Event::TurnEnded { .. })
                                    {
                                        return true;
                                    }
                                }
                                !session.status().is_running()
                            })
                            .await
                            .unwrap_or(false)
                        } else {
                            !session.status().is_running()
                        };
                        if !settled {
                            findings.push(self.make_finding(
                                session_num,
                                FindingKind::AbortWithoutTurnEnd,
                                json!({
                                    "waited_ms": abort_settle_timeout.as_millis() as u64,
                                }),
                            ));
                        }
                        json!({
                            "op": "abort",
                            "before_running": before_running,
                            "accepted": true,
                            "turn_ended": settled,
                            "ms": timestamp_ms().saturating_sub(started_ms),
                        })
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        if before_running {
                            findings.push(self.make_finding(
                                session_num,
                                FindingKind::AbortRejectedWhileRunning,
                                json!({ "error": msg }),
                            ));
                        }
                        json!({
                            "op": "abort",
                            "before_running": before_running,
                            "accepted": false,
                            "error": msg,
                            "ms": timestamp_ms().saturating_sub(started_ms),
                        })
                    }
                }
            }
            HarborOp::Steer => {
                let text = arg.unwrap_or("(harbor steer)");
                match session.steer(text).await {
                    Ok(()) => json!({
                        "op": "steer",
                        "accepted": true,
                        "ms": timestamp_ms().saturating_sub(started_ms),
                    }),
                    Err(err) => json!({
                        "op": "steer",
                        "accepted": false,
                        "error": err.to_string(),
                        "ms": timestamp_ms().saturating_sub(started_ms),
                    }),
                }
            }
            HarborOp::Compact => {
                if !session.capabilities().manual_compaction {
                    json!({
                        "op": "compact",
                        "result": "unsupported",
                    })
                } else {
                    let before_ctx = session.snapshot().context_usage.and_then(|c| c.tokens);
                    let opts = CompactionOptions {
                        timeout: Some(Duration::from_secs(300)),
                        ..Default::default()
                    };
                    match session.compact_with(&opts).await {
                        Ok(report) => json!({
                            "op": "compact",
                            "report": report,
                            "context_before": before_ctx,
                            "context_after": session.snapshot().context_usage.and_then(|c| c.tokens),
                            "ms": timestamp_ms().saturating_sub(started_ms),
                        }),
                        Err(err) => json!({
                            "op": "compact",
                            "error": err.to_string(),
                            "ms": timestamp_ms().saturating_sub(started_ms),
                        }),
                    }
                }
            }
            HarborOp::StopTask => {
                if !session.capabilities().stop_task {
                    json!({
                        "op": "stop-task",
                        "result": "unsupported",
                    })
                } else {
                    match session.stop_task(arg.unwrap_or("")).await {
                        Ok(()) => json!({
                            "op": "stop-task",
                            "accepted": true,
                            "ms": timestamp_ms().saturating_sub(started_ms),
                        }),
                        Err(err) => json!({
                            "op": "stop-task",
                            "accepted": false,
                            "error": err.to_string(),
                            "ms": timestamp_ms().saturating_sub(started_ms),
                        }),
                    }
                }
            }
            HarborOp::Usage => {
                let snap = session.snapshot();
                json!({
                    "op": "usage",
                    "usage": snap.usage,
                    "context": snap.context_usage,
                    "status": snap.status,
                })
            }
        };

        (body, findings)
    }
}

/// Enumerate PIDs (excluding the current Harbor process) whose working directory lies within `cwd`.
pub fn processes_in_cwd(cwd: &Path) -> Vec<u32> {
    let Ok(canonical_cwd) = fs::canonicalize(cwd) else {
        return Vec::new();
    };
    let self_pid = std::process::id();

    #[cfg(target_os = "macos")]
    {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let mut sys = System::new();
        let kind = ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .without_tasks();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let mut pids: Vec<u32> = sys
            .processes()
            .iter()
            .filter_map(|(pid, proc_info)| {
                let raw = pid.as_u32();
                if raw == self_pid || raw == 0 {
                    return None;
                }
                let proc_cwd = proc_info.cwd()?;
                let resolved =
                    fs::canonicalize(proc_cwd).unwrap_or_else(|_| proc_cwd.to_path_buf());
                resolved.starts_with(&canonical_cwd).then_some(raw)
            })
            .collect();
        pids.sort_unstable();
        pids
    }

    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut pids = Vec::new();
        for entry in entries.flatten() {
            let Some(pid_str) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(raw) = pid_str.parse::<u32>() else {
                continue;
            };
            if raw == self_pid || raw == 0 {
                continue;
            };
            let Ok(link) = fs::read_link(entry.path().join("cwd")) else {
                continue;
            };
            let resolved = fs::canonicalize(&link).unwrap_or(link);
            if resolved.starts_with(&canonical_cwd) {
                pids.push(raw);
            }
        }
        pids.sort_unstable();
        pids
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (canonical_cwd, self_pid);
        Vec::new()
    }
}
