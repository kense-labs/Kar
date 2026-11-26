//! Multi-agent soak & SDK contract-auditing daemon (`kar::harbor`).
//!
//! Pure-Rust equivalent of Oar Harbor supporting persistent multi-agent workspaces
//! (`~/.kar-harbor/<name>` or `--dir`), `channel.jsonl`, `journal.jsonl`, per-session
//! `.voyage.jsonl` crash-safe recordings, `@agent` / `@all` message routing via
//! [`crate::Session::deliver`], crash redelivery of unconsumed inputs, UTC daily token
//! budgeting (`--day-tokens`), automatic idle-context session recycling (`--recycle-idle-min`
//! & `--recycle-context`) with ~10K-token channel handoff, `--max-disk-gb` workspace disk
//! limits, and live SDK contract auditing ([`Finding`] / [`ContractAuditor`]).

pub mod budget;
pub mod contracts;
pub mod daemon;
pub mod inspect;
pub mod store;

pub use budget::{
    AgentBudget, HANDOFF_MAX_CHARS, RecyclePolicy, add_day_tokens, build_handoff, day_exhausted,
    directory_bytes, exceeds_disk_limit, ms_until_next_utc_day, should_recycle, utc_day_string,
};
pub use contracts::{
    ContractAuditor, DEFAULT_ABORT_SETTLE_TIMEOUT, Finding, FindingKind, HarborOp, processes_in_cwd,
};
pub use daemon::{
    HarborDaemon, HarborDaemonConfig, NativeSessionFactory, ScriptedSessionFactory, SendReceipt,
    SessionFactory,
};
pub use inspect::{
    AgentStatusRow, TraceOptions, findings_for, format_journal_entry, format_messages,
    format_status_table, read_messages, trace_voyage, watch_journal,
};
pub use store::{
    AgentSpec, AgentState, DaemonEndpoint, DayTokenCount, HarborMessage, HarborState, HarborStore,
    JournalEntry, UndeliveredItem, format_channel_message, mentions_in, parse_agent_env,
    resolve_harbor_dir, route_recipients, validate_agent_name,
};

use crate::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

/// Top-level Clap CLI arguments for `kar harbor <subcommand>`.
#[derive(Clone, Debug, Args)]
pub struct HarborCli {
    /// Harbor workspace directory (defaults to `$KAR_HARBOR_DIR`, `$HARBOR_DIR`, or `~/.kar-harbor/<name>`).
    #[arg(long, global = true)]
    pub dir: Option<PathBuf>,

    /// Named Harbor run under `~/.kar-harbor/<name>` when `--dir` is omitted.
    #[arg(long, global = true, default_value = "default")]
    pub name: String,

    #[command(subcommand)]
    pub command: HarborCommand,
}

