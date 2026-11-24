//! Project-bound durable workflows. An attempt is committed before an external call.
//! Only a receipt, or explicit reconciliation, allows that step to advance.
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
    collections::BTreeMap,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
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
    },
    Ask {
        question: String,
    },
    Wait {
        duration_ms: i64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub enabled: bool,
    pub schedule: Schedule,
    pub steps: Vec<Step>,
}
impl Definition {
    fn validate(&self) -> Result<()> {
        let bounded = |s: &str| !s.trim().is_empty() && s.len() <= 65536;
        if !bounded(&self.name)
            || self.steps.is_empty()
            || self.steps.len() > 256
            || self.steps.iter().any(|s| match s {
                Step::Script { command } => !bounded(command),
                Step::Agent {
                    runtime, prompt, ..
                } => !bounded(runtime) || !bounded(prompt),
                Step::Ask { question } => !bounded(question),
                Step::Wait { duration_ms } => *duration_ms < 0 || *duration_ms > 365 * 86400000i64,
            })
        {
            return Err(Error::Refused("invalid workflow name or steps".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workflow {
    pub project: Uuid,
    pub id: Uuid,
    #[serde(flatten)]
    pub definition: Definition,
    pub next_ms: Option<i64>,
    pub last_missed_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum Trigger {
    Manual(Uuid),
    Scheduled(i64),
    Webhook(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    Running,
    Waiting,
    Asking,
    Reconcile,
    Failed,
    Cancelled,
    Completed,
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
    pub webhook_due_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    webhook_templates: Option<Vec<Step>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    webhook_rendered: bool,
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
            chat: matches!(step, Step::Agent { .. }).then(Uuid::new_v4),
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
            Step::Ask { .. } => self.status = Status::Asking,
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
        self.detail = None;
        if success {
            self.cursor += 1;
        }
        self.status = if self.cancel_requested {
            Status::Cancelled
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
    Cancel,
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
    file: PathBuf,
    db: Mutex<Data>,
    clock: Arc<dyn Clock>,
    started: AtomicBool,
    changed: watch::Sender<()>,
}
impl Engine {
    pub(super) fn open(state: &Path, clock: Arc<dyn Clock>) -> Result<Self> {
        let file = state.join("workflows.json");
        let db = match File::open(&file) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e.into()),
        };
        let engine = Self {
            source_lock: Mutex::new(()),
            file,
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
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("workflow revision overflow".into()))?;
        let parent = self.file.parent().unwrap();
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut tmp, &next)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.file).map_err(|e| e.error)?;
        *current = next;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
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

fn insert_run(d: &mut Data, w: &Workflow, trigger: Trigger, input: Value, now: i64) -> Result<Run> {
    if let Some(old) = d
        .runs
        .values()
        .find(|r| r.workflow == w.id && r.trigger == trigger)
    {
        if old.input != input {
            return Err(Error::Refused(
                "delivery identifier reused with different input".into(),
            ));
        }
        return Ok(old.clone());
    }
    if !w.definition.enabled {
        return Err(Error::Refused("workflow disabled".into()));
    }
    let run = Run {
        webhook_due_ms: None,
        webhook_templates: None,
        webhook_rendered: false,
        id: Uuid::new_v4(),
        project: w.project,
        workflow: w.id,
        trigger,
        input,
        started_ms: now,
        steps: w.definition.steps.clone(),
        cursor: 0,
        status: Status::Ready,
        attempt: None,
        receipts: vec![],
        wake_at_ms: None,
        cancel_requested: false,
        detail: None,
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
    definition.validate()?;
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
    let preserved =
        old.filter(|old| old.definition.enabled && old.definition.schedule == definition.schedule);
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
        self.project(project)?;
        let e = &self.0.workflows;
        e.update(|d| {
            let w = d.workflows.get(&id).ok_or_else(missing)?.clone();
            same_project(project, w.project)?;
            if matches!(w.definition.schedule, Schedule::Webhook) {
                return Err(Error::Refused("webhook tasks run on delivery".into()));
            }
            insert_run(d, &w, Trigger::Manual(operation), input, e.clock.now_ms())
        })
    }
    pub fn workflow_action(
        &self,
        project: Uuid,
        id: Uuid,
        operation: Uuid,
        action: Action,
    ) -> Result<Run> {
        self.project(project)?;
        let e = &self.0.workflows;
        let now = e.clock.now_ms();
        e.update(|d| {
            let r = d.runs.get_mut(&id).ok_or_else(missing)?;
            same_project(project, r.project)?;
            if let Some((run, old)) = d.actions.get(&operation) {
                if *run != id || *old != action {
                    return Err(Error::Refused("operation identifier already used".into()));
                }
                return Ok(r.clone());
            }
            let active = r.attempt.as_ref().map(|a| a.id);
            match &action {
                Action::Answer { attempt, answer }
                    if r.status == Status::Asking && active == Some(*attempt) =>
                {
                    r.finish(true, answer.clone(), now)
                }
                Action::Cancel if !matches!(r.status, Status::Cancelled | Status::Completed) => {
                    r.cancel_requested = true;
                    if !matches!(r.status, Status::Running | Status::Reconcile) {
                        r.status = Status::Cancelled;
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
            Ok(r.clone())
        })
    }
}
