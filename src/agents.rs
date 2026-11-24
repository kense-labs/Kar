//! Host-owned, bounded groups of runtime sessions and their turn reports.
mod recording;
use crate::{Error, Outcome, Result, Runtime, Session, SessionOptions, Turn};
use recording::Recorder;
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize)]
pub struct AgentInfo {
    pub id: Uuid,
    pub name: Option<String>,
    pub runtime: String,
    pub session_id: String,
    pub turns_sent: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub id: Uuid,
    pub turn: u64,
    pub outcome: Outcome,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
struct Agent {
    info: AgentInfo,
    session: Session,
    send_gate: Arc<tokio::sync::Mutex<()>>,
    _permit: OwnedSemaphorePermit,
    active: Option<Weak<OwnedSemaphorePermit>>,
    recorder: Option<Recorder>,
}
#[derive(Default)]
struct State {
    agents: BTreeMap<Uuid, Agent>,
    reports: VecDeque<Report>,
}
#[derive(Clone)]
pub struct Crew {
    state: Arc<Mutex<State>>,
    limit: Arc<Semaphore>,
    running: Option<Arc<Semaphore>>,
    log_dir: Option<PathBuf>,
    changes: watch::Sender<u64>,
}
impl Crew {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            state: Default::default(),
            limit: Arc::new(Semaphore::new(max_sessions.min(Semaphore::MAX_PERMITS))),
            running: None,
            log_dir: None,
            changes: watch::channel(0).0,
        }
    }
    /// Limit simultaneously active agents. Idle sessions may receive further turns.
    pub fn with_running_limit(mut self, limit: usize) -> Self {
        self.running = Some(Arc::new(Semaphore::new(limit.min(Semaphore::MAX_PERMITS))));
        self
    }

    /// Write a separate Kar recording to a new private file for each opened agent.
    pub fn with_log_directory(mut self, dir: &Path) -> Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
        self.log_dir = Some(dir.canonicalize()?);
        Ok(self)
    }

    fn reserve_running(
        &self,
        existing: Option<&Weak<OwnedSemaphorePermit>>,
    ) -> Result<Option<Arc<OwnedSemaphorePermit>>> {
        let Some(limit) = &self.running else {
            return Ok(None);
        };
        if let Some(active) = existing.and_then(Weak::upgrade) {
            return Ok(Some(active));
        }
        let permit = limit
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Refused("active agent limit reached".into()))?;
        Ok(Some(Arc::new(permit)))
    }

    pub async fn spawn(
        &self,
        runtime: &Runtime,
        options: SessionOptions,
        name: Option<String>,
        prompt: String,
    ) -> Result<AgentInfo> {
        let (info, turn, active) = self.start(runtime, options, name, prompt).await?;
        self.monitor(info.id, 1, turn, active, None);
        Ok(info)
    }
    pub async fn run(
        &self,
        runtime: &Runtime,
        options: SessionOptions,
        name: Option<String>,
        prompt: String,
    ) -> Result<Report> {
        let (info, turn, active) = self.start(runtime, options, name, prompt).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.monitor(info.id, 1, turn, active, Some(tx));
        rx.await
            .map_err(|_| Error::Closed("report collector stopped".into()))
    }
    async fn start(
        &self,
        runtime: &Runtime,
        options: SessionOptions,
        name: Option<String>,
        prompt: String,
    ) -> Result<(AgentInfo, Turn, Option<Arc<OwnedSemaphorePermit>>)> {
        let permit = self
            .limit
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Refused("agent capacity reached".into()))?;
        let active = self.reserve_running(None)?;
        let id = Uuid::new_v4();
        let log = self
            .log_dir
            .as_ref()
            .map(|dir| dir.join(format!("{id}.kar.jsonl")));
        let session = runtime.open(options).await?;
        let recorder = if let Some(path) = &log {
            match Recorder::start(path, &runtime.id, &session).await {
                Ok(recorder) => Some(recorder),
                Err(error) => {
                    let _ = session.close().await;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let turn = match session.prompt(prompt).await {
            Ok(turn) => turn,
            Err(error) => {
                let closed = session.close().await;
                if let Some(recorder) = recorder {
                    if closed.is_err() {
                        recorder.abort();
                    } else {
                        recorder.finish().await?;
                    }
                }
                return Err(error);
            }
        };
        let info = AgentInfo {
            id,
            log,
            name,
            runtime: runtime.id.clone(),
            session_id: session.id().into(),
            turns_sent: 1,
        };
        self.state.lock().unwrap().agents.insert(
            info.id,
            Agent {
                info: info.clone(),
                session,
                send_gate: Default::default(),
                _permit: permit,
                active: active.as_ref().map(Arc::downgrade),
                recorder,
            },
        );
        Ok((info, turn, active))
    }
    fn monitor(
        &self,
        id: Uuid,
        number: u64,
        turn: Turn,
        active: Option<Arc<OwnedSemaphorePermit>>,
        recipient: Option<tokio::sync::oneshot::Sender<Report>>,
    ) {
        let weak = Arc::downgrade(&self.state);
        let changes = self.changes.clone();
        tokio::spawn(async move {
            let report = match turn.report().await {
                Ok(r) => Report {
                    id,
                    turn: number,
                    outcome: r.outcome,
                    text: r.text,
                    error: None,
                },
                Err(e) => Report {
                    id,
                    turn: number,
                    outcome: Outcome::Failed,
                    text: String::new(),
                    error: Some(e.to_string()),
                },
            };
            drop(active);
            let report = match recipient {
                Some(tx) => match tx.send(report) {
                    Ok(()) => return,
                    Err(report) => report,
                },
                None => report,
            };
            if let Some(state) = weak.upgrade() {
                state.lock().unwrap().reports.push_back(report);
                changes.send_modify(|n| *n = n.wrapping_add(1));
            }
        });
    }
    fn agent(&self, id: Uuid) -> Result<Session> {
        self.state
            .lock()
            .unwrap()
            .agents
            .get(&id)
            .map(|a| a.session.clone())
            .ok_or_else(|| Error::Refused("unknown subagent".into()))
    }
    pub fn list(&self) -> Vec<AgentInfo> {
        self.state
            .lock()
            .unwrap()
            .agents
            .values()
            .map(|a| a.info.clone())
            .collect()
    }
    pub fn unread(&self) -> Vec<(Uuid, u64)> {
        self.state
            .lock()
            .unwrap()
            .reports
            .iter()
            .map(|r| (r.id, r.turn))
            .collect()
    }
    pub async fn steer(&self, id: Uuid, message: String) -> Result<()> {
        self.agent(id)?.steer(message).await
    }
    pub async fn send(&self, id: Uuid, message: String, queue: bool) -> Result<u64> {
        let (session, gate) = self
            .state
            .lock()
            .unwrap()
            .agents
            .get(&id)
            .map(|a| (a.session.clone(), a.send_gate.clone()))
            .ok_or_else(|| Error::Refused("unknown subagent".into()))?;
        let _sending = gate.lock().await;
        let active = {
            let mut state = self.state.lock().unwrap();
            let agent = state
                .agents
                .get_mut(&id)
                .ok_or_else(|| Error::Refused("unknown subagent".into()))?;
            let permit = self.reserve_running(agent.active.as_ref())?;
            agent.active = permit.as_ref().map(Arc::downgrade);
            permit
        };
        let turn = if queue {
            session.queue(message).await?.1
        } else {
            session.prompt(message).await?
        };
        let number = {
            let mut s = self.state.lock().unwrap();
            let agent = s
                .agents
                .get_mut(&id)
                .ok_or_else(|| Error::Closed("subagent closed".into()))?;
            agent.info.turns_sent += 1;
            agent.info.turns_sent
        };
        self.monitor(id, number, turn, active, None);
        Ok(number)
    }
    pub async fn interrupt(&self, id: Uuid) -> Result<()> {
        self.agent(id)?.abort().await
    }
    pub async fn close(&self, id: Uuid) -> Result<()> {
        let agent = self
            .state
            .lock()
            .unwrap()
            .agents
            .remove(&id)
            .ok_or_else(|| Error::Refused("unknown subagent".into()))?;
        let result = agent.session.close().await;
        if let Some(recorder) = agent.recorder {
            if result.is_err() {
                recorder.abort();
            } else {
                recorder.finish().await?;
            }
        }
        result
    }
    /// Consume unread reports for any selected agent. An empty selection means all agents.
    pub async fn wait(&self, ids: &[Uuid], timeout: Duration) -> Vec<Report> {
        let mut changes = self.changes.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut ready = Vec::new();
            self.state.lock().unwrap().reports.retain(|r| {
                if ids.is_empty() || ids.contains(&r.id) {
                    ready.push(r.clone());
                    false
                } else {
                    true
                }
            });
            if !ready.is_empty() {
                return ready;
            }
            if !matches!(
                tokio::time::timeout_at(deadline, changes.changed()).await,
                Ok(Ok(()))
            ) {
                return ready;
            }
        }
    }
}