/// Subcommands supported by `kar harbor`.
#[derive(Clone, Debug, Subcommand)]
pub enum HarborCommand {
    /// Start the Harbor multi-agent daemon and open agent sessions.
    Up {
        /// Agent specification `<name>=<runtime>[:<model>]` (repeatable).
        #[arg(long = "agent")]
        agents: Vec<String>,

        /// Per-agent environment variable `<agent>:<KEY>=<VALUE>` (repeatable).
        #[arg(long = "agent-env")]
        agent_env: Vec<String>,

        /// Running-turn silence threshold in minutes before recording a `stalled` finding.
        #[arg(long = "stall-min", default_value_t = 10)]
        stall_min: u64,

        /// Optional total soak duration in hours before automatic clean shutdown.
        #[arg(long = "for")]
        for_hours: Option<f64>,

        /// Maximum Harbor workspace disk usage in GiB before stopping cleanly.
        #[arg(long = "max-disk-gb")]
        max_disk_gb: Option<f64>,

        /// Idle threshold in minutes before recycling a large-context session (`0` disables).
        #[arg(long = "recycle-idle-min", default_value_t = 60)]
        recycle_idle_min: u64,

        /// Context occupancy threshold in tokens paired with `--recycle-idle-min`.
        #[arg(long = "recycle-context", default_value_t = 128_000)]
        recycle_context: u64,

        /// Maximum reported tokens per agent per UTC day before holding deliveries until midnight.
        #[arg(long = "day-tokens")]
        day_tokens: Option<u64>,

        /// Spawn the Harbor daemon in the background and return immediately.
        #[arg(long)]
        detach: bool,
    },
    /// Gracefully dispose all Harbor agent sessions and stop the daemon.
    Down,
    /// Post a message to `channel.jsonl` (`@name` routes to an agent, `@all` broadcasts).
    Send {
        /// Sender handle (defaults to `$HARBOR_AGENT` or `"qa"`).
        #[arg(long)]
        from: Option<String>,

        /// Message text (reads stdin when omitted).
        text: Vec<String>,
    },
    /// Read recent messages from `channel.jsonl`.
    Read {
        /// Only return messages with `seq > after`.
        #[arg(long, default_value_t = 0)]
        after: u64,

        /// Maximum number of messages to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show live state, turn duration, context occupancy, day tokens, and pending queue for each agent.
    Status,
    /// Follow `journal.jsonl` findings (or all lifecycle events with `--all`) in real time.
    Watch {
        #[arg(long)]
        all: bool,
    },
    /// List contract-violation findings recorded so far.
    Findings {
        /// Optional agent name filter.
        agent: Option<String>,
    },
    /// Inspect recorded `.voyage.jsonl` frames for an agent session or explicit voyage path.
    Trace {
        /// Target agent name (optional when `--voyage` is provided).
        agent: Option<String>,

        /// 1-based turn index within the voyage recording.
        #[arg(long)]
        turn: Option<usize>,

        /// Show a window of records around this sequence number.
        #[arg(long)]
        around: Option<u64>,

        /// 1-based session open number (`session-<N>.voyage.jsonl`).
        #[arg(long)]
        session: Option<u64>,

        /// Explicit `.voyage.jsonl` path (relative to Harbor dir or absolute).
        #[arg(long)]
        voyage: Option<PathBuf>,

        /// Emit raw JSONL `Record` lines instead of compact summaries.
        #[arg(long)]
        raw: bool,
    },
    /// Dynamically add `<name>=<runtime>[:<model>]` to a running Harbor daemon.
    Add { spec: String },
    /// Dispose and reopen one agent session (resuming saved session unless `--fresh`).
    Restart {
        agent: String,
        #[arg(long)]
        fresh: bool,
    },
    /// Execute a QA control operation (`abort`, `steer`, `compact`, `stop-task`, `usage`) and audit SDK promises.
    Op {
        agent: String,
        #[arg(value_enum)]
        op: HarborOp,
        arg: Vec<String>,
    },
}

/// Send an authenticated loopback HTTP request to a running Harbor daemon.
pub async fn call_daemon(store: &HarborStore, route: &str, body: Value) -> Result<Value> {
    let endpoint = store.read_daemon_endpoint()?.ok_or_else(|| {
        Error::Closed(format!(
            "no running Harbor daemon found in {} (`daemon.json` absent)",
            store.dir().display()
        ))
    })?;
    let url = format!(
        "http://127.0.0.1:{}/{}",
        endpoint.port,
        route.trim_start_matches('/')
    );
    let client = reqwest::Client::builder().no_proxy().build()?;
    let resp = client
        .post(&url)
        .bearer_auth(&endpoint.token)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let val: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let msg = val["error"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Harbor daemon HTTP {}", status.as_u16()));
        return Err(Error::Refused(msg));
    }
    Ok(val)
}

#[allow(clippy::too_many_arguments)]
fn spawn_detached_up(
    dir: &std::path::Path,
    agents: &[String],
    agent_env: &[String],
    stall_min: u64,
    for_hours: Option<f64>,
    max_disk_gb: Option<f64>,
    recycle_idle_min: u64,
    recycle_context: u64,
    day_tokens: Option<u64>,
) -> Result<u32> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(dir)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    let err_file = log_file.try_clone()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("harbor")
        .arg("--dir")
        .arg(dir)
        .arg("up")
        .arg("--stall-min")
        .arg(stall_min.to_string())
        .arg("--recycle-idle-min")
        .arg(recycle_idle_min.to_string())
        .arg("--recycle-context")
        .arg(recycle_context.to_string());
    for a in agents {
        cmd.arg("--agent").arg(a);
    }
    for e in agent_env {
        cmd.arg("--agent-env").arg(e);
    }
    if let Some(h) = for_hours {
        cmd.arg("--for").arg(h.to_string());
    }
    if let Some(gb) = max_disk_gb {
        cmd.arg("--max-disk-gb").arg(gb.to_string());
    }
    if let Some(dt) = day_tokens {
        cmd.arg("--day-tokens").arg(dt.to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(err_file));

    let child = cmd.spawn()?;
    Ok(child.id())
}

/// Entrypoint executing any [`HarborCli`] command.
pub async fn run(cli: HarborCli) -> Result<()> {
    let dir = resolve_harbor_dir(cli.dir.as_deref(), Some(&cli.name))?;
    let store = Arc::new(HarborStore::open(&dir)?);

    match cli.command {
        HarborCommand::Up {
            agents,
            agent_env,
            stall_min,
            for_hours,
            max_disk_gb,
            recycle_idle_min,
            recycle_context,
            day_tokens,
            detach,
        } => {
            if let Some(dt) = day_tokens
                && dt == 0
            {
                return Err(Error::Refused(
                    "`--day-tokens` must be at least 1 token".into(),
                ));
            }
            if let Some(h) = for_hours
                && (!h.is_finite() || h <= 0.0)
            {
                return Err(Error::Refused(
                    "`--for` must be a positive number of hours".into(),
                ));
            }
            if let Some(gb) = max_disk_gb
                && (!gb.is_finite() || gb <= 0.0)
            {
                return Err(Error::Refused(
                    "`--max-disk-gb` must be a positive number of GiB".into(),
                ));
            }

            let parsed_specs: Vec<AgentSpec> = agents
                .iter()
                .map(|s| AgentSpec::parse(s))
                .collect::<Result<_>>()?;
            let parsed_env = parse_agent_env(&agent_env)?;

            if detach {
                let pid = spawn_detached_up(
                    &dir,
                    &agents,
                    &agent_env,
                    stall_min,
                    for_hours,
                    max_disk_gb,
                    recycle_idle_min,
                    recycle_context,
                    day_tokens,
                )?;
                println!(
                    "{}",
                    json!({
                        "detached": true,
                        "pid": pid,
                        "dir": dir,
                    })
                );
                return Ok(());
            }

            let config = HarborDaemonConfig {
                agents: parsed_specs,
                agent_env: parsed_env,
                stall_timeout: Duration::from_secs(stall_min.saturating_mul(60)),
                max_duration: for_hours.map(|h| Duration::from_secs_f64(h * 3600.0)),
                max_disk_bytes: max_disk_gb.map(|gb| (gb * (1024.0 * 1024.0 * 1024.0)) as u64),
                recycle: RecyclePolicy::from_limits(recycle_idle_min, recycle_context),
                day_tokens,
                ..HarborDaemonConfig::default()
            };

            let (daemon, join) = HarborDaemon::start_native(store, config).await?;
            let ctrl_daemon = daemon.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    ctrl_daemon.request_shutdown();
                }
            });
            join.await
                .map_err(|e| Error::Closed(format!("Harbor daemon task failed: {e}")))??;
        }
        HarborCommand::Down => {
            let reply = call_daemon(&store, "/down", json!({})).await?;
            println!("{reply}");
        }
        HarborCommand::Send { from, text } => {
            let body_text = if !text.is_empty() {
                text.join(" ")
            } else {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            };
            let sender = from
                .or_else(|| std::env::var("HARBOR_AGENT").ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| "qa".into());
            let reply = call_daemon(
                &store,
                "/send",
                json!({
                    "from": sender,
                    "text": body_text,
                }),
            )
            .await?;
            let seq = reply["seq"].as_u64().unwrap_or(0);
            let recipients: Vec<String> = reply["to"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|n| format!("@{n}"))
                .collect();
            let targets = if recipients.is_empty() {
                "nobody".into()
            } else {
                recipients.join(" ")
            };
            println!("sent msg={seq} to {targets}");
        }
        HarborCommand::Read { after, limit } => {
            let msgs = read_messages(&store, after, limit)?;
            let formatted = format_messages(&msgs);
            if !formatted.is_empty() {
                println!("{formatted}");
            }
        }
        HarborCommand::Status => {
            let reply = call_daemon(&store, "/status", json!({})).await?;
            let rows: Vec<AgentStatusRow> =
                serde_json::from_value(reply["agents"].clone()).unwrap_or_default();
            println!("{}", format_status_table(&rows));
        }
        HarborCommand::Watch { all } => {
            watch_journal(&store, all).await?;
        }
        HarborCommand::Findings { agent } => {
            for entry in findings_for(&store, agent.as_deref())? {
                println!("{}", format_journal_entry(&entry));
            }
        }
        HarborCommand::Trace {
            agent,
            turn,
            around,
            session,
            voyage,
            raw,
        } => {
            let output = trace_voyage(
                &store,
                &TraceOptions {
                    agent,
                    session,
                    around_seq: around,
                    turn,
                    voyage_path: voyage,
                    raw,
                },
            )?;
            println!("{output}");
        }
        HarborCommand::Add { spec } => {
            let reply = call_daemon(&store, "/add", json!({ "spec": spec })).await?;
            println!("{reply}");
        }
        HarborCommand::Restart { agent, fresh } => {
            let reply = call_daemon(
                &store,
                "/restart",
                json!({ "agent": agent, "fresh": fresh }),
            )
            .await?;
            println!("{reply}");
        }
        HarborCommand::Op { agent, op, arg } => {
            let joined = (!arg.is_empty()).then(|| arg.join(" "));
            let reply = call_daemon(
                &store,
                "/op",
                json!({
                    "agent": agent,
                    "op": op,
                    "arg": joined,
                }),
            )
            .await?;
            println!("{reply}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AttributionTier, Capabilities, ContextUsage, ContextUsageCapability, Event, EventCursor,
        FailureClass, ManualCompactionCapability, ObservedEvent, Outcome, Protocol, SessionUsage,
        TokenTotals, TurnFailure,
        observe::{AgentStatus, Phase},
        testing::{ScriptedStep, scripted_runtime},
    };
    use std::collections::BTreeMap;

    #[test]
    fn routing_handles_mentions_all_broadcast_and_agent_silence() {
        let agents = vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];

        // External sender without @mention broadcasts to all registered agents.
        assert_eq!(
            route_recipients("qa", "run smoke suite", &agents),
            vec!["alpha", "beta", "gamma"]
        );

        // Agent without @mention routes to nobody (prevents chatter loops).
        assert!(route_recipients("alpha", "thinking out loud", &agents).is_empty());

        // Explicit @agent routes only to named agents (excluding sender).
        assert_eq!(
            route_recipients("alpha", "hey @beta and @alpha check this", &agents),
            vec!["beta"]
        );

        // @all routes to every other agent, excluding sender.
        assert_eq!(
            route_recipients("beta", "@all status check", &agents),
            vec!["alpha", "gamma"]
        );
    }

    #[test]
    fn utc_day_budget_persists_pauses_and_rolls_over_at_midnight() {
        // 2026-11-26T12:00:00Z = 20783 * 86_400_000 + 43_200_000
        let noon_day1 = 1_795_694_400_000u64;
        let day1_str = utc_day_string(noon_day1);
        assert_eq!(day1_str, "2026-11-26");
        assert_eq!(ms_until_next_utc_day(noon_day1), 43_200_000);

        let mut budget = AgentBudget::new(None, Some(1_000), noon_day1);
        let usage_600 = SessionUsage {
            total: Some(TokenTotals {
                input: 400,
                output: 200,
                ..TokenTotals::default()
            }),
            ..SessionUsage::default()
        };
        let (state1, delta1) = budget.record_usage(&usage_600, None, noon_day1);
        assert_eq!(delta1, 600);
        assert_eq!(state1.tokens, 600);
        assert!(budget.is_day_paused(Some(&state1), noon_day1).is_none());

        let usage_1100 = SessionUsage {
            total: Some(TokenTotals {
                input: 700,
                output: 400,
                ..TokenTotals::default()
            }),
            ..SessionUsage::default()
        };
        let (state2, delta2) = budget.record_usage(&usage_1100, Some(&state1), noon_day1 + 5_000);
        assert_eq!(delta2, 500);
        assert_eq!(state2.tokens, 1_100);
        assert!(
            budget
                .is_day_paused(Some(&state2), noon_day1 + 5_000)
                .is_some()
        );

        // Next UTC day automatically lifts the pause and resets day token accumulation!
        let next_day = noon_day1 + 86_400_000;
        assert_eq!(utc_day_string(next_day), "2026-11-27");
        assert!(budget.is_day_paused(Some(&state2), next_day).is_none());
        let (state3, _) = budget.record_usage(&usage_1100, Some(&state2), next_day);
        assert_eq!(state3.day, "2026-11-27");
        assert_eq!(state3.tokens, 0);
    }

    #[test]
    fn recycle_policy_and_bounded_10k_token_handoff_respect_budget_and_ordering() {
        let policy = RecyclePolicy::from_limits(10, 50_000);
        assert!(!should_recycle(policy, Some(5 * 60_000), Some(80_000)));
        assert!(!should_recycle(policy, Some(15 * 60_000), Some(30_000)));
        assert!(should_recycle(policy, Some(15 * 60_000), Some(80_000)));
        assert!(!should_recycle(
            RecyclePolicy::from_limits(0, 50_000),
            Some(999_999),
            Some(999_999)
        ));

        let messages = vec![
            HarborMessage {
                seq: 1,
                timestamp_ms: 1,
                from: "qa".into(),
                to: vec!["alpha".into()],
                text: "first task".into(),
            },
            HarborMessage {
                seq: 2,
                timestamp_ms: 2,
                from: "beta".into(),
                to: vec!["gamma".into()],
                text: "unrelated peer message".into(),
            },
            HarborMessage {
                seq: 3,
                timestamp_ms: 3,
                from: "alpha".into(),
                to: vec!["qa".into()],
                text: "completed first task".into(),
            },
            HarborMessage {
                seq: 4,
                timestamp_ms: 4,
                from: "qa".into(),
                to: vec!["alpha".into()],
                text: "triggering message excluded by before_seq".into(),
            },
        ];

        let handoff = build_handoff(&messages, "alpha", 4, HANDOFF_MAX_CHARS).unwrap();
        assert!(handoff.contains("[#1 @qa -> @alpha] first task"));
        assert!(handoff.contains("[#3 @alpha -> @qa] completed first task"));
        assert!(!handoff.contains("unrelated peer message"));
        assert!(!handoff.contains("triggering message"));
        let idx1 = handoff.find("[#1").unwrap();
        let idx3 = handoff.find("[#3").unwrap();
        assert!(idx1 < idx3);

        // Oversized single message still produces a truncated ~10K-token excerpt within max_chars.
        let huge = vec![HarborMessage {
            seq: 1,
            timestamp_ms: 1,
            from: "qa".into(),
            to: vec!["alpha".into()],
            text: "x".repeat(60_000),
        }];
        let bounded = build_handoff(&huge, "alpha", 2, HANDOFF_MAX_CHARS).unwrap();
        assert!(bounded.contains("[truncated;"));
        assert!(bounded.len() <= HANDOFF_MAX_CHARS + 512);
    }

    #[test]
    fn disk_limit_check_measures_directory_bytes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("blob.bin"), vec![0u8; 4096])?;
        assert_eq!(directory_bytes(dir.path())?, 4096);
        assert_eq!(exceeds_disk_limit(dir.path(), Some(8192))?, None);
        assert_eq!(exceeds_disk_limit(dir.path(), Some(2048))?, Some(4096));
        Ok(())
    }

    #[test]
    fn contract_auditor_detects_all_sdk_violations() {
        let mut auditor = ContractAuditor::new("coder");
        auditor.set_voyage(Some("agents/coder/session-1.voyage.jsonl".into()));

        // 1. declaration_mismatch
        let caps = Capabilities {
            attribution: Some(AttributionTier::None),
            native_memory: None,
            protocol: Protocol::Pi,
            image_input: false,
            resume: true,
            fork: false,
            steering: true,
            session_mcp: false,
            mcp_transports: None,
            mcp_startup: None,
            native_session_file: false,
            model_requests: false,
            host_queue: true,
            durable_native_queue: false,
            output_schema: false,
            stop_task: false,
            goals: false,
            manual_compaction: false,
        };
        let decl = crate::SessionCapabilityDeclaration {
            image_input: Some(true), // Mismatch: opened has image_input = false!
            context_usage: Some(ContextUsageCapability {
                tokens: true,
                context_window: true,
            }),
            ..Default::default()
        };
        let open_findings = auditor.audit_opened(1, &caps, decl);
        assert_eq!(open_findings.len(), 1);
        assert_eq!(open_findings[0].check, FindingKind::DeclarationMismatch);

        let make_ev = |seq: u64, event: Event| ObservedEvent {
            cursor: EventCursor {
                stream_id: uuid::Uuid::nil(),
                seq,
                index: 0,
            },
            timestamp_ms: 1_000,
            session_id: Some("s1".into()),
            agent_path: Vec::new(),
            span_id: None,
            elided: false,
            message: Default::default(),
            event,
        };

        // 2. context_usage_missing on completed turn when promised
        let usage_100 = SessionUsage {
            total: Some(TokenTotals {
                input: 60,
                output: 40,
                ..TokenTotals::default()
            }),
            ..SessionUsage::default()
        };
        let ev_completed = make_ev(
            2,
            Event::TurnEnded {
                outcome: Outcome::Completed,
            },
        );
        let missing_ctx = auditor.audit_event(1, &ev_completed, &usage_100, None, None);
        assert!(
            missing_ctx
                .iter()
                .any(|f| f.check == FindingKind::ContextUsageMissing)
        );

        // 3. usage_decreased + 4. turn_failed
        let usage_50 = SessionUsage {
            total: Some(TokenTotals {
                input: 30,
                output: 20,
                ..TokenTotals::default()
            }),
            ..SessionUsage::default()
        };
        let failure = TurnFailure {
            class: FailureClass::Provider,
            reason: "502 bad gateway".into(),
            status: Some(502),
            credential: None,
            resets_at_ms: None,
            rate_limit_type: None,
        };
        let ev_failed = make_ev(
            3,
            Event::TurnEnded {
                outcome: Outcome::Failed,
            },
        );
        let turn_findings = auditor.audit_event(
            1,
            &ev_failed,
            &usage_50,
            Some(&ContextUsage {
                tokens: Some(50),
                context_window: Some(200_000),
                percent: Some(0.025),
            }),
            Some(&failure),
        );
        assert!(
            turn_findings
                .iter()
                .any(|f| f.check == FindingKind::UsageDecreased)
        );
        assert!(
            turn_findings
                .iter()
                .any(|f| f.check == FindingKind::TurnFailed)
        );

        // 5. stalled (deduplicated per session + since_seq)
        let running = AgentStatus::Running {
            input_id: Some("in-1".into()),
            request_id: Some("req-1".into()),
            since_seq: 4,
            since_ms: 1_000,
            last_event_ms: 1_000,
            phase: Phase::WaitingModel,
        };
        let stall1 = auditor
            .audit_stall(1, &running, 700_000, Duration::from_secs(600))
            .expect("stall reported");
        assert_eq!(stall1.check, FindingKind::Stalled);
        assert!(
            auditor
                .audit_stall(1, &running, 800_000, Duration::from_secs(600))
                .is_none()
        );

        // 6. unexpected_exit (+ resume_loop on second rapid exit)
        let (exit1, fresh1) = auditor.audit_unexpected_exit(1, 5, "SIGSEGV", 1_000, 5_000);
        assert!(!fresh1);
        assert_eq!(exit1[0].check, FindingKind::UnexpectedExit);
        let (exit2, fresh2) = auditor.audit_unexpected_exit(2, 2, "SIGSEGV", 6_000, 10_000);
        assert!(fresh2);
        assert!(exit2.iter().any(|f| f.check == FindingKind::ResumeLoop));

        // 7. leftover_processes
        let leftover = auditor
            .audit_leftover_pids(2, vec![4242, 4243])
            .expect("leftover processes reported");
        assert_eq!(leftover.check, FindingKind::LeftoverProcesses);
        let _ = ManualCompactionCapability::Unsupported { reason: "none" };
    }

    #[tokio::test]
    async fn harbor_daemon_routes_messages_records_voyages_redelivers_crashes_and_audits_ops()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Arc::new(HarborStore::open(dir.path().join("harbor-run"))?);

        // Seed an unconsumed crash-leftover message in channel.jsonl + state.json BEFORE starting!
        let crash_msg = store.append_message("qa", vec!["alpha".into()], "recover after crash")?;
        let mut pre_state = HarborState::default();
        let mut alpha_pre =
            AgentState::from_spec(AgentSpec::parse("alpha=mock-alpha")?, BTreeMap::new());
        alpha_pre.undelivered = vec![UndeliveredItem {
            seq: crash_msg.seq,
            from_person: true,
        }];
        pre_state.agents.insert("alpha".into(), alpha_pre);
        store.save_state(&pre_state)?;

        let factory = ScriptedSessionFactory::new();
        factory.register(scripted_runtime(
            "mock-alpha",
            vec![
                ScriptedStep::reply("recovered crash input"),
                ScriptedStep::reply("handled @all broadcast"),
            ],
        ));
        factory.register(scripted_runtime(
            "mock-beta",
            vec![ScriptedStep::reply("beta got @all")],
        ));

        let config = HarborDaemonConfig {
            agents: vec![
                AgentSpec::parse("alpha=mock-alpha")?,
                AgentSpec::parse("beta=mock-beta")?,
            ],
            watchdog_interval: Duration::from_millis(50),
            ..HarborDaemonConfig::default()
        };

        let (daemon, join) =
            HarborDaemon::start_with_factory(Arc::clone(&store), config, Arc::new(factory)).await?;

        // Wait briefly for crash redelivery turn on `alpha` to settle.
        for _ in 0..40 {
            let statuses = daemon.status().await;
            let alpha_row = statuses.iter().find(|r| r.name == "alpha").unwrap();
            if alpha_row.turns >= 1 && alpha_row.state == "idle" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        // Send `@all` message via loopback HTTP `call_daemon`!
        let sent = call_daemon(
            &store,
            "/send",
            json!({ "from": "qa", "text": "@all run integration check" }),
        )
        .await?;
        assert_eq!(sent["to"], json!(["alpha", "beta"]));

        for _ in 0..40 {
            let statuses = daemon.status().await;
            let a = statuses.iter().find(|r| r.name == "alpha").unwrap();
            let b = statuses.iter().find(|r| r.name == "beta").unwrap();
            if a.turns >= 2 && a.state == "idle" && b.turns >= 1 && b.state == "idle" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        // Exercise QA `op` (`usage`) over loopback HTTP!
        let op_reply =
            call_daemon(&store, "/op", json!({ "agent": "alpha", "op": "usage" })).await?;
        assert_eq!(op_reply["op"], "usage");

        // Verify `.voyage.jsonl` was recorded crash-safely and `trace_voyage` reads it!
        let trace_text = trace_voyage(
            &store,
            &TraceOptions {
                agent: Some("alpha".into()),
                ..TraceOptions::default()
            },
        )?;
        assert!(trace_text.contains("total_records="));

        // Verify `redelivering` was journaled for the crash-recovered message!
        let entries = store.journal_entries()?;
        assert!(entries.iter().any(|e| e.kind == "redelivering"));

        // Shut down cleanly via `/down`!
        let _ = call_daemon(&store, "/down", json!({})).await?;
        join.await.unwrap()?;
        assert!(store.read_daemon_endpoint()?.is_none());
        Ok(())
    }
}
