//! Multi-agent Harbor daemon, per-agent session lifecycle & crash-redelivery actor,
//! and loopback Axum control server (`kar::harbor::daemon`).

use super::{
    budget::{AgentBudget, HANDOFF_MAX_CHARS, RecyclePolicy, build_handoff, exceeds_disk_limit},
    contracts::{ContractAuditor, DEFAULT_ABORT_SETTLE_TIMEOUT, Finding, FindingKind, HarborOp},
    inspect::AgentStatusRow,
    store::{
        AgentSpec, AgentState, DaemonEndpoint, HarborMessage, HarborState, HarborStore,
        UndeliveredItem, format_channel_message, route_recipients,
    },
};
use crate::{
    Delivery, Error, Event, EventSubscription, InstallationOptions, InstallationSnapshot, Result,
    Session, SessionCapabilityDeclaration, SessionOptions, Subscription,
    stream::timestamp_ms,
    testing::ScriptedRuntime,
    voyage::{VoyageHeader, VoyageRecorder},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

pub type OpenedAgentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(Session, SessionCapabilityDeclaration)>> + Send + 'a>>;

/// Pluggable factory opening a [`Session`] + [`SessionCapabilityDeclaration`] for a Harbor agent.
pub trait SessionFactory: Send + Sync + 'static {
    fn open_agent(&self, runtime_id: &str, options: SessionOptions) -> OpenedAgentFuture<'_>;
}

/// Production [`SessionFactory`] probing and launching installed vendor runtimes via [`crate::runtime`].
#[derive(Clone, Default)]
pub struct NativeSessionFactory;

impl SessionFactory for NativeSessionFactory {
    fn open_agent(
        &self,
        runtime_id: &str,
        options: SessionOptions,
    ) -> Pin<Box<dyn Future<Output = Result<(Session, SessionCapabilityDeclaration)>> + Send + '_>>
    {
        let runtime_id = runtime_id.to_owned();
        Box::pin(async move {
            let rt = crate::runtime(&runtime_id)?;
            let mut probe_opts = InstallationOptions {
                cwd: Some(options.cwd.clone()),
                env: BTreeMap::new(),
            };
            for k in &options.unset_env {
                probe_opts.env.insert(OsString::from(k), None);
            }
            for (k, v) in &options.env {
                probe_opts.env.insert(k.clone(), Some(v.clone()));
            }
            let snap = rt.probe_installation(&probe_opts).await?;
            if !matches!(snap, InstallationSnapshot::Available { .. }) {
                return Err(Error::Unsupported(format!(
                    "runtime {runtime_id:?} is not available on this host ({snap:?})"
                )));
            }
            let declared = rt.session_capabilities(&snap).unwrap_or_default();
            let session = rt.open(options).await?;
            Ok((session, declared))
        })
    }
}

/// Deterministic [`SessionFactory`] backed by [`ScriptedRuntime`] instances for zero-token soak testing.
#[derive(Clone, Default)]
pub struct ScriptedSessionFactory {
    runtimes: Arc<Mutex<BTreeMap<String, ScriptedRuntime>>>,
}

impl ScriptedSessionFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, runtime: ScriptedRuntime) {
        self.runtimes
            .lock()
            .unwrap()
            .insert(runtime.id().to_owned(), runtime);
    }
}

impl SessionFactory for ScriptedSessionFactory {
    fn open_agent(
        &self,
        runtime_id: &str,
        options: SessionOptions,
    ) -> Pin<Box<dyn Future<Output = Result<(Session, SessionCapabilityDeclaration)>> + Send + '_>>
    {
        let found = self.runtimes.lock().unwrap().get(runtime_id).cloned();
        let runtime_id = runtime_id.to_owned();
        Box::pin(async move {
            let Some(rt) = found else {
                return Err(Error::Unsupported(format!(
                    "unregistered scripted runtime {runtime_id:?}"
                )));
            };
            let declared = rt.capability_declaration();
            let session = rt.open(options).await?;
            Ok((session.into_session(), declared))
        })
    }
}

/// Configuration for [`HarborDaemon`].
#[derive(Clone, Debug)]
pub struct HarborDaemonConfig {
    pub agents: Vec<AgentSpec>,
    pub agent_env: BTreeMap<String, BTreeMap<String, String>>,
    pub stall_timeout: Duration,
    pub max_duration: Option<Duration>,
    pub max_disk_bytes: Option<u64>,
    pub recycle: Option<RecyclePolicy>,
    pub day_tokens: Option<u64>,
    pub watchdog_interval: Duration,
    pub abort_settle_timeout: Duration,
    pub harbor_bin_hint: String,
}

impl Default for HarborDaemonConfig {
    fn default() -> Self {
        let bin_hint = std::env::current_exe()
            .ok()
            .map(|p| format!("{} harbor", p.display()))
            .unwrap_or_else(|| "kar harbor".into());
        Self {
            agents: Vec::new(),
            agent_env: BTreeMap::new(),
            stall_timeout: Duration::from_secs(10 * 60),
            max_duration: None,
            max_disk_bytes: None,
            recycle: RecyclePolicy::from_limits(60, 128_000),
            day_tokens: None,
            watchdog_interval: Duration::from_secs(5),
            abort_settle_timeout: DEFAULT_ABORT_SETTLE_TIMEOUT,
            harbor_bin_hint: bin_hint,
        }
    }
}

fn channel_preamble(agent_name: &str, cwd: &Path, bin_hint: &str, harbor_dir: &Path) -> String {
    format!(
        "You are Harbor agent @{agent_name} working in `{}`.\n\
         Collaborate with humans and peer agents through the Harbor channel:\n\
         - Post replies with `{bin_hint} --dir \"{}\" send --from {agent_name} \"@recipient ...\"` (or `@all`).\n\
         - Read earlier channel history with `{bin_hint} --dir \"{}\" read`.\n\
         Plain assistant text is NOT posted to the channel automatically unless sent via `harbor send`.",
        cwd.display(),
        harbor_dir.display(),
        harbor_dir.display(),
    )
}

