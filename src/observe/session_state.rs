//! Root session queries. The same reducer reads live records, recordings and imported Voyages.
use crate::{Cursor, Direction, Error, Event, Outcome, Record, RecordKind, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    WaitingModel,
    Thinking,
    Responding,
    Compacting,
    Tool {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        writing: bool,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentStatus {
    Idle {
        last_outcome: Option<Outcome>,
        released: bool,
    },
    Running {
        since_seq: u64,
        since_ms: u64,
        last_event_ms: u64,
        phase: Phase,
    },
}
impl AgentStatus {
    /// Wall-clock silence for a running turn at or beyond the threshold, including during replay.
    pub fn silent_for(&self, now_ms: u64, threshold: std::time::Duration) -> Option<u64> {
        let Self::Running { last_event_ms, .. } = self else {
            return None;
        };
        let elapsed = now_ms.checked_sub(*last_event_ms)?;
        (std::time::Duration::from_millis(elapsed) >= threshold).then_some(elapsed)
    }
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionEdge {
    pub parent: String,
    pub child: String,
}
/// Edges identify native child sessions created or addressed through a tool call.
/// Agent paths within a session are separate and never create graph nodes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGraph {
    pub nodes: BTreeSet<String>,
    pub edges: BTreeSet<SessionEdge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub cursor: Option<Cursor>,
    pub status: AgentStatus,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    /// Most recent usage report with its native accounting semantics.
    pub last_usage: Option<Value>,
    #[serde(default)]
    pub usage: crate::SessionUsage,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub child_usage: BTreeMap<String, crate::SessionUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<crate::ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction: Option<crate::CompactionReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_compaction: Option<crate::ManualCompactionReport>,
    /// None before observation, Some(None) after a confirmed empty goal.
    #[serde(
        default,
        deserialize_with = "crate::goal::observed",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal: Option<Option<crate::GoalSnapshot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<crate::TurnFailure>,
    #[serde(
        default,
        deserialize_with = "crate::stream::present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_structured_output: Option<Value>,
    pub graph: SessionGraph,
    /// Pending host interactions for this stream, including attributed children.
    #[serde(default)]
    pub pending_requests: Vec<crate::PendingRequest>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_startup: BTreeMap<String, crate::McpStartupStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<crate::NativeTask>,
}
impl SessionSnapshot {
    pub(crate) fn into_readback<T>(self, project: impl FnOnce(Self) -> T) -> super::Readback<T> {
        super::Readback {
            seq: self.cursor.as_ref().map(|c| c.after_seq),
            value: project(self),
        }
    }
}
struct Run {
    span: Option<String>,
    since_seq: u64,
    since_ms: u64,
    last_event_ms: u64,
    phase: Phase,
    tools: BTreeMap<String, Phase>,
}
/// Pass the persistent root id to keep child turns and nested agent activity out of root queries.
pub struct SessionReducer {
    compacting: Option<(u64, u64)>,
    ledger: crate::usage::Ledger,
    snapshot: SessionSnapshot,
    runs: Vec<Run>,
    queued: BTreeMap<String, (u64, u64)>,
    last_outcome: Option<Outcome>,
    released: bool,
}
impl SessionReducer {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            ledger: Default::default(),
            compacting: None,
            snapshot: SessionSnapshot {
                pending_requests: vec![],
                mcp_startup: BTreeMap::new(),
                graph: SessionGraph {
                    nodes: [id.clone()].into(),
                    edges: Default::default(),
                },
                id,
                cursor: None,
                status: AgentStatus::Idle {
                    last_outcome: None,
                    released: false,
                },
                model: None,
                effort: None,
                service_tier: None,
                tasks: vec![],
                last_usage: None,
                usage: Default::default(),
                child_usage: Default::default(),
                context_usage: None,
                manual_compaction: None,
                last_compaction: None,
                goal: None,
                last_failure: None,
                last_structured_output: None,
            },
            runs: vec![],
            queued: BTreeMap::new(),
            last_outcome: None,
            released: false,
        }
    }
    /// Running totals for a native session, including its own linked descendants when measured.
    pub fn usage_for(&self, session: &str) -> crate::SessionUsage {
        if session == self.snapshot.id {
            return self.snapshot.usage.clone();
        }
        self.ledger.summary(session, &self.snapshot.graph).0
    }
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }
    pub fn apply(&mut self, r: &Record) -> Result<()> {
        if !self.snapshot.cursor.as_ref().map_or(r.seq == 0, |c| {
            c.stream_id == r.stream_id && c.after_seq.checked_add(1) == Some(r.seq)
        }) {
            return Err(Error::InvalidCursor);
        }
        self.fold(r);
        Ok(())
    }
    fn run_index(&self, span: &Option<String>) -> Option<usize> {
        if let Some(span) = span {
            self.runs
                .iter()
                .position(|t| t.span.as_ref() == Some(span))
                .or_else(|| self.runs.iter().position(|t| t.span.is_none()))
        } else {
            (!self.runs.is_empty()).then_some(0)
        }
    }
    fn start(&mut self, r: &Record) {
        if let Some(i) = self.run_index(&r.span_id) {
            if self.runs[i].span.is_none() {
                self.runs[i].span = r.span_id.clone();
            }
            return;
        }
        self.runs.push(Run {
            span: r.span_id.clone(),
            since_seq: r.seq,
            since_ms: r.timestamp_ms,
            last_event_ms: r.timestamp_ms,
            phase: Phase::WaitingModel,
            tools: BTreeMap::new(),
        });
    }
    fn phase(run: &mut Run, e: &Event) -> bool {
        let phase = match e {
            Event::TextDelta { .. } | Event::Message { .. } => Phase::Responding,
            Event::ReasoningDelta { .. } => Phase::Thinking,
            Event::ToolStarted { id, name } => {
                let phase = Phase::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    writing: false,
                };
                run.tools.insert(id.clone(), phase.clone());
                phase
            }
            Event::ToolUpdated { id } => match run.tools.get(id) {
                Some(tool) => tool.clone(),
                None => return false,
            },
            Event::ToolInputDelta { id, .. } | Event::ToolInput { id, .. } => {
                if let Some(tool @ Phase::Tool { .. }) = run.tools.get_mut(id) {
                    if let Phase::Tool { writing, .. } = tool {
                        *writing = matches!(e, Event::ToolInputDelta { .. });
                    }
                    if matches!(&run.phase, Phase::Tool { id: current, .. } if current == id) {
                        run.phase = tool.clone();
                    }
                }
                return true;
            }
            Event::ToolEnded { id } => {
                run.tools.remove(id);
                if !matches!(&run.phase,Phase::Tool{id:active,..} if active==id) {
                    return false;
                }
                run.tools
                    .last_key_value()
                    .map_or(Phase::WaitingModel, |(_, tool)| tool.clone())
            }
            Event::Compaction { state } if state == "started" => Phase::Compacting,
            Event::Compaction { .. } | Event::Retry { .. } => Phase::WaitingModel,
            _ => return false,
        };
        run.phase = phase;
        true
    }
    // Early Rust recordings stored admission and queue changes only in the native record.
    fn legacy_event(r: &Record) -> Option<Event> {
        let frame = &r.native;
        if r.direction == Direction::FromRuntime
            && r.kind == RecordKind::Frame
            && frame["method"] == "mcpServer/startupStatus/updated"
            && frame["params"]["threadId"].as_str() == r.session_id.as_deref()
        {
            return Event::codex_mcp_startup(&frame["params"]);
        }
        if r.direction == Direction::ToRuntime
            && r.kind == RecordKind::Request
            && (matches!(
                frame["method"].as_str(),
                Some("turn/start" | "session/prompt")
            ) || matches!(frame["type"].as_str(), Some("user" | "prompt")))
        {
            return Some(Event::PromptDispatched);
        }
        if r.direction != Direction::Host {
            return None;
        }
        let input_id = frame["inputId"].as_str()?.to_owned();
        match frame["type"].as_str()? {
            "queue" => Some(Event::InputQueued { input_id }),
            "withdrawn" | "dequeued" => Some(Event::InputRemoved { input_id }),
            _ => None,
        }
    }
    pub(crate) fn fold(&mut self, r: &Record) {
        self.snapshot.cursor = Some(r.cursor());
        if let Some(usage) = self
            .ledger
            .observe(&self.snapshot.id, &mut self.snapshot.graph, r)
        {
            self.snapshot.usage = usage.usage;
            self.snapshot.child_usage = usage.children;
        }
        let root = r.agent_path.is_empty()
            && r.session_id
                .as_ref()
                .is_none_or(|id| id == &self.snapshot.id);
        let changes = crate::tasks::changes(&self.snapshot.tasks, r, root);
        crate::tasks::merge(&mut self.snapshot.tasks, changes);

        // Older Rust recordings carry the outgoing response or cancellation only in the native frame.
        let resolved = if r.direction == Direction::ToRuntime && r.kind == RecordKind::Response {
            r.native
                .pointer("/response/request_id")
                .or_else(|| r.native.get("id"))
        } else if r.direction == Direction::FromRuntime
            && r.native["type"] == "control_cancel_request"
        {
            r.native.get("request_id")
        } else if r.direction == Direction::FromRuntime
            && r.native["method"] == "serverRequest/resolved"
        {
            r.native.pointer("/params/requestId")
        } else {
            None
        };
        if let Some(id) = resolved {
            self.snapshot.pending_requests.retain(|p| &p.id != id);
        }
        if root && let Some(report) = r.manual_compaction() {
            if report.status.is_pending() {
                self.compacting
                    .get_or_insert((report.request.after_seq, r.timestamp_ms));
            } else {
                self.compacting = None;
            }
            self.snapshot.manual_compaction = Some(report);
        }
        if root && let Some(report) = r.compactions().last() {
            self.snapshot.last_compaction = Some(report);
        }
        let legacy = if root && r.events.is_empty() {
            Self::legacy_event(r)
        } else {
            None
        };
        crate::interaction::update_pending(&mut self.snapshot.pending_requests, r, root);
        for e in r.events.iter().chain(legacy.as_ref()) {
            if !root {
                continue;
            }
            if let Some(outcome) = e.resolved_input() {
                // A Pi extension can settle an input without starting a model run.
                if !matches!(
                    e,
                    Event::NativeInput {
                        receipt: crate::InputReceipt::Handled,
                        ..
                    }
                ) {
                    self.snapshot.last_structured_output = r.structured_output().cloned();
                    self.snapshot.last_failure = r.turn_failure().cloned();
                    self.last_outcome = Some(outcome);
                }
                if let Some(i) = self.run_index(&r.span_id) {
                    self.runs.remove(i);
                }
                continue;
            }
            match e {
                Event::ContextUsage { usage } => self.snapshot.context_usage = Some(usage.clone()),
                Event::McpServerStartup { server, status }
                    if r.session_id.as_ref() == Some(&self.snapshot.id) =>
                {
                    self.snapshot.mcp_startup.insert(server.clone(), *status);
                }
                Event::GoalChanged { goal } => self.snapshot.goal = Some(goal.clone()),
                Event::Model { model } => self.snapshot.model = Some(model.clone()),
                Event::Effort { effort } => self.snapshot.effort = Some(effort.clone()),
                Event::ServiceTier { tier } => self.snapshot.service_tier = Some(tier.clone()),
                Event::Usage { usage } => self.snapshot.last_usage = Some(usage.clone()),
                Event::TurnStarted | Event::PromptDispatched if !self.released => self.start(r),
                Event::InputQueued { input_id } => {
                    self.queued
                        .insert(input_id.clone(), (r.seq, r.timestamp_ms));
                }
                Event::InputRemoved { input_id } | Event::InputDropped { input_id } => {
                    self.queued.remove(input_id);
                }
                Event::NotificationActivity { .. } => self.start(r),
                Event::NotificationJoined { id, .. } => {
                    let group = id.to_string();
                    self.runs.retain(|run| run.span.as_ref() != Some(&group));
                }
                Event::Released | Event::Exited { .. } => {
                    self.compacting = None;
                    self.runs.clear();
                    self.queued.clear();
                    self.released = true;
                }
                _ => {
                    if let Some(i) = self.run_index(&r.span_id) {
                        let run = &mut self.runs[i];
                        if Self::phase(run, e) {
                            run.last_event_ms = r.timestamp_ms;
                        }
                    }
                }
            }
        }
        if root && (!r.events.is_empty() || legacy.is_some()) {
            if let Some((since_seq, since_ms)) = self.compacting {
                self.snapshot.status = AgentStatus::Running {
                    since_seq,
                    since_ms,
                    last_event_ms: r.timestamp_ms,
                    phase: Phase::Compacting,
                };
                return;
            }
            self.snapshot.status = self.runs.first().map_or_else(
                || match self.queued.values().min() {
                    Some(&(seq, ms)) => AgentStatus::Running {
                        since_seq: seq,
                        since_ms: ms,
                        last_event_ms: r.timestamp_ms,
                        phase: Phase::WaitingModel,
                    },
                    None => AgentStatus::Idle {
                        last_outcome: self.last_outcome.clone(),
                        released: self.released,
                    },
                },
                |t| AgentStatus::Running {
                    since_seq: t.since_seq,
                    since_ms: t.since_ms,
                    last_event_ms: t.last_event_ms,
                    phase: t.phase.clone(),
                },
            );
        }
    }
}
