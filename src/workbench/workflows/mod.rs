//! Project-bound durable workflows. An attempt is committed before an external call.
//! Only a receipt, or explicit reconciliation, allows that step to advance.
mod mailbox;
mod milestones;
pub(in crate::workbench) mod model_schedules;
pub(in crate::workbench) use mailbox::Message;
mod routes;
mod runner;
mod schedule;
mod script;
mod sources;
mod views;
mod webhook;
pub(super) use routes::{hooks, routes};
pub use schedule::Schedule;

use super::{Workbench, missing};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, atomic::AtomicBool},
};
use tokio::sync::watch;
use uuid::Uuid;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        jiff::Timestamp::now().as_millisecond()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStrategy {
    #[default]
    Local,
    Worktree,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyPolicy {
    #[default]
    Allow,
    Skip,
    CancelPrevious,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskChoice {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskLink {
    pub label: String,
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<AskChoice>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_reason: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<AskLink>,
}

impl AskMetadata {
    pub fn is_empty(&self) -> bool {
        self.kind.is_none()
            && self.choices.is_empty()
            && !self.require_reason
            && self.links.is_empty()
    }

    pub fn validate(&self) -> Result<()> {
        let non_empty = |s: &str, max: usize| {
            !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control)
        };
        if let Some(k) = &self.kind
            && !non_empty(k, 64)
        {
            return Err(Error::Refused("invalid ask question kind".into()));
        }
        if self.choices.len() > 64 {
            return Err(Error::Refused(
                "ask question choices exceed 64 options".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for c in &self.choices {
            if !non_empty(&c.id, 128)
                || !non_empty(&c.label, 256)
                || c.description.as_ref().is_some_and(|d| d.len() > 2048)
                || !seen.insert(c.id.as_str())
            {
                return Err(Error::Refused("invalid or duplicate ask choice".into()));
            }
        }
        if self.links.len() > 32 {
            return Err(Error::Refused("ask question links exceed 32 items".into()));
        }
        for l in &self.links {
            let parsed = url::Url::parse(&l.url)
                .map_err(|_| Error::Refused("invalid ask question link URL".into()))?;
            if !non_empty(&l.label, 256)
                || l.url.len() > 4096
                || !matches!(parsed.scheme(), "http" | "https")
            {
                return Err(Error::Refused("invalid ask question link".into()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub workspace_strategy: WorkspaceStrategy,
    #[serde(default)]
    pub concurrency: ConcurrencyPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl WorkflowMetadata {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> Result<()> {
        let token = |s: &str, max: usize| {
            !s.trim().is_empty()
                && s.trim() == s
                && s.len() <= max
                && !s.chars().any(char::is_control)
        };
        if self.effort.as_deref().is_some_and(|e| !token(e, 64))
            || self.service_tier.as_deref().is_some_and(|t| !token(t, 64))
            || self.tags.len() > 32
            || self.tags.iter().any(|t| !token(t, 64))
            || self.subject.as_deref().is_some_and(|s| !token(s, 256))
        {
            return Err(Error::Refused("invalid workflow metadata fields".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    Script {
        command: String,
    },
    Agent {
        runtime: String,
        model: Option<String>,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service_tier: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_strategy: Option<WorkspaceStrategy>,
    },
    Ask {
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<i64>,
    },
    WaitSignal {
        channel: String,
    },
    Wait {
        duration_ms: i64,
    },
    Workflow {
        workflow: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
    },
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Skip {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireStep {
    Script {
        command: String,
    },
    Agent {
        runtime: String,
        model: Option<String>,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service_tier: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_strategy: Option<WorkspaceStrategy>,
    },
    Ask {
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<AskChoice>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        require_reason: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        links: Vec<AskLink>,
    },
    WaitSignal {
        channel: String,
    },
    Wait {
        duration_ms: i64,
    },
    Workflow {
        workflow: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
    },
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Skip {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl WireStep {
    fn from_step(step: &Step, ask_meta: Option<&AskMetadata>) -> Self {
        match step {
            Step::Script { command } => Self::Script {
                command: command.clone(),
            },
            Step::Agent {
                runtime,
                model,
                prompt,
                effort,
                service_tier,
                workspace_strategy,
            } => Self::Agent {
                runtime: runtime.clone(),
                model: model.clone(),
                prompt: prompt.clone(),
                effort: effort.clone(),
                service_tier: service_tier.clone(),
                workspace_strategy: *workspace_strategy,
            },
            Step::Ask {
                question,
                timeout_ms,
            } => Self::Ask {
                question: question.clone(),
                timeout_ms: *timeout_ms,
                kind: ask_meta.and_then(|m| m.kind.clone()),
                choices: ask_meta.map(|m| m.choices.clone()).unwrap_or_default(),
                require_reason: ask_meta.is_some_and(|m| m.require_reason),
                links: ask_meta.map(|m| m.links.clone()).unwrap_or_default(),
            },
            Step::WaitSignal { channel } => Self::WaitSignal {
                channel: channel.clone(),
            },
            Step::Wait { duration_ms } => Self::Wait {
                duration_ms: *duration_ms,
            },
            Step::Workflow { workflow, input } => Self::Workflow {
                workflow: *workflow,
                input: input.clone(),
            },
            Step::Stop { reason } => Self::Stop {
                reason: reason.clone(),
            },
            Step::Skip { reason } => Self::Skip {
                reason: reason.clone(),
            },
        }
    }

    fn into_step(self) -> (Step, Option<AskMetadata>) {
        match self {
            Self::Script { command } => (Step::Script { command }, None),
            Self::Agent {
                runtime,
                model,
                prompt,
                effort,
                service_tier,
                workspace_strategy,
            } => (
                Step::Agent {
                    runtime,
                    model,
                    prompt,
                    effort,
                    service_tier,
                    workspace_strategy,
                },
                None,
            ),
            Self::Ask {
                question,
                timeout_ms,
                kind,
                choices,
                require_reason,
                links,
            } => {
                let meta = AskMetadata {
                    kind,
                    choices,
                    require_reason,
                    links,
                };
                (
                    Step::Ask {
                        question,
                        timeout_ms,
                    },
                    (!meta.is_empty()).then_some(meta),
                )
            }
            Self::WaitSignal { channel } => (Step::WaitSignal { channel }, None),
            Self::Wait { duration_ms } => (Step::Wait { duration_ms }, None),
            Self::Workflow { workflow, input } => (Step::Workflow { workflow, input }, None),
            Self::Stop { reason } => (Step::Stop { reason }, None),
            Self::Skip { reason } => (Step::Skip { reason }, None),
        }
    }
}

type StagedExtras = (u32, WorkflowMetadata, BTreeMap<usize, AskMetadata>);
static PARSED_DEFINITION_EXTRAS: LazyLock<Mutex<VecDeque<StagedExtras>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

fn definition_digest(def: &Definition) -> u32 {
    serde_json::to_vec(def).map_or(0, |b| crc32fast::hash(&b))
}

fn stash_definition_extras(
    def: &Definition,
    meta: WorkflowMetadata,
    ask: BTreeMap<usize, AskMetadata>,
) {
    if meta.is_default() && ask.is_empty() {
        return;
    }
    let digest = definition_digest(def);
    let mut q = PARSED_DEFINITION_EXTRAS.lock().unwrap();
    if q.len() >= 64 {
        q.pop_front();
    }
    q.push_back((digest, meta, ask));
}

fn take_definition_extras(
    def: &Definition,
) -> Option<(WorkflowMetadata, BTreeMap<usize, AskMetadata>)> {
    let digest = definition_digest(def);
    let mut q = PARSED_DEFINITION_EXTRAS.lock().unwrap();
    let pos = q.iter().rposition(|(d, _, _)| *d == digest)?;
    let (_, meta, ask) = q.remove(pos)?;
    Some((meta, ask))
}

#[derive(Clone, Debug, Serialize)]
pub struct Definition {
    pub name: String,
    pub enabled: bool,
    pub schedule: Schedule,
    pub steps: Vec<Step>,
}

impl<'de> Deserialize<'de> for Definition {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawDefinition {
            name: String,
            enabled: bool,
            schedule: Schedule,
            steps: Vec<WireStep>,
            #[serde(default)]
            effort: Option<String>,
            #[serde(default)]
            service_tier: Option<String>,
            #[serde(default)]
            workspace_strategy: WorkspaceStrategy,
            #[serde(default)]
            concurrency: ConcurrencyPolicy,
            #[serde(default)]
            tags: Vec<String>,
            #[serde(default)]
            subject: Option<String>,
        }
        let raw = RawDefinition::deserialize(deserializer)?;
        let metadata = WorkflowMetadata {
            effort: raw.effort,
            service_tier: raw.service_tier,
            workspace_strategy: raw.workspace_strategy,
            concurrency: raw.concurrency,
            tags: raw.tags,
            subject: raw.subject,
        };
        metadata.validate().map_err(serde::de::Error::custom)?;
        let mut steps = Vec::with_capacity(raw.steps.len());
        let mut ask_metadata = BTreeMap::new();
        for (idx, ws) in raw.steps.into_iter().enumerate() {
            let (step, ask) = ws.into_step();
            if let Some(m) = ask {
                m.validate().map_err(serde::de::Error::custom)?;
                ask_metadata.insert(idx, m);
            }
            steps.push(step);
        }
        let def = Self {
            name: raw.name,
            enabled: raw.enabled,
            schedule: raw.schedule,
            steps,
        };
        stash_definition_extras(&def, metadata, ask_metadata);
        Ok(def)
    }
}

impl Definition {
    fn validate(&self) -> Result<()> {
        self.validate_for(None)
    }
    fn validate_for(&self, self_id: Option<Uuid>) -> Result<()> {
        let bounded = |s: &str| !s.trim().is_empty() && s.len() <= 65536;
        let valid_token = |opt: &Option<String>| {
            opt.as_deref().is_none_or(|s| {
                !s.trim().is_empty()
                    && s.trim() == s
                    && s.len() <= 64
                    && !s.chars().any(char::is_control)
            })
        };
        let valid_timeout =
            |ms: &Option<i64>| ms.is_none_or(|d| (0..=365 * 86_400_000).contains(&d));
        if !bounded(&self.name)
            || self.steps.is_empty()
            || self.steps.len() > 256
            || self.steps.iter().any(|s| match s {
                Step::Script { command } => !bounded(command),
                Step::Agent {
                    runtime,
                    prompt,
                    effort,
                    service_tier,
                    ..
                } => {
                    !bounded(runtime)
                        || !bounded(prompt)
                        || !valid_token(effort)
                        || !valid_token(service_tier)
                }
                Step::WaitSignal { channel } => !mailbox::valid_key(channel),
                Step::Ask {
                    question,
                    timeout_ms,
                } => !bounded(question) || !valid_timeout(timeout_ms),
                Step::Wait { duration_ms } => *duration_ms < 0 || *duration_ms > 365 * 86400000i64,
                Step::Workflow { workflow, .. } => self_id == Some(*workflow),
                Step::Stop { reason } | Step::Skip { reason } => {
                    reason.as_ref().is_some_and(|r| r.len() > 65536)
                }
            })
        {
            return Err(Error::Refused("invalid workflow name or steps".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Workflow {
    pub project: Uuid,
    pub id: Uuid,
    pub definition: Definition,
    pub metadata: WorkflowMetadata,
    pub ask_metadata: BTreeMap<usize, AskMetadata>,
    pub next_ms: Option<i64>,
    pub last_missed_ms: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct WireWorkflow {
    project: Uuid,
    id: Uuid,
    name: String,
    enabled: bool,
    schedule: Schedule,
    steps: Vec<WireStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(default)]
    workspace_strategy: WorkspaceStrategy,
    #[serde(default)]
    concurrency: ConcurrencyPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    ask_metadata: BTreeMap<usize, AskMetadata>,
    next_ms: Option<i64>,
    last_missed_ms: Option<i64>,
}

impl Serialize for Workflow {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let steps = self
            .definition
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| WireStep::from_step(s, self.ask_metadata.get(&i)))
            .collect();
        let wire = WireWorkflow {
            project: self.project,
            id: self.id,
            name: self.definition.name.clone(),
            enabled: self.definition.enabled,
            schedule: self.definition.schedule.clone(),
            steps,
            effort: self.metadata.effort.clone(),
            service_tier: self.metadata.service_tier.clone(),
            workspace_strategy: self.metadata.workspace_strategy,
            concurrency: self.metadata.concurrency,
            tags: self.metadata.tags.clone(),
            subject: self.metadata.subject.clone(),
            ask_metadata: self.ask_metadata.clone(),
            next_ms: self.next_ms,
            last_missed_ms: self.last_missed_ms,
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Workflow {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let wire = WireWorkflow::deserialize(deserializer)?;
        let mut steps = Vec::with_capacity(wire.steps.len());
        let mut ask_metadata = wire.ask_metadata;
        for (idx, ws) in wire.steps.into_iter().enumerate() {
            let (step, ask) = ws.into_step();
            if let Some(m) = ask {
                ask_metadata.entry(idx).or_insert(m);
            }
            steps.push(step);
        }
        Ok(Self {
            project: wire.project,
            id: wire.id,
            definition: Definition {
                name: wire.name,
                enabled: wire.enabled,
                schedule: wire.schedule,
                steps,
            },
            metadata: WorkflowMetadata {
                effort: wire.effort,
                service_tier: wire.service_tier,
                workspace_strategy: wire.workspace_strategy,
                concurrency: wire.concurrency,
                tags: wire.tags,
                subject: wire.subject,
            },
            ask_metadata,
            next_ms: wire.next_ms,
            last_missed_ms: wire.last_missed_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum Trigger {
    Manual(Uuid),
    Scheduled(i64),
    Webhook(String),
    Child { parent_run: Uuid, attempt: Uuid },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    Running,
    Waiting,
    Asking,
    Reconcile,
    Failed,
    Cancelled,
    Stopped,
    Skipped,
    Completed,
}
impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Stopped | Self::Skipped | Self::Cancelled
        )
    }
    pub(super) fn outcome_str(self) -> Option<&'static str> {
        match self {
            Self::Completed => Some("completed"),
            Self::Stopped => Some("stopped"),
            Self::Skipped => Some("skipped"),
            Self::Cancelled => Some("cancelled"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub id: Uuid,
    pub chat: Option<Uuid>,
    pub step: usize,
    pub started_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub attempt: Attempt,
    pub status: Status,
    pub result: Value,
    pub ended_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_schedule: Option<model_schedules::Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_due_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    webhook_templates: Option<Vec<Step>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    webhook_rendered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_attempt_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_terminal: Option<Status>,
    #[serde(default, skip_serializing_if = "WorkflowMetadata::is_default")]
    pub metadata: WorkflowMetadata,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ask_metadata: BTreeMap<usize, AskMetadata>,
    pub id: Uuid,
    pub project: Uuid,
    pub workflow: Uuid,
    pub trigger: Trigger,
    pub input: Value,
    pub started_ms: i64,
    pub steps: Vec<Step>,
    pub cursor: usize,
    pub status: Status,
    pub attempt: Option<Attempt>,
    pub receipts: Vec<Receipt>,
    pub wake_at_ms: Option<i64>,
    pub cancel_requested: bool,
    pub detail: Option<String>,
}

impl Run {
    fn begin(&mut self, now: i64) -> Attempt {
        let step = &self.steps[self.cursor];
        let attempt = Attempt {
            id: Uuid::new_v4(),
            chat: matches!(step, Step::Agent { .. }).then(|| match &self.model_schedule {
                Some(b) if b.bind => b.owner,
                Some(b) if !b.fresh => {
                    crate::chat::stable_id([self.workflow.as_bytes().as_slice(), b"scheduled chat"])
                }
                _ => Uuid::new_v4(),
            }),
            step: self.cursor,
            started_ms: now,
        };
        self.attempt = Some(attempt.clone());
        self.detail = None;
        match step {
            Step::Wait { duration_ms } => {
                self.status = Status::Waiting;
                self.wake_at_ms = Some(now.saturating_add(*duration_ms));
            }
            Step::Ask { timeout_ms, .. } => {
                self.status = Status::Asking;
                self.wake_at_ms = timeout_ms.map(|d| now.saturating_add(d));
            }
            Step::WaitSignal { .. } | Step::Workflow { .. } => {
                self.status = Status::Waiting;
                self.wake_at_ms = None;
            }
            _ => self.status = Status::Running,
        }
        attempt
    }
    fn finish(&mut self, success: bool, result: Value, now: i64) {
        let attempt = self.attempt.take().expect("active step");
        self.receipts.push(Receipt {
            attempt,
            status: if success {
                Status::Completed
            } else {
                Status::Failed
            },
            result,
            ended_ms: now,
        });
        self.wake_at_ms = None;
        if !self.cancel_requested {
            self.detail = None;
        }
        if success {
            self.cursor += 1;
        }
        self.status = if self.cancel_requested {
            self.requested_terminal.unwrap_or(Status::Cancelled)
        } else if !success {
            Status::Failed
        } else if self.cursor == self.steps.len() {
            Status::Completed
        } else {
            Status::Ready
        };
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Answer {
        attempt: Uuid,
        answer: Value,
    },
    Withdraw {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Cancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Skip {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Retry {
        attempt: Uuid,
    },
    Reconcile {
        attempt: Uuid,
        succeeded: bool,
        evidence: String,
        result: Value,
    },
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Data {
    #[serde(default)]
    milestones: crate::workbench::mcp::milestones::Log,
    #[serde(default)]
    model_schedules: BTreeMap<Uuid, model_schedules::Binding>,
    #[serde(default)]
    model_schedule_commands: BTreeMap<Uuid, model_schedules::Command>,
    #[serde(default)]
    mailbox: mailbox::Mailbox,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    sources: BTreeMap<Uuid, BTreeMap<String, sources::Saved>>,
    workflows: BTreeMap<Uuid, Workflow>,
    secrets: BTreeMap<Uuid, String>,
    #[serde(default)]
    webhook_tokens: BTreeMap<Uuid, String>,
    #[serde(default)]
    deliveries: BTreeMap<Uuid, Vec<webhook::Receipt>>,
    #[serde(default)]
    webhook_rules: BTreeMap<Uuid, webhook::Rules>,
    #[serde(default)]
    webhook_windows: BTreeMap<Uuid, webhook::Window>,
    runs: BTreeMap<Uuid, Run>,
    actions: BTreeMap<Uuid, (Uuid, Action)>,
}

pub(super) struct Engine {
    source_lock: Mutex<()>,
    wal: super::persistence::WalStore<Data>,
    db: Mutex<Data>,
    clock: Arc<dyn Clock>,
    started: AtomicBool,
    changed: watch::Sender<()>,
}
impl Engine {
    pub(in crate::workbench) fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub(in crate::workbench) fn compact_wal(&self) -> Result<u64> {
        let db = self.db.lock().unwrap();
        self.wal.compact(&*db)
    }

    pub(super) fn owns_agent_chat(&self, chat: Uuid) -> bool {
        self.db.lock().unwrap().runs.values().any(|run| {
            if run
                .model_schedule
                .as_ref()
                .is_some_and(|binding| binding.bind)
            {
                return false;
            }
            run.attempt
                .iter()
                .chain(run.receipts.iter().map(|r| &r.attempt))
                .any(|attempt| attempt.chat == Some(chat))
        })
    }
    pub(in crate::workbench) fn active_run_for_chat(&self, chat: Uuid) -> Option<Uuid> {
        self.db
            .lock()
            .unwrap()
            .runs
            .values()
            .find(|run| {
                !run.status.is_terminal()
                    && run.status != Status::Failed
                    && run.attempt.as_ref().is_some_and(|a| a.chat == Some(chat))
            })
            .map(|run| run.id)
    }

    pub(super) fn open(state: &Path, clock: Arc<dyn Clock>) -> Result<Self> {
        let file = state.join("workflows.json");
        let (db, wal) = super::persistence::WalStore::open(&file)?;
        let engine = Self {
            source_lock: Mutex::new(()),
            wal,
            db: Mutex::new(db),
            clock,
            started: false.into(),
            changed: watch::channel(()).0,
        };
        // A new engine owns Workbench's exclusive host lease. No old worker can
        // publish a receipt. Do not replay an unacknowledged side effect.
        if engine
            .db
            .lock()
            .unwrap()
            .runs
            .values()
            .any(|r| r.status == Status::Running)
        {
            engine.update(|d| {
                for run in d.runs.values_mut().filter(|r| r.status == Status::Running) {
                    if run.model_schedule.is_some() && run.attempt.is_some() {
                        run.status = Status::Ready;
                        continue;
                    }
                    run.status = Status::Reconcile;
                    run.detail =
                        Some("host stopped before the external result was committed".into());
                }
                Ok(())
            })?;
        }
        Ok(engine)
    }
    fn update<T>(&self, f: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        self.update_checked(|_| Ok(()), |d, ()| f(d))
    }
    fn update_checked<A, T>(
        &self,
        check: impl FnOnce(&Data) -> Result<A>,
        f: impl FnOnce(&mut Data, A) -> Result<T>,
    ) -> Result<T> {
        let mut current = self.db.lock().unwrap();
        let approved = check(&current)?;
        let mut next = current.clone();
        let value = f(&mut next, approved)?;
        let now = self.clock.now_ms();
        reconcile_hierarchy(&mut next, now);
        milestones::record(&current, &mut next, now);
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("workflow revision overflow".into()))?;
        self.wal.record(&next)?;
        *current = next;
        self.changed.send_replace(());
        Ok(value)
    }
}
fn same_project(project: Uuid, other: Uuid) -> Result<()> {
    if other != project {
        Err(missing())
    } else {
        Ok(())
    }
}

fn reconcile_hierarchy(d: &mut Data, now: i64) {
    loop {
        let mut progressed = false;
        let parent_stops: BTreeMap<Uuid, (Status, Option<String>)> = d
            .runs
            .values()
            .filter_map(|p| {
                let target = if p.status == Status::Stopped
                    || p.requested_terminal == Some(Status::Stopped)
                {
                    Some(Status::Stopped)
                } else if p.cancel_requested
                    || matches!(p.status, Status::Cancelled | Status::Skipped)
                {
                    Some(Status::Cancelled)
                } else {
                    None
                }?;
                Some((p.id, (target, p.detail.clone())))
            })
            .collect();
        for c in d.runs.values_mut() {
            if c.status.is_terminal() {
                continue;
            }
            let Some(pid) = c.parent_run_id else {
                continue;
            };
            let Some((target, reason)) = parent_stops.get(&pid) else {
                continue;
            };
            if !c.cancel_requested || c.requested_terminal != Some(*target) {
                c.cancel_requested = true;
                c.requested_terminal = Some(*target);
                if c.detail.is_none() {
                    c.detail = reason.clone();
                }
                progressed = true;
            }
            if !matches!(c.status, Status::Running | Status::Reconcile) && c.status != *target {
                c.status = *target;
                c.wake_at_ms = None;
                progressed = true;
            }
        }
        type FinishedChild = (Uuid, Uuid, Uuid, Status, Option<String>, Vec<Receipt>);
        let finished_children: Vec<FinishedChild> = d
            .runs
            .values()
            .filter_map(|c| {
                if !c.status.is_terminal() {
                    return None;
                }
                let pid = c.parent_run_id?;
                let aid = c.parent_attempt_id?;
                let waiting = d.runs.get(&pid).is_some_and(|p| {
                    p.status == Status::Waiting && p.attempt.as_ref().map(|a| a.id) == Some(aid)
                });
                waiting.then(|| {
                    (
                        pid,
                        aid,
                        c.id,
                        c.status,
                        c.detail.clone(),
                        c.receipts.clone(),
                    )
                })
            })
            .collect();
        for (pid, aid, cid, cstatus, cdetail, creceipts) in finished_children {
            let Some(parent) = d.runs.get_mut(&pid) else {
                continue;
            };
            if parent.status != Status::Waiting
                || parent.attempt.as_ref().map(|a| a.id) != Some(aid)
            {
                continue;
            }
            if cstatus == Status::Completed {
                parent.finish(
                    true,
                    json!({"run_id": cid, "outcome": "completed", "receipts": creceipts}),
                    now,
                );
            } else {
                let outcome = cstatus.outcome_str().unwrap_or("cancelled");
                parent.finish(
                    false,
                    json!({"run_id": cid, "outcome": outcome, "detail": cdetail, "receipts": creceipts}),
                    now,
                );
                if !parent.cancel_requested && parent.status == Status::Failed {
                    parent.detail =
                        Some(cdetail.unwrap_or_else(|| format!("child workflow {outcome}")));
                }
            }
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
}

fn insert_run(d: &mut Data, w: &Workflow, trigger: Trigger, input: Value, now: i64) -> Result<Run> {
    insert_run_with_parent(d, w, trigger, input, None, None, now)
}

fn insert_run_with_parent(
    d: &mut Data,
    w: &Workflow,
    trigger: Trigger,
    input: Value,
    parent_run_id: Option<Uuid>,
    parent_attempt_id: Option<Uuid>,
    now: i64,
) -> Result<Run> {
    if let Some(old) = d
        .runs
        .values()
        .find(|r| r.workflow == w.id && r.trigger == trigger)
    {
        if old.input != input
            || old.parent_run_id != parent_run_id
            || old.parent_attempt_id != parent_attempt_id
        {
            return Err(Error::Refused(
                "delivery identifier reused with different input".into(),
            ));
        }
        return Ok(old.clone());
    }
    if !w.definition.enabled {
        return Err(Error::Refused("workflow disabled".into()));
    }
    let has_active = d
        .runs
        .values()
        .any(|r| r.workflow == w.id && !r.status.is_terminal() && r.status != Status::Failed);
    let (initial_status, cancel_req, req_term, skip_detail) = match w.metadata.concurrency {
        ConcurrencyPolicy::Allow => (Status::Ready, false, None, None),
        ConcurrencyPolicy::Skip if has_active => (
            Status::Skipped,
            true,
            Some(Status::Skipped),
            Some("skipped due to active concurrent workflow run".into()),
        ),
        ConcurrencyPolicy::Skip => (Status::Ready, false, None, None),
        ConcurrencyPolicy::CancelPrevious => {
            for r in d.runs.values_mut().filter(|r| {
                r.workflow == w.id && !r.status.is_terminal() && r.status != Status::Failed
            }) {
                r.cancel_requested = true;
                r.requested_terminal = Some(Status::Cancelled);
                if r.detail.is_none() {
                    r.detail = Some("cancelled by newer workflow run".into());
                }
                if !matches!(r.status, Status::Running | Status::Reconcile) {
                    r.status = Status::Cancelled;
                    r.wake_at_ms = None;
                }
            }
            (Status::Ready, false, None, None)
        }
    };
    let run = Run {
        model_schedule: d.model_schedules.get(&w.id).cloned(),
        webhook_due_ms: None,
        webhook_templates: None,
        webhook_rendered: false,
        parent_run_id,
        parent_attempt_id,
        requested_terminal: req_term,
        metadata: w.metadata.clone(),
        ask_metadata: w.ask_metadata.clone(),
        id: Uuid::new_v4(),
        project: w.project,
        workflow: w.id,
        trigger,
        input,
        started_ms: now,
        steps: w.definition.steps.clone(),
        cursor: 0,
        status: initial_status,
        attempt: None,
        receipts: vec![],
        wake_at_ms: None,
        cancel_requested: cancel_req,
        detail: skip_detail,
    };
    d.runs.insert(run.id, run.clone());
    Ok(run)
}

fn install_definition(
    d: &mut Data,
    project: Uuid,
    id: Uuid,
    definition: Definition,
    webhook_secret: Option<String>,
    now: i64,
) -> Result<Workflow> {
    let parsed_extras = take_definition_extras(&definition);
    install_definition_with_extras(
        d,
        project,
        id,
        definition,
        parsed_extras,
        webhook_secret,
        now,
    )
}

fn install_definition_with_extras(
    d: &mut Data,
    project: Uuid,
    id: Uuid,
    definition: Definition,
    extras: Option<(WorkflowMetadata, BTreeMap<usize, AskMetadata>)>,
    webhook_secret: Option<String>,
    now: i64,
) -> Result<Workflow> {
    definition.validate_for(Some(id))?;
    let (metadata, ask_metadata) = match extras {
        Some((m, a)) => {
            m.validate()?;
            for item in a.values() {
                item.validate()?;
            }
            (m, a)
        }
        None => (WorkflowMetadata::default(), BTreeMap::new()),
    };
    let candidate = definition.schedule.next_after(now)?;
    let old = d.workflows.get(&id);
    if let Some(old) = old {
        same_project(project, old.project)?;
    }
    if old.is_none()
        && d.runs
            .values()
            .any(|r| r.workflow == id && r.project != project)
    {
        return Err(missing());
    }
    let preserved = old.filter(|old| {
        old.definition.enabled && old.definition.schedule.same_timing(&definition.schedule)
    });
    let next_ms = if !definition.enabled {
        None
    } else if let Some(old) = preserved {
        old.next_ms
    } else {
        if matches!(definition.schedule, Schedule::Once { .. }) && candidate.is_none() {
            return Err(Error::Refused(
                "one-time schedule must be in the future".into(),
            ));
        }
        candidate
    };
    if matches!(definition.schedule, Schedule::Webhook) {
        if let Some(secret) = webhook_secret {
            if secret.len() < 16 || secret.len() > 4096 {
                return Err(Error::Refused(
                    "webhook signing secret must be 16 to 4096 bytes".into(),
                ));
            }
            d.secrets.insert(id, secret);
        }
        // Token-only hooks are reachable after the owner issues a URL.
        // The permanent URL always requires an explicit signing secret.
    } else {
        d.secrets.remove(&id);
        d.webhook_tokens.remove(&id);
        d.webhook_rules.remove(&id);
        d.webhook_windows.remove(&id);
    }
    let w = Workflow {
        project,
        id,
        next_ms,
        last_missed_ms: old.and_then(|w| w.last_missed_ms),
        definition,
        metadata,
        ask_metadata,
    };
    d.workflows.insert(id, w.clone());
    Ok(w)
}

impl Workbench {
    pub fn save_workflow(
        &self,
        project: Uuid,
        id: Uuid,
        definition: Definition,
        webhook_secret: Option<String>,
    ) -> Result<Workflow> {
        self.project(project)?;
        let engine = &self.0.workflows;
        engine.update(|d| {
            sources::allow_direct(d, id)?;
            install_definition(
                d,
                project,
                id,
                definition,
                webhook_secret,
                engine.clock.now_ms(),
            )
        })
    }

    pub fn save_workflow_with_metadata(
        &self,
        project: Uuid,
        id: Uuid,
        definition: Definition,
        metadata: WorkflowMetadata,
        ask_metadata: BTreeMap<usize, AskMetadata>,
        webhook_secret: Option<String>,
    ) -> Result<Workflow> {
        self.project(project)?;
        let engine = &self.0.workflows;
        engine.update(|d| {
            sources::allow_direct(d, id)?;
            let _ = take_definition_extras(&definition);
            install_definition_with_extras(
                d,
                project,
                id,
                definition,
                Some((metadata, ask_metadata)),
                webhook_secret,
                engine.clock.now_ms(),
            )
        })
    }

    pub fn withdraw_workflow_question(
        &self,
        project: Uuid,
        run_id: Uuid,
        operation: Uuid,
        attempt: Option<Uuid>,
        reason: Option<String>,
    ) -> Result<Run> {
        self.workflow_action(
            project,
            run_id,
            operation,
            Action::Withdraw { attempt, reason },
        )
    }
    pub fn workflows(&self, project: Uuid) -> Result<Vec<Workflow>> {
        self.project(project)?;
        Ok(self
            .0
            .workflows
            .db
            .lock()
            .unwrap()
            .workflows
            .values()
            .filter(|w| w.project == project)
            .cloned()
            .collect())
    }
    pub fn workflow_runs(&self, project: Uuid, id: Uuid) -> Result<Vec<Run>> {
        self.project(project)?;
        let d = self.0.workflows.db.lock().unwrap();
        same_project(project, d.workflows.get(&id).ok_or_else(missing)?.project)?;
        Ok(d.runs
            .values()
            .filter(|r| r.workflow == id)
            .cloned()
            .collect())
    }
    pub fn workflow_run(&self, project: Uuid, id: Uuid) -> Result<Run> {
        self.project(project)?;
        let d = self.0.workflows.db.lock().unwrap();
        let r = d.runs.get(&id).ok_or_else(missing)?;
        same_project(project, r.project)?;
        Ok(r.clone())
    }
    pub fn trigger_workflow(
        &self,
        project: Uuid,
        id: Uuid,
        operation: Uuid,
        input: Value,
    ) -> Result<Run> {
        self.trigger_workflow_with_parent(project, id, operation, input, None)
    }
    pub fn trigger_workflow_with_parent(
        &self,
        project: Uuid,
        id: Uuid,
        operation: Uuid,
        input: Value,
        parent_run_id: Option<Uuid>,
    ) -> Result<Run> {
        self.trigger_workflow_checked(project, id, operation, input, parent_run_id, || Ok(()))
    }
    pub(in crate::workbench) fn trigger_workflow_checked(
        &self,
        project: Uuid,
        id: Uuid,
        operation: Uuid,
        input: Value,
        parent_run_id: Option<Uuid>,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<Run> {
        self.project(project)?;
        let e = &self.0.workflows;
        e.update(|d| {
            authorize()?;
            let w = d.workflows.get(&id).ok_or_else(missing)?.clone();
            same_project(project, w.project)?;
            if let Some(pid) = parent_run_id {
                let parent = d.runs.get(&pid).ok_or_else(missing)?;
                same_project(project, parent.project)?;
            }
            if matches!(w.definition.schedule, Schedule::Webhook) {
                return Err(Error::Refused("webhook tasks run on delivery".into()));
            }
            insert_run_with_parent(
                d,
                &w,
                Trigger::Manual(operation),
                input,
                parent_run_id,
                None,
                e.clock.now_ms(),
            )
        })
    }
    pub fn workflow_action(
        &self,
        project: Uuid,
        id: Uuid,
        operation: Uuid,
        action: Action,
    ) -> Result<Run> {
        self.workflow_action_checked(project, id, operation, action, || Ok(()))
    }
    pub(in crate::workbench) fn workflow_action_checked(
        &self,
        project: Uuid,
        id: Uuid,
        operation: Uuid,
        action: Action,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<Run> {
        self.project(project)?;
        let e = &self.0.workflows;
        let now = e.clock.now_ms();
        e.update(|d| {
            authorize()?;
            let r = d.runs.get_mut(&id).ok_or_else(missing)?;
            same_project(project, r.project)?;
            if d.mailbox.commands.contains_key(&operation) {
                return Err(Error::Refused("workflow command already used".into()));
            }
            if let Some((run, old)) = d.actions.get(&operation) {
                if *run != id || *old != action {
                    return Err(Error::Refused("operation identifier already used".into()));
                }
                return Ok(r.clone());
            }
            let active = r.attempt.as_ref().map(|a| a.id);
            let clean_reason = |reason: &Option<String>| -> Result<Option<String>> {
                if reason.as_ref().is_some_and(|s| s.len() > 65536) {
                    return Err(Error::Refused("reason exceeds limit".into()));
                }
                Ok(reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned))
            };
            match &action {
                Action::Answer { attempt, answer }
                    if (r.status == Status::Asking
                        || r.status == Status::Waiting
                            && matches!(r.steps.get(r.cursor), Some(Step::WaitSignal { .. })))
                        && active == Some(*attempt) =>
                {
                    if r.status == Status::Asking
                        && let Some(meta) = r.ask_metadata.get(&r.cursor)
                    {
                        if meta.require_reason {
                            let has_reason = if !meta.choices.is_empty() {
                                answer
                                    .get("reason")
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| !s.trim().is_empty())
                            } else {
                                answer.as_str().is_some_and(|s| !s.trim().is_empty())
                                    || answer
                                        .get("reason")
                                        .and_then(Value::as_str)
                                        .is_some_and(|s| !s.trim().is_empty())
                            };
                            if !has_reason {
                                return Err(Error::Refused(
                                    "question requires a non-empty reason".into(),
                                ));
                            }
                        }
                        if !meta.choices.is_empty() {
                            let chosen = answer
                                .as_str()
                                .or_else(|| answer.get("choice").and_then(Value::as_str));
                            if let Some(c) = chosen
                                && !meta.choices.iter().any(|opt| opt.id == c)
                            {
                                return Err(Error::Refused(
                                    "answer is not one of the allowed question choices".into(),
                                ));
                            }
                        }
                    }
                    r.finish(true, answer.clone(), now)
                }
                Action::Withdraw { attempt, reason }
                    if r.status == Status::Asking && attempt.is_none_or(|a| active == Some(a)) =>
                {
                    let msg = clean_reason(reason)?;
                    r.finish(
                        true,
                        json!({"withdrawn": true, "answered": false, "reason": msg}),
                        now,
                    )
                }
                Action::Cancel { reason } if !r.status.is_terminal() => {
                    let msg = clean_reason(reason)?;
                    r.cancel_requested = true;
                    r.requested_terminal = Some(Status::Cancelled);
                    if let Some(msg) = msg {
                        r.detail = Some(msg);
                    }
                    if !matches!(r.status, Status::Running | Status::Reconcile) {
                        r.status = Status::Cancelled;
                        r.wake_at_ms = None;
                    }
                }
                Action::Stop { reason } if !r.status.is_terminal() => {
                    let msg = clean_reason(reason)?;
                    r.cancel_requested = true;
                    r.requested_terminal = Some(Status::Stopped);
                    if let Some(msg) = msg {
                        r.detail = Some(msg);
                    }
                    if !matches!(r.status, Status::Running | Status::Reconcile) {
                        r.status = Status::Stopped;
                        r.wake_at_ms = None;
                    }
                }
                Action::Skip { reason } if !r.status.is_terminal() => {
                    let msg = clean_reason(reason)?;
                    r.cancel_requested = true;
                    r.requested_terminal = Some(Status::Skipped);
                    if let Some(msg) = msg {
                        r.detail = Some(msg);
                    }
                    if !matches!(r.status, Status::Running | Status::Reconcile) {
                        r.status = Status::Skipped;
                        r.wake_at_ms = None;
                    }
                }
                Action::Retry { attempt }
                    if !r.cancel_requested
                        && r.status == Status::Failed
                        && r.receipts.last().is_some_and(|a| a.attempt.id == *attempt) =>
                {
                    r.status = Status::Ready
                }
                Action::Reconcile {
                    attempt,
                    succeeded,
                    evidence,
                    result,
                } if r.status == Status::Reconcile
                    && active == Some(*attempt)
                    && !evidence.trim().is_empty() =>
                {
                    r.finish(
                        *succeeded,
                        json!({"evidence":evidence,"result":result}),
                        now,
                    )
                }
                _ => {
                    return Err(Error::Refused(
                        "action does not match the active step or state".into(),
                    ));
                }
            }
            d.actions.insert(operation, (id, action));
            reconcile_hierarchy(d, now);
            Ok(d.runs.get(&id).cloned().unwrap())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workflows_support_concurrency_policies_rich_ask_withdraw_and_wal_recovery()
    -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root)?;
        let store_dir = tmp.path().join("store");

        let app = Workbench::open(
            crate::chat::ChatStore::open(&store_dir)?,
            std::slice::from_ref(&root),
        )?;
        let project = app.register(&root, "proj")?.id;

        // Parse rich Definition via JSON (exercising tags, subject, concurrency, and rich Ask metadata).
        let def_skip: Definition = serde_json::from_value(json!({
            "name": "Deploy Gate",
            "enabled": true,
            "schedule": {"type": "manual"},
            "concurrency": "skip",
            "tags": ["deploy", "prod"],
            "subject": "rel-42",
            "steps": [
                {
                    "type": "ask",
                    "question": "Ship to prod?",
                    "kind": "approval",
                    "choices": [{"id": "yes", "label": "Approve"}, {"id": "no", "label": "Hold"}],
                    "require_reason": true,
                    "links": [{"label": "PR", "url": "https://example.com/pr/42"}]
                },
                {"type": "wait", "duration_ms": 0}
            ]
        }))?;
        let wf_skip = Uuid::new_v4();
        let saved = app.save_workflow(project, wf_skip, def_skip, None)?;
        assert_eq!(saved.metadata.concurrency, ConcurrencyPolicy::Skip);
        assert_eq!(saved.metadata.tags, vec!["deploy", "prod"]);
        assert_eq!(saved.metadata.subject.as_deref(), Some("rel-42"));

        let run1 = app.trigger_workflow(project, wf_skip, Uuid::new_v4(), Value::Null)?;
        assert_eq!(run1.status, Status::Ready);
        // Second trigger while run1 is active must be skipped by ConcurrencyPolicy::Skip!
        let run2 = app.trigger_workflow(project, wf_skip, Uuid::new_v4(), Value::Null)?;
        assert_eq!(run2.status, Status::Skipped);

        app.tick_workflows()?;
        let asking = app.workflow_run(project, run1.id)?;
        assert_eq!(asking.status, Status::Asking);
        let attempt_id = asking.attempt.as_ref().unwrap().id;

        // Require-reason rejects bare choice without reason.
        assert!(
            app.workflow_action(
                project,
                run1.id,
                Uuid::new_v4(),
                Action::Answer {
                    attempt: attempt_id,
                    answer: json!("yes"),
                },
            )
            .is_err()
        );

        // Withdrawing the open question marks it withdrawn WITHOUT cancelling the workflow!
        let withdrawn = app.withdraw_workflow_question(
            project,
            run1.id,
            Uuid::new_v4(),
            Some(attempt_id),
            Some("superseded by auto-check".into()),
        )?;
        assert_eq!(withdrawn.status, Status::Ready);
        assert_eq!(withdrawn.receipts[0].result["withdrawn"], true);
        assert_eq!(withdrawn.receipts[0].result["answered"], false);

        // ConcurrencyPolicy::CancelPrevious cancels prior active runs.
        let def_cancel: Definition = serde_json::from_value(json!({
            "name": "Rolling Check",
            "enabled": true,
            "schedule": {"type": "manual"},
            "concurrency": "cancel_previous",
            "steps": [{"type": "ask", "question": "Continue?"}]
        }))?;
        let wf_cancel = Uuid::new_v4();
        app.save_workflow(project, wf_cancel, def_cancel, None)?;
        let c1 = app.trigger_workflow(project, wf_cancel, Uuid::new_v4(), Value::Null)?;
        app.tick_workflows()?;
        assert_eq!(app.workflow_run(project, c1.id)?.status, Status::Asking);
        let c2 = app.trigger_workflow(project, wf_cancel, Uuid::new_v4(), Value::Null)?;
        assert_eq!(app.workflow_run(project, c1.id)?.status, Status::Cancelled);
        assert_eq!(c2.status, Status::Ready);

        // Reopen Workbench from disk to verify WalStore replays workflow WAL frames cleanly!
        drop(app);
        let reopened = Workbench::open(
            crate::chat::ChatStore::open(&store_dir)?,
            std::slice::from_ref(&root),
        )?;
        assert_eq!(
            reopened.workflow_run(project, c1.id)?.status,
            Status::Cancelled
        );
        assert_eq!(reopened.workflow_run(project, c2.id)?.status, Status::Ready);
        assert_eq!(
            reopened.workflow_run(project, run1.id)?.receipts[0].result["withdrawn"],
            true
        );
        Ok(())
    }
}