enum AgentCmd {
    Open {
        fresh: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Deliver {
        message: HarborMessage,
        from_person: bool,
    },
    Restore {
        undelivered: Vec<UndeliveredItem>,
        messages: Vec<HarborMessage>,
    },
    Replied,
    Restart {
        fresh: bool,
        reply: oneshot::Sender<Result<Value>>,
    },
    Op {
        op: HarborOp,
        arg: Option<String>,
        reply: oneshot::Sender<Value>,
    },
    Status {
        now_ms: u64,
        reply: oneshot::Sender<AgentStatusRow>,
    },
    Tick {
        now_ms: u64,
    },
    Close {
        why: String,
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
struct HarborAgentHandle {
    tx: mpsc::UnboundedSender<AgentCmd>,
}

impl HarborAgentHandle {
    async fn open(&self, fresh: bool) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(AgentCmd::Open { fresh, reply: tx });
        rx.await
            .map_err(|_| Error::Closed("harbor agent actor stopped".into()))?
    }

    fn deliver(&self, message: HarborMessage, from_person: bool) {
        let _ = self.tx.send(AgentCmd::Deliver {
            message,
            from_person,
        });
    }

    fn restore(&self, undelivered: Vec<UndeliveredItem>, messages: Vec<HarborMessage>) {
        let _ = self.tx.send(AgentCmd::Restore {
            undelivered,
            messages,
        });
    }

    fn replied(&self) {
        let _ = self.tx.send(AgentCmd::Replied);
    }

    async fn restart(&self, fresh: bool) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(AgentCmd::Restart { fresh, reply: tx });
        rx.await
            .map_err(|_| Error::Closed("harbor agent actor stopped".into()))?
    }

    async fn op(&self, op: HarborOp, arg: Option<String>) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(AgentCmd::Op { op, arg, reply: tx });
        rx.await
            .map_err(|_| Error::Closed("harbor agent actor stopped".into()))
    }

    async fn status(&self, now_ms: u64) -> Option<AgentStatusRow> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(AgentCmd::Status { now_ms, reply: tx });
        rx.await.ok()
    }

    fn tick(&self, now_ms: u64) {
        let _ = self.tx.send(AgentCmd::Tick { now_ms });
    }

    async fn close(&self, why: impl Into<String>) {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(AgentCmd::Close {
            why: why.into(),
            reply: tx,
        });
        let _ = rx.await;
    }
}

#[derive(Clone, Debug)]
struct QueuedMessage {
    message: HarborMessage,
    from_person: bool,
}

#[derive(Clone, Debug)]
struct AcceptedBatch {
    items: Vec<QueuedMessage>,
    landed_queued: bool,
}

struct AgentActor {
    state: AgentState,
    store: Arc<HarborStore>,
    shared_state: Arc<Mutex<HarborState>>,
    factory: Arc<dyn SessionFactory>,
    config: HarborDaemonConfig,
    budget: AgentBudget,
    auditor: ContractAuditor,
    session: Option<Session>,
    recorder: Option<VoyageRecorder>,
    record_sub: Option<Subscription>,
    event_sub: Option<EventSubscription>,
    inbox: VecDeque<QueuedMessage>,
    accepted: BTreeMap<Uuid, AcceptedBatch>,
    delivery_attempts: BTreeMap<u64, u32>,
    needs_preamble: bool,
    rules_in_system_prompt: bool,
    pending_handoff: Option<String>,
    awaiting_reply: bool,
    paused_for_day_tokens: bool,
    wanted: bool,
    opened_at_ms: u64,
    open_failures: u32,
    turns: u64,
    turn_started_ms: Option<u64>,
}

impl AgentActor {
    fn spawn(
        state: AgentState,
        store: Arc<HarborStore>,
        shared_state: Arc<Mutex<HarborState>>,
        factory: Arc<dyn SessionFactory>,
        config: HarborDaemonConfig,
    ) -> HarborAgentHandle {
        let now_ms = timestamp_ms();
        let budget = AgentBudget::new(config.recycle, config.day_tokens, now_ms);
        let auditor = ContractAuditor::new(state.name.clone());
        let paused = budget
            .is_day_paused(state.day_tokens.as_ref(), now_ms)
            .is_some();
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Self {
            state,
            store,
            shared_state,
            factory,
            config,
            budget,
            auditor,
            session: None,
            recorder: None,
            record_sub: None,
            event_sub: None,
            inbox: VecDeque::new(),
            accepted: BTreeMap::new(),
            delivery_attempts: BTreeMap::new(),
            needs_preamble: false,
            rules_in_system_prompt: false,
            pending_handoff: None,
            awaiting_reply: false,
            paused_for_day_tokens: paused,
            wanted: true,
            opened_at_ms: now_ms,
            open_failures: 0,
            turns: 0,
            turn_started_ms: None,
        };
        tokio::spawn(actor.run(rx));
        HarborAgentHandle { tx }
    }

    fn work_cwd(&self) -> PathBuf {
        self.state
            .cwd
            .clone()
            .unwrap_or_else(|| self.store.agent_work_dir(&self.state.name))
    }

    fn persist_agent_state(&mut self) {
        let mut combined: Vec<UndeliveredItem> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for batch in self.accepted.values() {
            for item in &batch.items {
                if seen.insert(item.message.seq) {
                    combined.push(UndeliveredItem {
                        seq: item.message.seq,
                        from_person: item.from_person,
                    });
                }
            }
        }
        for item in &self.inbox {
            if seen.insert(item.message.seq) {
                combined.push(UndeliveredItem {
                    seq: item.message.seq,
                    from_person: item.from_person,
                });
            }
        }
        combined.sort_by_key(|u| u.seq);
        self.state.undelivered = combined;

        let mut guard = self.shared_state.lock().unwrap();
        guard
            .agents
            .insert(self.state.name.clone(), self.state.clone());
        let _ = self.store.save_state(&guard);
    }

