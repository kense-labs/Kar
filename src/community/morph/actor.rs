use super::{
    console::{Console, Lease},
    models,
    projection::{self, Projection},
    required_id,
};
use crate::{
    ApprovalMode, Direction, Error, Event, RecordKind, Result, SessionOptions, TurnReport,
    stream::Journal, transport::Command,
};
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use uuid::Uuid;

type Report = oneshot::Sender<Result<TurnReport>>;
struct Follow {
    id: String,
    done: Option<Report>,
    status: String,
    projection: Projection,
    adopted: bool,
    _watchers: JoinSet<()>,
}
enum Update {
    Task(String, Value),
    Unreachable(String),
}
pub(super) struct Actor {
    lease: Lease,
    id: String,
    cwd: PathBuf,
    model: Option<String>,
    approvals: ApprovalMode,
    journal: Journal,
    tasks: VecDeque<Follow>,
    queue: VecDeque<(Uuid, String, Report)>,
    pending: HashMap<String, String>,
    answered: HashMap<String, String>,
    updates: mpsc::Sender<Update>,
    inbox: mpsc::Receiver<Update>,
    snapshots: mpsc::Sender<(String, Value)>,
    frames: mpsc::Receiver<(String, Value)>,
}
impl Actor {
    pub fn new(options: &SessionOptions, id: String, lease: Lease, journal: Journal) -> Self {
        let (updates, inbox) = mpsc::channel(32);
        let (snapshots, frames) = mpsc::channel(32);
        Self {
            lease,
            id,
            cwd: options.cwd.clone(),
            model: options.model.clone(),
            approvals: options.approvals,
            journal,
            tasks: VecDeque::new(),
            queue: VecDeque::new(),
            pending: HashMap::new(),
            answered: HashMap::new(),
            updates,
            inbox,
            snapshots,
            frames,
        }
    }
    fn record(
        &self,
        span: Option<&str>,
        direction: impl Into<crate::stream::RecordStamp>,
        kind: RecordKind,
        v: Value,
        events: Vec<Event>,
    ) {
        self.journal.append(
            direction,
            kind,
            None,
            vec![],
            span.map(str::to_owned),
            v,
            events,
        );
    }
    async fn submit(&self, text: String, input_id: Option<Uuid>) -> Result<Value> {
        let mut body = json!({"task":text,"topic_id":self.id,"workspace_dir":self.cwd});
        if let Some(model) = &self.model {
            body["llm_profile"] = model.clone().into();
        }
        let request = Uuid::new_v4();
        let stamp = crate::stream::RecordStamp::from(Direction::ToRuntime);
        self.record(
            None,
            if let Some(id) = input_id {
                stamp.with_prompt_request(id, false, request)
            } else {
                stamp
            },
            RecordKind::Request,
            body.clone(),
            vec![],
        );
        let answer = self
            .lease
            .console
            .call(Method::POST, &["tasks"], Some(&body))
            .await;
        match &answer {
            Ok(native) => self.record(
                native["id"].as_str(),
                Direction::FromRuntime,
                RecordKind::Response,
                native.clone(),
                if input_id.is_some()
                    && required_id(native).is_ok()
                    && native["steer_target_task_id"]
                        .as_str()
                        .is_none_or(|s| s.is_empty())
                {
                    vec![Event::PromptDispatched]
                } else {
                    vec![]
                },
            ),
            Err(err) => self.record(
                None,
                Direction::Host,
                RecordKind::Response,
                json!({"error":err.to_string()}),
                vec![],
            ),
        }
        if input_id.is_some() {
            match &answer {
                Err(error) => self.journal.prompt_error(request, error, &[]),
                Ok(native) if required_id(native).is_ok() => {
                    self.journal.prompt_decision(request, None)
                }
                Ok(_) => self
                    .journal
                    .prompt_decision(request, Some(("error", "Morph omitted the task identity"))),
            }
        }
        answer
    }
    fn follow(&mut self, id: &str, done: Option<Report>, saved: Option<Projection>) {
        if self.tasks.iter().any(|t| t.id == id) {
            return;
        }
        let mut watchers = JoinSet::new();
        let console = self.lease.console.clone();
        let task = id.to_owned();
        let tx = self.snapshots.clone();
        watchers.spawn(async move { console.snapshots(task, tx).await });
        watchers.spawn(poll(
            self.lease.console.clone(),
            id.to_owned(),
            self.updates.clone(),
        ));
        self.tasks.push_back(Follow {
            id: id.into(),
            done,
            status: String::new(),
            adopted: saved.is_some(),
            projection: saved.unwrap_or_default(),
            _watchers: watchers,
        });
        self.record(
            Some(id),
            Direction::Host,
            RecordKind::Frame,
            Value::Null,
            vec![Event::TurnStarted],
        );
    }
    fn adopt(&mut self, params: Value) -> Result<Value> {
        if !self.tasks.is_empty() || !self.queue.is_empty() {
            return Err(Error::Busy);
        }
        #[derive(serde::Deserialize)]
        struct Observation {
            id: String,
            projection: Projection,
        }
        let saved: Observation = serde_json::from_value(params)?;
        self.follow(&saved.id, None, Some(saved.projection));
        Ok(Value::Null)
    }
    async fn start(&mut self, id: Uuid, text: String, done: Report) -> Result<()> {
        let answer = match self.submit(text, Some(id)).await {
            Ok(answer) => answer,
            Err(error) => {
                let _ = done.send(Err(Error::Refused(error.to_string())));
                return Err(error);
            }
        };
        let carrier = answer["steer_target_task_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(required_id(&answer)?);
        self.follow(carrier, Some(done), None);
        Ok(())
    }
    async fn steer(&mut self, text: String) -> Result<Value> {
        if self.tasks.is_empty() {
            return Err(Error::Refused("no active turn".into()));
        }
        let answer = self.submit(text, None).await?;
        let id = required_id(&answer)?;
        let carrier = answer["steer_target_task_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(id);
        if carrier == id && answer["status"] == "done" {
            return Err(Error::Refused("Morph did not accept this steer".into()));
        }
        self.follow(carrier, None, None);
        Ok(answer)
    }
    async fn stop(&self, id: &str) -> Result<Value> {
        self.record(
            Some(id),
            Direction::ToRuntime,
            RecordKind::Request,
            json!({"action":"stop"}),
            vec![],
        );
        let answer = self
            .lease
            .console
            .call(Method::POST, &["tasks", id, "stop"], None)
            .await?;
        self.record(
            Some(id),
            Direction::FromRuntime,
            RecordKind::Response,
            answer.clone(),
            vec![],
        );
        if answer["found"] != true {
            return Err(Error::Refused("no active turn".into()));
        }
        Ok(answer)
    }
    async fn decide(&mut self, id: &str, result: &Value) -> Result<()> {
        let task = self
            .pending
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Refused("unknown or answered approval".into()))?;
        let action = match result["decision"].as_str() {
            Some("approve") => "approve",
            Some("deny") => "deny",
            _ => {
                return Err(Error::Refused(
                    "Morph decision must be approve or deny".into(),
                ));
            }
        };
        let body =
            json!({"actor":"kar","note":result["note"].as_str().unwrap_or("Rust host decision")});
        let answer = self
            .lease
            .console
            .call(Method::POST, &["approvals", id, action], Some(&body))
            .await?;
        self.record(
            Some(&task),
            Direction::ToRuntime,
            RecordKind::Response,
            answer,
            vec![Event::RequestResolved { id: id.into() }],
        );
        self.answered.insert(id.into(), task);
        self.pending.remove(id);
        Ok(())
    }
    async fn approval(&mut self, task: &str, id: &str) {
        if self.answered.contains_key(id) {
            return;
        }
        if !self.pending.contains_key(id) {
            let Ok(info) = self
                .lease
                .console
                .call(Method::GET, &["approvals", id], None)
                .await
            else {
                return;
            };
            self.pending.insert(id.into(), task.into());
            self.record(
                Some(task),
                Direction::FromRuntime,
                RecordKind::Request,
                info,
                vec![Event::ApprovalRequested {
                    id: id.into(),
                    method: "morph/approval".into(),
                }],
            );
        }
        if self.approvals == ApprovalMode::Deny {
            let _ = self.decide(id, &json!({"decision":"deny"})).await;
        }
    }
    async fn update(&mut self, update: Update) -> Result<()> {
        let (id, info) = match update {
            Update::Task(id, v) => (id, v),
            Update::Unreachable(reason) => return Err(Error::Closed(reason)),
        };
        let Some(index) = self.tasks.iter().position(|t| t.id == id) else {
            return Ok(());
        };
        let status = info["status"].as_str().unwrap_or("");
        if let Some(outcome) = Projection::outcome(&info) {
            if self.tasks[index].adopted
                && !self.tasks[index]
                    .projection
                    .covers_remaining(&info["result"]["trace"])
            {
                return Err(Error::Closed(
                    "Morph task history has a gap after the saved position".into(),
                ));
            }
            let task = self.tasks.remove(index).expect("followed task");
            let (mut events, text, failure) = task.projection.finish(&id, &info, outcome.clone());
            if let Ok(Ok(metadata)) = tokio::time::timeout(
                Duration::from_millis(700),
                self.lease
                    .console
                    .call(Method::GET, &["topic", &self.id, "metadata"], None),
            )
            .await
            {
                let context = projection::context(&metadata);
                self.record(
                    Some(&id),
                    Direction::FromRuntime,
                    RecordKind::Frame,
                    metadata,
                    context,
                );
            }
            self.pending.retain(|request, task| {
                if task == &id {
                    events.push(Event::RequestResolved {
                        id: request.clone().into(),
                    });
                    false
                } else {
                    true
                }
            });
            self.record(
                Some(&id),
                Direction::FromRuntime,
                RecordKind::Frame,
                info,
                events,
            );
            self.answered.retain(|_, task| task != &id);
            if let Some(done) = task.done {
                let _ = done.send(Ok(TurnReport {
                    native_input_receipt: None,
                    structured_output: None,
                    outcome,
                    text,
                    failure,
                }));
            }
        } else {
            if self.tasks[index].status != status {
                self.tasks[index].status = status.into();
                self.record(
                    Some(&id),
                    Direction::FromRuntime,
                    RecordKind::Frame,
                    info.clone(),
                    vec![],
                );
            }
            if status == "pending"
                && let Some(approval) = info["approval_request_id"].as_str()
            {
                self.approval(&id, approval).await;
            }
        }
        Ok(())
    }
    async fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Prompt(id, input, admission, ack, done) => {
                let result = if self.tasks.is_empty() && self.queue.is_empty() {
                    match crate::chat::Admission::commit(admission).await {
                        Ok(()) => self.start(id, input.text, done).await,
                        Err(e) => Err(e),
                    }
                } else {
                    Err(self.journal.busy_prompt(id, &input, false, &[]))
                };
                let _ = ack.send(result);
            }
            Command::Queue(id, text, ack, done) => {
                self.queue.push_back((id, text, done));
                self.journal.host_event(Event::InputQueued {
                    input_id: id.to_string(),
                });
                let _ = ack.send(Ok(()));
            }
            Command::Withdraw(id, ack) => {
                let result = self
                    .queue
                    .iter()
                    .position(|e| e.0 == id)
                    .and_then(|i| self.queue.remove(i))
                    .map(|(_, _, done)| {
                        self.journal.host_event(Event::InputRemoved {
                            input_id: id.to_string(),
                        });
                        let _ = done.send(Err(Error::Refused("input withdrawn".into())));
                    })
                    .ok_or_else(|| Error::Refused("input is not queued".into()));
                let _ = ack.send(result);
            }
            Command::Steer(input, ack) => {
                let _ = ack.send(self.steer(input.text).await);
            }
            Command::Abort(ack) => {
                let result = match self.tasks.front() {
                    Some(t) => self.stop(&t.id).await,
                    None => Err(Error::Refused("no active turn".into())),
                };
                let _ = ack.send(result);
            }
            Command::Respond(id, result, ack) => {
                let result =
                    match result.payload(&self.journal, &id, "morph/approval", &Value::Null) {
                        Ok(value) => self.decide(id.as_str().unwrap_or(""), &value).await,
                        Err(err) => Err(err),
                    };
                let _ = ack.send(result);
            }
            Command::StopTask(_, ack) => {
                let _ = ack.send(Err(Error::Unsupported(
                    "native task stop on this adapter".into(),
                )));
            }
            Command::Compact(_, ack) => {
                let _ = ack.send(Err(Error::Unsupported(
                    "manual compaction through this protocol".into(),
                )));
            }
            Command::Rpc(method, params, _, ack) => {
                let result = match method.as_str() {
                    "profiles" => models(&self.lease.console).await,
                    "followTask" => self.adopt(params),
                    "readTask" => match params["id"].as_str() {
                        Some(id) => self.lease.console.read_task(id).await,
                        None => Err(Error::Protocol("missing Morph task id".into())),
                    },
                    _ => Err(Error::Unsupported(method)),
                };
                let _ = ack.send(result);
            }
            Command::Send(_, ack) => {
                let _ = ack.send(Err(Error::Unsupported("raw stdio on Morph".into())));
            }
            Command::Close(_) => unreachable!("close is handled by run"),
        }
    }
    async fn settle(&mut self) {
        // A detached controller may be replaced while the native task is still running.
        self.tasks.retain(|t| !t.adopted);
        for t in &self.tasks {
            let _ = self.stop(&t.id).await;
        }
        while !self.tasks.is_empty() {
            if let Some(update) = self.inbox.recv().await
                && self.update(update).await.is_err()
            {
                break;
            }
        }
    }
    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let mut exit = self.lease.exit();
        let (ack, reason, settle) = loop {
            if self.tasks.is_empty()
                && let Some((id, text, done)) = self.queue.pop_front()
            {
                let _ = self.start(id, text, done).await;
                self.journal.host_event(Event::InputRemoved {
                    input_id: id.to_string(),
                });
                continue;
            }
            tokio::select! {
                cmd=commands.recv()=>match cmd{
                    Some(Command::Close(ack))=>break (Some((self.journal.request_dispose(), ack)),"session closed".into(),true),
                    Some(cmd)=>self.command(cmd).await,
                    None=>break (None,"session dropped".into(),true),
                },
                Some((task,v))=self.frames.recv()=>{
                    if let Some(t)=self.tasks.iter_mut().find(|t|t.id==task) {
                        if t.adopted && v["trace"].is_object() && !t.projection.covers_remaining(&v["trace"]) {
                            // Ignore stale or incomplete websocket snapshots. The final GET
                            // decides whether all entries survived in the native archive.
                            continue;
                        }
                        let events=t.projection.snapshot(&task,&v);
                        self.record(Some(&task),Direction::FromRuntime,RecordKind::Frame,v,events);
                    }
                }
                Some(update)=self.inbox.recv()=>if let Err(err)=self.update(update).await{break (None,err.to_string(),false);},
                _=async {if let Some(rx)=&mut exit {let _=rx.wait_for(Option::is_some).await;}else{std::future::pending::<()>().await;}}=>{
                    break (None,"owned Morph Console exited".into(),false);
                }
            }
        };
        if settle {
            let _ = tokio::time::timeout(Duration::from_secs(3), self.settle()).await;
        }
        for task in self.tasks.drain(..) {
            if let Some(done) = task.done {
                let _ = done.send(Err(Error::Closed(reason.clone())));
            }
        }
        for (_, _, done) in self.queue.drain(..) {
            let _ = done.send(Err(Error::Closed(reason.clone())));
        }
        let stopped = self.lease.release().await;
        if let Some((id, _)) = &ack {
            let outcome = match &stopped {
                Ok(Some(status)) => json!({"type":"exited", "code":status.code()}),
                Ok(None) => json!({"type":"accepted"}),
                Err(err) => json!({"type":"rejected", "code":"error", "reason":err.to_string()}),
            };
            self.journal
                .disposed(*id, outcome, Event::Exited { reason });
        } else {
            self.journal.host_event(Event::Exited { reason });
        }
        self.journal.close();
        if let Some((_, ack)) = ack {
            let _ = ack.send(stopped.map(|_| ()).map_err(Error::from));
        }
    }
}
async fn poll(api: Console, id: String, tx: mpsc::Sender<Update>) {
    let mut wait = Duration::from_millis(150);
    let mut failures = 0;
    let mut previous = String::new();
    loop {
        tokio::time::sleep(wait).await;
        match api.call(Method::GET, &["tasks", &id], None).await {
            Ok(info) if info["id"].as_str() == Some(&id) && info["status"].is_string() => {
                failures = 0;
                let status = info["status"].as_str().unwrap();
                wait = if status == previous {
                    (wait * 2).min(Duration::from_secs(1))
                } else {
                    Duration::from_millis(250)
                };
                let terminal = matches!(status, "done" | "failed" | "canceled");
                previous = status.into();
                if tx.send(Update::Task(id.clone(), info)).await.is_err() || terminal {
                    return;
                }
            }
            _ => {
                failures += 1;
                if failures >= 8 {
                    let _ = tx
                        .send(Update::Unreachable(format!(
                            "Morph stopped returning task {id}"
                        )))
                        .await;
                    return;
                }
                wait = Duration::from_secs(1);
            }
        }
    }
}