    fn record_finding(&self, finding: &Finding) {
        let _ = self.store.append_journal(
            "finding",
            Some(&self.state.name),
            finding.to_journal_payload(),
        );
    }

    fn record_findings(&self, findings: &[Finding]) {
        for f in findings {
            self.record_finding(f);
        }
    }

    async fn open_now(&mut self, force_fresh: bool) -> Result<()> {
        if self.session.is_some() || !self.wanted {
            return Ok(());
        }
        let fresh = force_fresh || self.pending_handoff.is_some();
        let resume_target = if fresh {
            None
        } else {
            self.state.session_id.clone()
        };

        if let Some(resume_id) = resume_target {
            match self.open_single(Some(resume_id.clone()), None).await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let f = self.auditor.make_finding(
                        self.state.opens,
                        FindingKind::ResumeFailed,
                        json!({
                            "session_id": resume_id,
                            "error": err.to_string(),
                        }),
                    );
                    self.record_finding(&f);
                }
            }
        }

        let handoff = self.pending_handoff.clone();
        match self.open_single(None, handoff).await {
            Ok(()) => {
                self.pending_handoff = None;
                self.open_failures = 0;
                Ok(())
            }
            Err(err) => {
                self.open_failures += 1;
                let f = self.auditor.make_finding(
                    self.state.opens,
                    FindingKind::OpenFailed,
                    json!({
                        "attempt": self.open_failures,
                        "error": err.to_string(),
                    }),
                );
                self.record_finding(&f);
                if self.open_failures >= 5 {
                    let gave_up = self.auditor.make_finding(
                        self.state.opens,
                        FindingKind::GaveUp,
                        json!({ "attempts": self.open_failures }),
                    );
                    self.record_finding(&gave_up);
                }
                Err(err)
            }
        }
    }

    async fn open_single(&mut self, resume: Option<String>, handoff: Option<String>) -> Result<()> {
        let cwd = self.work_cwd();
        fs::create_dir_all(&cwd)?;
        let started_ms = timestamp_ms();
        let preamble = channel_preamble(
            &self.state.name,
            &cwd,
            &self.config.harbor_bin_hint,
            self.store.dir(),
        );

        let build_opts = |with_append: bool| -> SessionOptions {
            let mut opts = SessionOptions::new(&cwd);
            opts.resume = resume.clone();
            opts.model = self.state.model.clone();
            if with_append {
                opts.settings.append_system_prompt = Some(preamble.clone());
            }
            for (k, v) in &self.state.env {
                opts.env.insert(OsString::from(k), OsString::from(v));
            }
            opts
        };

        let (session, declared, rules_in_prompt) = match self
            .factory
            .open_agent(&self.state.runtime, build_opts(true))
            .await
        {
            Ok((s, decl)) => (s, decl, true),
            Err(Error::Unsupported(reason)) => {
                let _ = self.store.append_journal(
                    "rules_in_messages",
                    Some(&self.state.name),
                    json!({ "refusal": reason }),
                );
                let (s, decl) = self
                    .factory
                    .open_agent(&self.state.runtime, build_opts(false))
                    .await?;
                (s, decl, false)
            }
            Err(err) => return Err(err),
        };

        let open_num = self.state.opens + 1;
        let voyage_rel = self.store.voyage_rel_path(&self.state.name, open_num);
        let voyage_abs = self.store.voyage_path(&self.state.name, open_num);
        let header = VoyageHeader::new(
            &self.state.runtime,
            cwd.clone(),
            session.id(),
            session.stream_id(),
        )
        .with_model(session.model().or_else(|| self.state.model.clone()))
        .with_recorder("kar-harbor");
        let mut recorder = VoyageRecorder::create(&voyage_abs, &header)?;

        let mut record_sub = session.subscribe(None)?;
        while let Some(rec) = record_sub.try_next() {
            let _ = recorder.record(&rec);
        }
        let event_sub = session.events(None)?;

        self.auditor.set_voyage(Some(voyage_rel.clone()));
        let mismatch_findings =
            self.auditor
                .audit_opened(open_num, &session.capabilities(), declared);
        self.record_findings(&mismatch_findings);

        let now_ms = timestamp_ms();
        self.budget.on_session_opened(&session.usage(), now_ms);
        self.opened_at_ms = now_ms;
        self.turns = 0;
        self.turn_started_ms = None;

        self.state.opens = open_num;
        self.state.session_id = Some(session.id().to_owned());
        if resume.is_none() {
            self.state.context_tokens = None;
        }
        self.rules_in_system_prompt = rules_in_prompt;
        self.needs_preamble = !rules_in_prompt && (self.needs_preamble || resume.is_none());
        if handoff.is_some() {
            self.pending_handoff = handoff;
        }

        let session_id = session.id().to_owned();
        self.session = Some(session);
        self.recorder = Some(recorder);
        self.record_sub = Some(record_sub);
        self.event_sub = Some(event_sub);
        self.persist_agent_state();

        let _ = self.store.append_journal(
            if resume.is_none() {
                "opened"
            } else {
                "resumed"
            },
            Some(&self.state.name),
            json!({
                "session_id": session_id,
                "opens": open_num,
                "ms": now_ms.saturating_sub(started_ms),
                "voyage": voyage_rel,
            }),
        );
        Ok(())
    }

    fn flush_remaining_records(&mut self) {
        if let (Some(sub), Some(rec)) = (&mut self.record_sub, &mut self.recorder) {
            while let Some(r) = sub.try_next() {
                let _ = rec.record(&r);
            }
        }
    }

    fn detach_session(&mut self, why: &str) -> Option<Session> {
        self.flush_remaining_records();
        if let Some(sess) = &self.session {
            let now_ms = timestamp_ms();
            let (day_count, _) =
                self.budget
                    .record_usage(&sess.usage(), self.state.day_tokens.as_ref(), now_ms);
            self.state.day_tokens = Some(day_count);
            if let Some(ctx) = sess.snapshot().context_usage.and_then(|c| c.tokens) {
                self.state.context_tokens = Some(ctx);
            }
        }
        if let Some(recorder) = self.recorder.take() {
            let _ = recorder.finish_with_reason(why);
        }
        self.record_sub = None;
        self.event_sub = None;
        self.turn_started_ms = None;

        // Re-queue any batches accepted by the detached session that never finished consuming!
        if !self.accepted.is_empty() {
            let mut unconsumed: Vec<QueuedMessage> = std::mem::take(&mut self.accepted)
                .into_values()
                .flat_map(|b| b.items)
                .collect();
            unconsumed.sort_by_key(|m| m.message.seq);
            let seqs: Vec<u64> = unconsumed.iter().map(|m| m.message.seq).collect();
            let _ = self.store.append_journal(
                "redelivering",
                Some(&self.state.name),
                json!({ "why": why, "seqs": seqs }),
            );
            for item in unconsumed.into_iter().rev() {
                self.inbox.push_front(item);
            }
        }
        self.persist_agent_state();
        self.session.take()
    }

    async fn close_now(&mut self, why: &str) {
        let Some(session) = self.detach_session(why) else {
            return;
        };
        if let Err(err) = session.close().await {
            let f = self.auditor.make_finding(
                self.state.opens,
                FindingKind::DisposeFailed,
                json!({ "error": err.to_string() }),
            );
            self.record_finding(&f);
        }
        if let Some(leftover) = self
            .auditor
            .audit_leftover_processes(self.state.opens, &self.work_cwd())
        {
            self.record_finding(&leftover);
        }
    }

    fn sync_day_budget_pause(&mut self, now_ms: u64) -> bool {
        if let Some(wait) = self
            .budget
            .is_day_paused(self.state.day_tokens.as_ref(), now_ms)
        {
            if !self.paused_for_day_tokens {
                self.paused_for_day_tokens = true;
                let _ = self.store.append_journal(
                    "limit",
                    Some(&self.state.name),
                    json!({
                        "limit": "day_tokens",
                        "tokens": self.state.day_tokens.as_ref().map(|d| d.tokens),
                        "max": self.budget.day_limit,
                        "resume_in_ms": wait.as_millis() as u64,
                    }),
                );
            }
            true
        } else {
            if self.paused_for_day_tokens {
                self.paused_for_day_tokens = false;
                let _ = self.store.append_journal(
                    "resumed_after_limit",
                    Some(&self.state.name),
                    json!({ "limit": "day_tokens" }),
                );
            }
            false
        }
    }

    async fn pump_inbox(&mut self) {
        while !self.inbox.is_empty() && self.wanted {
            let now_ms = timestamp_ms();
            if self.sync_day_budget_pause(now_ms) {
                return;
            }
            if self.session.is_none() && self.open_now(false).await.is_err() {
                return;
            }
            let Some(session) = self.session.clone() else {
                return;
            };

            // Check idle+context session recycling before starting a new delivery on an idle session!
            if !session.status().is_running() {
                let ctx_tokens = session
                    .snapshot()
                    .context_usage
                    .and_then(|c| c.tokens)
                    .or(self.state.context_tokens);
                if self.budget.should_recycle_now(ctx_tokens, now_ms) {
                    let next_seq = self.inbox.front().map_or(u64::MAX, |item| item.message.seq);
                    let handoff = self.store.messages().ok().and_then(|msgs| {
                        build_handoff(&msgs, &self.state.name, next_seq, HANDOFF_MAX_CHARS)
                    });
                    let _ = self.store.append_journal(
                        "recycle",
                        Some(&self.state.name),
                        json!({
                            "session": self.state.opens,
                            "idle_ms": self.budget.idle_ms(now_ms),
                            "context_tokens": ctx_tokens,
                        }),
                    );
                    self.pending_handoff = handoff;
                    self.close_now("recycle").await;
                    if self.open_now(true).await.is_err() {
                        return;
                    }
                    continue;
                }
            }

            let batch: Vec<QueuedMessage> = self.inbox.drain(..).collect();
            let mut sections: Vec<String> = Vec::new();
            if self.needs_preamble {
                sections.push(channel_preamble(
                    &self.state.name,
                    &self.work_cwd(),
                    &self.config.harbor_bin_hint,
                    self.store.dir(),
                ));
            }
            if let Some(handoff) = self.pending_handoff.clone() {
                sections.push(handoff);
            }
            sections.extend(batch.iter().map(|q| format_channel_message(&q.message)));
            let payload = sections.join("\n\n");
            let input_id = Uuid::new_v4();

            match session.deliver_with_id(input_id, payload).await {
                Ok(delivery) => {
                    self.needs_preamble = false;
                    self.pending_handoff = None;
                    let landed_queued = matches!(delivery, Delivery::Queued { .. });
                    let landed_name = match &delivery {
                        Delivery::Started(_) => "prompted",
                        Delivery::Steered => "steered",
                        Delivery::Queued { .. } => "queued",
                    };
                    if batch.iter().any(|q| q.from_person) {
                        self.awaiting_reply = true;
                    }
                    let seqs: Vec<u64> = batch.iter().map(|q| q.message.seq).collect();
                    let _ = self.store.append_journal(
                        "delivered",
                        Some(&self.state.name),
                        json!({
                            "session": self.state.opens,
                            "input_id": input_id,
                            "landed": landed_name,
                            "seqs": seqs,
                        }),
                    );
                    self.accepted.insert(
                        input_id,
                        AcceptedBatch {
                            items: batch,
                            landed_queued,
                        },
                    );
                    self.persist_agent_state();
                }
                Err(err) => {
                    let mut retryable = Vec::new();
                    for item in batch {
                        let count = self
                            .delivery_attempts
                            .entry(item.message.seq)
                            .and_modify(|c| *c += 1)
                            .or_insert(1);
                        if *count >= 5 {
                            let f = self.auditor.make_finding(
                                self.state.opens,
                                FindingKind::InputDropped,
                                json!({
                                    "seq": item.message.seq,
                                    "attempts": *count,
                                    "error": err.to_string(),
                                }),
                            );
                            self.record_finding(&f);
                        } else {
                            retryable.push(item);
                        }
                    }
                    for item in retryable.into_iter().rev() {
                        self.inbox.push_front(item);
                    }
                    self.persist_agent_state();
                    return;
                }
            }
        }
    }

    async fn handle_observed_event(&mut self, ev: crate::ObservedEvent) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let snap = session.snapshot();
        if matches!(ev.event, Event::TokenUsage { .. } | Event::Usage { .. }) {
            let now_ms = timestamp_ms();
            let (day_count, _) =
                self.budget
                    .record_usage(&snap.usage, self.state.day_tokens.as_ref(), now_ms);
            self.state.day_tokens = Some(day_count);
            if let Some(ctx) = snap.context_usage.as_ref().and_then(|c| c.tokens) {
                self.state.context_tokens = Some(ctx);
            }
            let _ = self.sync_day_budget_pause(now_ms);
            self.persist_agent_state();
        }

        if !ev.agent_path.is_empty() {
            return;
        }

        let findings = self.auditor.audit_event(
            self.state.opens,
            &ev,
            &snap.usage,
            snap.context_usage.as_ref(),
            snap.last_failure.as_ref(),
        );
        self.record_findings(&findings);

        match &ev.event {
            Event::TurnStarted => {
                self.turns += 1;
                self.turn_started_ms = Some(ev.timestamp_ms);
                let _ = self.store.append_journal(
                    "turn_started",
                    Some(&self.state.name),
                    json!({
                        "session": self.state.opens,
                        "turn": self.turns,
                        "seq": ev.cursor.seq,
                    }),
                );
            }
            Event::Compaction { state } if state != "started" => {
                self.needs_preamble = !self.rules_in_system_prompt;
            }
            Event::TurnEnded { outcome } => {
                let now_ms = timestamp_ms();
                let duration_ms = self
                    .turn_started_ms
                    .take()
                    .map(|s| ev.timestamp_ms.saturating_sub(s));
                let (day_count, _) =
                    self.budget
                        .on_turn_ended(&snap.usage, self.state.day_tokens.as_ref(), now_ms);
                self.state.day_tokens = Some(day_count);
                if let Some(ctx) = snap.context_usage.as_ref().and_then(|c| c.tokens) {
                    self.state.context_tokens = Some(ctx);
                }
                let _ = self.sync_day_budget_pause(now_ms);

                if !snap.status.is_running() {
                    self.accepted.clear();
                } else {
                    self.accepted.retain(|_, b| b.landed_queued);
                }
                if *outcome == crate::Outcome::Failed {
                    self.awaiting_reply = false;
                }
                self.persist_agent_state();

                let _ = self.store.append_journal(
                    "turn_ended",
                    Some(&self.state.name),
                    json!({
                        "session": self.state.opens,
                        "turn": self.turns,
                        "seq": ev.cursor.seq,
                        "outcome": outcome,
                        "ms": duration_ms,
                    }),
                );
                self.pump_inbox().await;
            }
            Event::InputDropped { input_id } => {
                let parsed = Uuid::parse_str(input_id).ok();
                let dropped_batch = parsed.and_then(|id| self.accepted.remove(&id));
                let seqs: Vec<u64> = dropped_batch
                    .as_ref()
                    .map(|b| b.items.iter().map(|i| i.message.seq).collect())
                    .unwrap_or_default();
                let f = self.auditor.make_finding(
                    self.state.opens,
                    FindingKind::InputDropped,
                    json!({
                        "seq": ev.cursor.seq,
                        "input_id": input_id,
                        "seqs": seqs,
                    }),
                );
                self.record_finding(&f);
                if let Some(batch) = dropped_batch {
                    for item in batch.items.into_iter().rev() {
                        self.inbox.push_front(item);
                    }
                    self.persist_agent_state();
                    self.pump_inbox().await;
                }
            }
            Event::Exited { reason } => {
                let now_ms = timestamp_ms();
                let (exit_findings, force_fresh) = self.auditor.audit_unexpected_exit(
                    self.state.opens,
                    ev.cursor.seq,
                    reason,
                    self.opened_at_ms,
                    now_ms,
                );
                self.record_findings(&exit_findings);
                let _ = self.detach_session("unexpected_exit");
                if self.wanted {
                    let _ = self.open_now(force_fresh).await;
                    self.pump_inbox().await;
                }
            }
            _ => {}
        }
    }

    fn build_status(&self, now_ms: u64) -> AgentStatusRow {
        let state = match &self.session {
            None if self.paused_for_day_tokens => "paused_budget".to_owned(),
            None => "closed".to_owned(),
            Some(sess) => {
                if self.paused_for_day_tokens {
                    "paused_budget".to_owned()
                } else if sess.status().is_running() {
                    "running".to_owned()
                } else {
                    "idle".to_owned()
                }
            }
        };
        let active_turn_ms = self
            .turn_started_ms
            .map(|started| now_ms.saturating_sub(started));
        let context_tokens = self
            .session
            .as_ref()
            .and_then(|s| s.snapshot().context_usage.and_then(|c| c.tokens))
            .or(self.state.context_tokens);
        let day_tokens = self.state.day_tokens.as_ref().map(|d| d.tokens);
        AgentStatusRow {
            name: self.state.name.clone(),
            runtime: self.state.runtime.clone(),
            model: self.state.model.clone(),
            state,
            opens: self.state.opens,
            turns: self.turns,
            active_turn_ms,
            context_tokens,
            day_tokens,
            pending: self.inbox.len(),
            awaiting_reply: self.awaiting_reply,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<AgentCmd>) {
        loop {
            let rec_fut = futures_util::future::OptionFuture::from(
                self.record_sub.as_mut().map(Subscription::next),
            );
            let ev_fut = futures_util::future::OptionFuture::from(
                self.event_sub.as_mut().map(EventSubscription::next),
            );

            tokio::select! {
                biased;
                Some(maybe_rec) = rec_fut => {
                    match maybe_rec {
                        Some(rec) => {
                            if let Some(recorder) = &mut self.recorder {
                                let _ = recorder.record(&rec);
                            }
                        }
                        None => {
                            self.record_sub = None;
                        }
                    }
                }
                Some(maybe_ev) = ev_fut => {
                    match maybe_ev {
                        Some(ev) => self.handle_observed_event(ev).await,
                        None => {
                            self.event_sub = None;
                        }
                    }
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else {
                        self.wanted = false;
                        self.close_now("channel_dropped").await;
                        break;
                    };
                    match cmd {
                        AgentCmd::Open { fresh, reply } => {
                            let res = self.open_now(fresh).await;
                            if res.is_ok() {
                                self.pump_inbox().await;
                            }
                            let _ = reply.send(res);
                        }
                        AgentCmd::Deliver { message, from_person } => {
                            self.inbox.push_back(QueuedMessage { message, from_person });
                            self.persist_agent_state();
                            self.pump_inbox().await;
                        }
                        AgentCmd::Restore { undelivered, messages } => {
                            let by_seq: BTreeMap<u64, HarborMessage> =
                                messages.into_iter().map(|m| (m.seq, m)).collect();
                            let mut restored = Vec::new();
                            for item in undelivered {
                                if let Some(msg) = by_seq.get(&item.seq) {
                                    restored.push(QueuedMessage {
                                        message: msg.clone(),
                                        from_person: item.from_person,
                                    });
                                }
                            }
                            if !restored.is_empty() {
                                let seqs: Vec<u64> = restored.iter().map(|r| r.message.seq).collect();
                                let _ = self.store.append_journal(
                                    "redelivering",
                                    Some(&self.state.name),
                                    json!({ "why": "harbor restarted", "seqs": seqs }),
                                );
                                for item in restored.into_iter().rev() {
                                    self.inbox.push_front(item);
                                }
                                self.persist_agent_state();
                                self.pump_inbox().await;
                            }
                        }
                        AgentCmd::Replied => {
                            self.awaiting_reply = false;
                        }
                        AgentCmd::Restart { fresh, reply } => {
                            let _ = self.store.append_journal(
                                "restart",
                                Some(&self.state.name),
                                json!({ "fresh": fresh }),
                            );
                            self.close_now("restart").await;
                            self.wanted = true;
                            let res = self.open_now(fresh).await.map(|()| {
                                json!({
                                    "agent": self.state.name,
                                    "opens": self.state.opens,
                                    "session_id": self.state.session_id,
                                })
                            });
                            if res.is_ok() {
                                self.pump_inbox().await;
                            }
                            let _ = reply.send(res);
                        }
                        AgentCmd::Op { op, arg, reply } => {
                            if let Some(sess) = self.session.clone() {
                                let (body, findings) = self
                                    .auditor
                                    .execute_op(
                                        self.state.opens,
                                        &sess,
                                        op,
                                        arg.as_deref(),
                                        self.config.abort_settle_timeout,
                                    )
                                    .await;
                                self.record_findings(&findings);
                                let _ = self.store.append_journal(
                                    "op",
                                    Some(&self.state.name),
                                    body.clone(),
                                );
                                let _ = reply.send(body);
                            } else {
                                let _ = reply.send(json!({
                                    "op": op.as_str(),
                                    "result": "no_session",
                                }));
                            }
                        }
                        AgentCmd::Status { now_ms, reply } => {
                            let _ = reply.send(self.build_status(now_ms));
                        }
                        AgentCmd::Tick { now_ms } => {
                            if let Some(sess) = &self.session {
                                let status = sess.status();
                                if let Some(finding) = self.auditor.audit_stall(
                                    self.state.opens,
                                    &status,
                                    now_ms,
                                    self.config.stall_timeout,
                                ) {
                                    self.record_finding(&finding);
                                }
                            }
                            let was_paused = self.paused_for_day_tokens;
                            if !self.sync_day_budget_pause(now_ms) && was_paused {
                                self.pump_inbox().await;
                            }
                        }
                        AgentCmd::Close { why, reply } => {
                            self.wanted = false;
                            self.close_now(&why).await;
                            let _ = reply.send(());
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Live in-process handle to a running [`HarborDaemon`].
#[derive(Clone)]
pub struct HarborDaemon {
    inner: Arc<DaemonInner>,
    endpoint: DaemonEndpoint,
}

struct DaemonInner {
    store: Arc<HarborStore>,
    shared_state: Arc<Mutex<HarborState>>,
    agents: tokio::sync::RwLock<BTreeMap<String, HarborAgentHandle>>,
    factory: Arc<dyn SessionFactory>,
    config: HarborDaemonConfig,
    token: String,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Deserialize)]
struct SendRequest {
    from: String,
    text: String,
}

#[derive(Serialize, Deserialize)]
pub struct SendReceipt {
    pub seq: u64,
    pub to: Vec<String>,
}

#[derive(Deserialize)]
struct AddRequest {
    spec: String,
}

#[derive(Deserialize)]
struct RestartRequest {
    agent: String,
    #[serde(default)]
    fresh: bool,
}

#[derive(Deserialize)]
struct OpRequest {
    agent: String,
    op: HarborOp,
    #[serde(default)]
    arg: Option<String>,
}

impl HarborDaemon {
    /// Start a Harbor daemon with the production [`NativeSessionFactory`].
    pub async fn start_native(
        store: Arc<HarborStore>,
        config: HarborDaemonConfig,
    ) -> Result<(Self, tokio::task::JoinHandle<Result<()>>)> {
        Self::start_with_factory(store, config, Arc::new(NativeSessionFactory)).await
    }

    /// Start a Harbor daemon with a custom [`SessionFactory`] (e.g. [`ScriptedSessionFactory`]).
    pub async fn start_with_factory(
        store: Arc<HarborStore>,
        config: HarborDaemonConfig,
        factory: Arc<dyn SessionFactory>,
    ) -> Result<(Self, tokio::task::JoinHandle<Result<()>>)> {
        if let Some(used) = exceeds_disk_limit(store.dir(), config.max_disk_bytes)? {
            return Err(Error::Refused(format!(
                "Harbor directory {} uses {used} bytes, exceeding `--max-disk-gb` budget",
                store.dir().display()
            )));
        }

        let mut persisted = store.load_state()?;
        for spec in &config.agents {
            let env = config
                .agent_env
                .get(&spec.name)
                .cloned()
                .unwrap_or_default();
            persisted
                .agents
                .entry(spec.name.clone())
                .and_modify(|existing| {
                    existing.runtime = spec.runtime.clone();
                    if spec.model.is_some() {
                        existing.model = spec.model.clone();
                    }
                    for (k, v) in &env {
                        existing.env.insert(k.clone(), v.clone());
                    }
                })
                .or_insert_with(|| AgentState::from_spec(spec.clone(), env));
        }
        store.save_state(&persisted)?;

        let initial_undelivered: BTreeMap<String, Vec<UndeliveredItem>> = persisted
            .agents
            .iter()
            .map(|(k, a)| (k.clone(), a.undelivered.clone()))
            .collect();
        let shared_state = Arc::new(Mutex::new(persisted.clone()));

        let mut handles = BTreeMap::new();
        for (name, agent_state) in persisted.agents {
            let handle = AgentActor::spawn(
                agent_state,
                Arc::clone(&store),
                Arc::clone(&shared_state),
                Arc::clone(&factory),
                config.clone(),
            );
            handles.insert(name, handle);
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let token = Uuid::new_v4().to_string();
        let endpoint = DaemonEndpoint {
            pid: std::process::id(),
            port,
            token: token.clone(),
            started_ms: timestamp_ms(),
        };
        store.write_daemon_endpoint(&endpoint)?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let inner = Arc::new(DaemonInner {
            store: Arc::clone(&store),
            shared_state,
            agents: tokio::sync::RwLock::new(handles),
            factory,
            config: config.clone(),
            token,
            shutdown_tx,
        });

        let daemon = Self {
            inner: Arc::clone(&inner),
            endpoint: endpoint.clone(),
        };

        // Open every registered agent and redeliver any crash-interrupted inputs!
        let existing_messages = store.messages()?;
        {
            let map = daemon.inner.agents.read().await;
            for (name, handle) in map.iter() {
                let _ = handle.open(false).await;
                if let Some(items) = initial_undelivered.get(name)
                    && !items.is_empty()
                {
                    handle.restore(items.clone(), existing_messages.clone());
                }
            }
        }

        let _ = store.append_journal(
            "daemon_started",
            None,
            json!({
                "pid": endpoint.pid,
                "port": endpoint.port,
            }),
        );

        let join = tokio::spawn(run_daemon_loop(inner, listener, shutdown_rx));
        Ok((daemon, join))
    }

    pub fn endpoint(&self) -> &DaemonEndpoint {
        &self.endpoint
    }

    /// Post a message to `channel.jsonl` and route via `@agent` / `@all` to target agents.
    pub async fn send(
        &self,
        from: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<SendReceipt> {
        self.inner.send_message(from.into(), text.into()).await
    }

    /// Query live status of every registered Harbor agent.
    pub async fn status(&self) -> Vec<AgentStatusRow> {
        self.inner.collect_status().await
    }

    /// Dynamically add and open a new agent in the running daemon.
    pub async fn add_agent(&self, spec: AgentSpec, env: BTreeMap<String, String>) -> Result<Value> {
        self.inner.add_agent(spec, env).await
    }

    /// Restart one agent (`fresh = true` discards saved resume session ID).
    pub async fn restart_agent(&self, agent: &str, fresh: bool) -> Result<Value> {
        self.inner.restart_agent(agent, fresh).await
    }

    /// Execute a QA [`HarborOp`] on one agent and audit SDK promises.
    pub async fn op(&self, agent: &str, op: HarborOp, arg: Option<String>) -> Result<Value> {
        self.inner.run_op(agent, op, arg).await
    }

    /// Trigger graceful shutdown of all agents and the loopback control server.
    pub fn request_shutdown(&self) {
        let _ = self.inner.shutdown_tx.send(true);
    }
}

impl DaemonInner {
    async fn send_message(&self, from: String, text: String) -> Result<SendReceipt> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Err(Error::Refused("cannot post an empty Harbor message".into()));
        }
        let map = self.agents.read().await;
        let known: Vec<String> = map.keys().cloned().collect();
        let recipients = route_recipients(&from, &text, &known);
        let from_person = !known.iter().any(|k| k == &from);

        let message = self
            .store
            .append_message(from.clone(), recipients.clone(), text)?;
        if let Some(sender_agent) = map.get(&from) {
            sender_agent.replied();
        }
        for target in &recipients {
            if let Some(agent) = map.get(target) {
                agent.deliver(message.clone(), from_person);
            }
        }
        Ok(SendReceipt {
            seq: message.seq,
            to: recipients,
        })
    }

    async fn collect_status(&self) -> Vec<AgentStatusRow> {
        let now_ms = timestamp_ms();
        let map = self.agents.read().await;
        let mut rows = Vec::with_capacity(map.len());
        for handle in map.values() {
            if let Some(row) = handle.status(now_ms).await {
                rows.push(row);
            }
        }
        rows
    }

    async fn add_agent(&self, spec: AgentSpec, env: BTreeMap<String, String>) -> Result<Value> {
        {
            let map = self.agents.read().await;
            if map.contains_key(&spec.name) {
                return Err(Error::Refused(format!(
                    "agent {:?} is already registered",
                    spec.name
                )));
            }
        }
        let name = spec.name.clone();
        let state = AgentState::from_spec(spec, env);
        {
            let mut shared = self.shared_state.lock().unwrap();
            shared.agents.insert(name.clone(), state.clone());
            self.store.save_state(&shared)?;
        }
        let handle = AgentActor::spawn(
            state,
            Arc::clone(&self.store),
            Arc::clone(&self.shared_state),
            Arc::clone(&self.factory),
            self.config.clone(),
        );
        handle.open(false).await?;
        self.agents.write().await.insert(name.clone(), handle);
        Ok(json!({ "added": name }))
    }

    async fn restart_agent(&self, agent: &str, fresh: bool) -> Result<Value> {
        let handle = self
            .agents
            .read()
            .await
            .get(agent)
            .cloned()
            .ok_or_else(|| Error::Refused(format!("unknown Harbor agent {agent:?}")))?;
        handle.restart(fresh).await
    }

    async fn run_op(&self, agent: &str, op: HarborOp, arg: Option<String>) -> Result<Value> {
        let handle = self
            .agents
            .read()
            .await
            .get(agent)
            .cloned()
            .ok_or_else(|| Error::Refused(format!("unknown Harbor agent {agent:?}")))?;
        handle.op(op, arg).await
    }

    async fn close_all(&self, why: &str) {
        let handles: Vec<HarborAgentHandle> = self.agents.read().await.values().cloned().collect();
        for handle in handles {
            handle.close(why).await;
        }
        self.store.remove_daemon_endpoint();
        let _ = self
            .store
            .append_journal("daemon_stopped", None, json!({ "why": why }));
    }
}

fn check_bearer(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .is_some_and(|tok| tok == expected)
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

async fn http_send(
    State(inner): State<Arc<DaemonInner>>,
    headers: HeaderMap,
    Json(req): Json<SendRequest>,
) -> Response {
    if !check_bearer(&headers, &inner.token) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match inner.send_message(req.from, req.text).await {
        Ok(receipt) => (StatusCode::OK, Json(json!(receipt))).into_response(),
        Err(err) => json_err(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn http_status(State(inner): State<Arc<DaemonInner>>, headers: HeaderMap) -> Response {
    if !check_bearer(&headers, &inner.token) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let rows = inner.collect_status().await;
    (StatusCode::OK, Json(json!({ "agents": rows }))).into_response()
}

async fn http_add(
    State(inner): State<Arc<DaemonInner>>,
    headers: HeaderMap,
    Json(req): Json<AddRequest>,
) -> Response {
    if !check_bearer(&headers, &inner.token) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let spec = match AgentSpec::parse(&req.spec) {
        Ok(s) => s,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let env = inner
        .config
        .agent_env
        .get(&spec.name)
        .cloned()
        .unwrap_or_default();
    match inner.add_agent(spec, env).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn http_restart(
    State(inner): State<Arc<DaemonInner>>,
    headers: HeaderMap,
    Json(req): Json<RestartRequest>,
) -> Response {
    if !check_bearer(&headers, &inner.token) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match inner.restart_agent(&req.agent, req.fresh).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn http_op(
    State(inner): State<Arc<DaemonInner>>,
    headers: HeaderMap,
    Json(req): Json<OpRequest>,
) -> Response {
    if !check_bearer(&headers, &inner.token) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match inner.run_op(&req.agent, req.op, req.arg).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn http_down(State(inner): State<Arc<DaemonInner>>, headers: HeaderMap) -> Response {
    if !check_bearer(&headers, &inner.token) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let _ = inner.shutdown_tx.send(true);
    (StatusCode::OK, Json(json!({ "stopping": true }))).into_response()
}

async fn run_daemon_loop(
    inner: Arc<DaemonInner>,
    listener: tokio::net::TcpListener,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let app = Router::new()
        .route("/send", post(http_send))
        .route("/status", get(http_status).post(http_status))
        .route("/add", post(http_add))
        .route("/restart", post(http_restart))
        .route("/op", post(http_op))
        .route("/down", post(http_down))
        .with_state(Arc::clone(&inner));

    let watchdog_inner = Arc::clone(&inner);
    let mut watchdog_shutdown = shutdown_rx.clone();
    let deadline = watchdog_inner
        .config
        .max_duration
        .map(|d| tokio::time::Instant::now() + d);
    let interval_dur = watchdog_inner
        .config
        .watchdog_interval
        .max(Duration::from_millis(20));

    let watchdog = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval_dur);
        loop {
            tokio::select! {
                biased;
                changed = watchdog_shutdown.changed() => {
                    if changed.is_err() || *watchdog_shutdown.borrow() {
                        return "down";
                    }
                }
                _ = ticker.tick() => {
                    if let Some(limit_instant) = deadline
                        && tokio::time::Instant::now() >= limit_instant
                    {
                        let _ = watchdog_inner.store.append_journal(
                            "limit",
                            None,
                            json!({ "limit": "for_duration" }),
                        );
                        let _ = watchdog_inner.shutdown_tx.send(true);
                        return "duration_elapsed";
                    }
                    if let Ok(Some(used_bytes)) = exceeds_disk_limit(
                        watchdog_inner.store.dir(),
                        watchdog_inner.config.max_disk_bytes,
                    ) {
                        let _ = watchdog_inner.store.append_journal(
                            "limit",
                            None,
                            json!({
                                "limit": "max_disk_bytes",
                                "used_bytes": used_bytes,
                                "max_bytes": watchdog_inner.config.max_disk_bytes,
                            }),
                        );
                        let _ = watchdog_inner.shutdown_tx.send(true);
                        return "disk_limit_exceeded";
                    }
                    let now_ms = timestamp_ms();
                    let handles: Vec<HarborAgentHandle> = watchdog_inner
                        .agents
                        .read()
                        .await
                        .values()
                        .cloned()
                        .collect();
                    for h in handles {
                        h.tick(now_ms);
                    }
                }
            }
        }
    });

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            if shutdown_rx.changed().await.is_err() {
                break;
            }
        }
    });

    let serve_res = server.await;
    let _ = inner.shutdown_tx.send(true);
    let stop_reason = watchdog.await.unwrap_or("down");
    inner.close_all(stop_reason).await;
    serve_res.map_err(Error::Io)
}
