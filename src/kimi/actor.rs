use super::{
    projection::{Frame, Projection},
    web::{self, Socket, Web, segment},
};
use crate::{
    ApprovalMode, CompactionStatus, Direction, Error, Event, Outcome, Protocol, RecordKind, Result,
    SessionOptions, TurnReport,
    compaction::Operation,
    input::Input,
    stream::Journal,
    transport::{Command, redact},
};

use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

type Reply<T> = oneshot::Sender<Result<T>>;
struct Turn {
    request_id: Uuid,
    done: Reply<TurnReport>,
    span: Option<String>,
    prompt: String,
    texts: Vec<(String, String)>,
    failure: Option<crate::TurnFailure>,
}
struct Steer {
    id: String,
    parent: Option<String>,
    prompt: Option<String>,
}
struct PendingAbort {
    deadline: tokio::time::Instant,
    span: Option<String>,
}
pub(super) struct Actor {
    web: Web,
    socket: Socket,
    id: String,
    path: String,
    journal: Journal,
    secrets: Vec<String>,
    mode: ApprovalMode,
    projection: Projection,
    turn: Option<Turn>,
    queue: VecDeque<(Uuid, Input, Reply<TurnReport>)>,
    steers: Vec<Steer>,
    pending_abort: Option<PendingAbort>,
    sent: HashSet<String>,
    compaction: Option<Operation>,
    requests: HashMap<String, (String, Value)>,
    resolved_requests: HashSet<String>,
    controls: HashMap<String, (tokio::time::Instant, Option<Reply<Value>>)>,
    timeout: Duration,
}
impl Actor {
    pub fn new(
        web: Web,
        socket: Socket,
        id: String,
        journal: Journal,
        opts: &SessionOptions,
    ) -> Self {
        let mut secrets = crate::options::environment_secrets(&opts.env);
        secrets.push(web.token.clone());
        secrets.extend(
            opts.mcp_servers
                .values()
                .flat_map(|m| m.secret_values())
                .cloned(),
        );
        secrets.retain(|s| !s.is_empty());
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self {
            path: format!("/sessions/{}", segment(&id)),
            id,
            web,
            socket,
            journal,
            secrets,
            mode: opts.approvals,
            projection: Projection::default(),
            turn: None,
            queue: VecDeque::new(),
            steers: vec![],
            pending_abort: None,
            sent: HashSet::new(),
            compaction: None,
            requests: HashMap::new(),
            resolved_requests: HashSet::new(),
            controls: HashMap::new(),
            timeout: opts.handshake_timeout,
        }
    }
    fn busy(&self) -> bool {
        self.turn.is_some()
            || self.projection.active().is_some()
            || !self.steers.is_empty()
            || self.pending_abort.is_some()
            || self.compaction.is_some()
    }
    fn record(
        &self,
        dir: impl Into<crate::stream::RecordStamp>,
        kind: RecordKind,
        mut body: Value,
        events: Vec<Event>,
    ) {
        redact(&mut body, &self.secrets);
        self.journal.push(dir, kind, None, vec![], body, events);
    }
    pub(super) fn capture_recovery(&self, snapshot: &Value, local: &crate::chat::ChatTurn) {
        if snapshot["type"] == "turns"
            && snapshot["agent_id"] == "main"
            && let Some((turn, _)) = super::recovery_turn(&snapshot["turns"], local)
        {
            // Retain only this host input's snapshot. Replaying an entire history
            // would publish old messages and token totals on the new stream.
            let mut turn = turn.clone();
            for item in turn
                .get_mut("items")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                if item["type"] == "user_message" {
                    crate::input::omit_image_bytes(item);
                }
            }
            self.record(
                Direction::FromRuntime,
                RecordKind::Frame,
                json!({"type":"turns","session_id":self.id,"agent_id":"main","turns":[turn]}),
                vec![],
            );
        }
    }
    async fn submit(
        &mut self,
        input: Input,
        id: Uuid,
        done: Option<Reply<TurnReport>>,
    ) -> Result<()> {
        let steer = done.is_none();
        let body = json!({"prompt_id":id,"content":input.blocks(Protocol::KimiKap)});
        let route = if steer { "/steer" } else { "/prompts" };
        let response = self
            .web
            .request(
                Method::POST,
                &(self.path.clone() + route),
                Some(body.clone()),
            )
            .await;
        if let Err(error) = response {
            if !steer {
                let request = self
                    .journal
                    .attempted_prompt(id, &input, false, &self.secrets);
                self.journal.prompt_error(request, &error, &self.secrets);
            }
            return Err(error);
        }
        let mut public = body;
        crate::input::omit_image_bytes(&mut public);
        redact(&mut public, &self.secrets);
        let text = crate::observe::sent_prompt_text(&public).unwrap_or_default();
        let mut events = vec![];
        if !steer {
            events.push(Event::PromptDispatched);
        }
        events.push(Event::UserMessage { text });
        let request = input.request_id;
        let mut stamp = crate::stream::RecordStamp::from(Direction::ToRuntime);
        stamp.carried = input.carried;
        self.record(
            if steer {
                stamp
            } else {
                stamp.with_prompt_request(
                    id,
                    false,
                    request,
                    crate::ImageSubmission::loaded(&input.text, &input.images, &self.secrets),
                )
            },
            RecordKind::Request,
            public,
            events,
        );
        if !steer {
            self.journal.control_decision(request, None);
        }
        let id = id.to_string();
        self.sent.insert(id.clone());
        if let Some(done) = done {
            self.turn = Some(Turn {
                request_id: request,
                done,
                span: None,
                prompt: id,
                texts: vec![],
                failure: None,
            });
        } else {
            self.steers.push(Steer {
                id,
                parent: self.projection.active().map(str::to_owned),
                prompt: self.turn.as_ref().map(|t| t.prompt.clone()),
            });
        }
        Ok(())
    }
    async fn ws_control(&mut self, mut body: Value, reply: Option<Reply<Value>>) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        body["request_id"] = id.clone().into();
        body["session_id"] = self.id.clone().into();
        web::send(&mut self.socket, &body).await?;
        self.controls
            .insert(id, (tokio::time::Instant::now() + self.timeout, reply));
        Ok(())
    }
    pub async fn frame(&mut self, mut v: Value) -> Result<()> {
        if v.get("session_id").is_some_and(|s| s != &self.id) {
            return Err(Error::Protocol(
                "Kimi sent a frame for another session".into(),
            ));
        }
        if v["type"] == "response" {
            if let Some((_, reply)) = v["request_id"]
                .as_str()
                .and_then(|id| self.controls.remove(id))
            {
                let result = if v["code"] == 0 {
                    Ok(Value::Null)
                } else {
                    Err(Error::Refused("Kimi refused WebSocket command".into()))
                };
                if let Some(tx) = reply {
                    let _ = tx.send(result);
                } else {
                    result?;
                }
            }
            return Ok(());
        }
        if v["type"] == "interaction" {
            self.interaction(v).await?;
            return Ok(());
        }
        redact(&mut v, &self.secrets);
        for mut frame in self.projection.frames(v) {
            let finished = self.observe(&mut frame);
            if frame.raw["item"]["type"] == "user_message"
                && let Some(user) = frame.raw.get_mut("item")
            {
                crate::input::omit_image_bytes(user);
            }
            self.journal.append(
                Direction::FromRuntime,
                RecordKind::Frame,
                None,
                frame.path.clone(),
                frame.span,
                frame.raw,
                frame.events,
            );
            if let Some((done, report)) = finished {
                let _ = done.send(Ok(report));
            }
            if frame.path.is_empty()
                && let Some(op) = &mut self.compaction
                && let Some(r) = self.journal.latest_record()
                && op.observe(&r)
            {
                op.publish(&self.journal);
                if !op.report.status.is_pending() {
                    self.compaction = None;
                }
            }
        }
        for id in std::mem::take(&mut self.projection.subscriptions) {
            self.ws_control(json!({"type":"subscribe","agent_ids":[id]}), None)
                .await?;
        }
        Ok(())
    }
    fn observe(&mut self, f: &mut Frame) -> Option<(Reply<TurnReport>, TurnReport)> {
        let n = &f.raw;
        if n["item"]["type"] == "user_message" {
            let ids = n["item"]["prompt_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            let own = ids.iter().any(|id| self.sent.contains(*id));
            if f.path.is_empty() {
                self.steers.retain(|s| !ids.contains(&s.id.as_str()));
            }
            if own {
                f.events.retain(|e| !matches!(e, Event::UserMessage { .. }));
            }
        }
        if !f.path.is_empty() {
            return None;
        }
        if f.events.contains(&Event::TurnStarted) {
            if let Some(cancel) = &mut self.pending_abort
                && cancel.span.is_none()
            {
                cancel.span.clone_from(&f.span);
            }
            if let Some(i) = self.steers.iter().position(|s| {
                s.parent
                    .as_deref()
                    .is_some_and(|p| n["turn"]["steered_from_turn_id"] == p)
                    || s.prompt
                        .as_deref()
                        .is_some_and(|p| n["turn"]["steered_from_prompt_id"] == p)
            }) {
                self.steers.remove(i);
            }
            if let Some(t) = &mut self.turn
                && t.span.is_none()
            {
                t.span = f.span.clone();
            }
            for s in &mut self.steers {
                if s.parent.is_none() {
                    s.parent = f.span.clone();
                }
            }
        }
        if let Some(outcome) = f.events.iter().find_map(|e| match e {
            Event::TurnEnded { outcome } => Some(outcome),
            _ => None,
        }) {
            if self
                .pending_abort
                .as_ref()
                .is_some_and(|cancel| match &cancel.span {
                    Some(target) => f.span.as_ref() == Some(target),
                    None => self.projection.active().is_none(),
                })
            {
                self.pending_abort = None;
            }
            if *outcome != Outcome::Completed && self.projection.active().is_none() {
                self.steers.clear();
            }
        }
        let t = self.turn.as_mut()?;
        if t.span.is_some() && f.span != t.span {
            return None;
        }
        let mut end = None;
        for e in &f.events {
            match e {
                Event::Message { text } | Event::TextDelta { text } => {
                    let id = n["item_id"]
                        .as_str()
                        .or_else(|| n["item"]["item_id"].as_str())
                        .unwrap_or("");
                    let index = t
                        .texts
                        .iter()
                        .position(|(i, _)| i == id)
                        .unwrap_or_else(|| {
                            t.texts.push((id.into(), String::new()));
                            t.texts.len() - 1
                        });
                    if matches!(e, Event::Message { .. }) {
                        t.texts[index].1 = text.clone()
                    } else {
                        t.texts[index].1.push_str(text)
                    }
                }
                Event::TurnFailure { failure } => t.failure = Some(failure.clone()),
                Event::TurnEnded { outcome } => end = Some(outcome.clone()),
                _ => {}
            }
        }
        if let Some(outcome) = end {
            let t = self.turn.take().unwrap();
            return Some((
                t.done,
                TurnReport {
                    native_input_receipt: None,
                    outcome,
                    text: t
                        .texts
                        .into_iter()
                        .map(|(_, s)| s)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    failure: t.failure,
                    structured_output: None,
                },
            ));
        }
        None
    }
    async fn answer(&mut self, id: &str, body: Value) -> Result<()> {
        self.web
            .request(
                Method::POST,
                &format!("{}/interactions/{}", self.path, segment(id)),
                Some(body),
            )
            .await?;
        self.requests.remove(id);
        self.resolved_requests.insert(id.to_owned());
        self.journal
            .host_event(Event::RequestResolved { id: id.into() });
        Ok(())
    }
    async fn interaction(&mut self, mut v: Value) -> Result<()> {
        let Some(id) = v["interaction_id"].as_str().map(str::to_owned) else {
            return Ok(());
        };
        if v["status"] == "ended" {
            self.resolved_requests.insert(id.clone());
            if self.requests.remove(&id).is_some() {
                self.journal
                    .host_event(Event::RequestResolved { id: id.into() });
            }
        } else if v["status"] == "pending"
            && !self.requests.contains_key(&id)
            && !self.resolved_requests.contains(&id)
        {
            let question = v["kind"] == "question";
            let method = if question {
                "kimi/question"
            } else {
                "kimi/approval"
            };
            self.requests.insert(id.clone(), (method.into(), v.clone()));
            redact(&mut v, &self.secrets);
            let agent = v["agent_id"].as_str().unwrap_or("main");
            self.journal.push(
                Direction::FromRuntime,
                RecordKind::Request,
                None,
                self.projection.agent_path(agent),
                v.clone(),
                vec![Event::ApprovalRequested {
                    id: id.clone().into(),
                    method: method.into(),
                }],
            );
            if self.mode != ApprovalMode::Host {
                let result = self
                    .answer(
                        &id,
                        if question {
                            json!({"kind":"question","dismissed":true})
                        } else {
                            json!({"kind":"approval","decision":"rejected"})
                        },
                    )
                    .await;
                // Another native client may have answered before our REST request arrived.
                if !matches!(result, Err(Error::Refused(_))) {
                    result?;
                }
            }
        }
        Ok(())
    }
    fn acknowledge_input<T>(ack: Reply<T>, result: Result<T>) -> Result<()> {
        let unknown = matches!(
            &result,
            Err(Error::Http(_) | Error::Json(_) | Error::Timeout(_))
        );
        let _ = ack.send(result);
        if unknown {
            return Err(Error::Closed(
                "Kimi input delivery could not be confirmed".into(),
            ));
        }
        Ok(())
    }
    fn arm_abort(&mut self, deadline: tokio::time::Instant) {
        self.pending_abort.get_or_insert_with(|| PendingAbort {
            deadline,
            span: self.projection.active().map(str::to_owned),
        });
    }
    async fn abort(&mut self) -> Result<Value> {
        let idle = self.turn.is_none() && self.projection.active().is_none();
        let local = idle && self.steers.is_empty() && self.pending_abort.is_none();
        let id = Uuid::new_v4();
        self.record(
            if local {
                Direction::Host
            } else {
                Direction::ToRuntime
            },
            RecordKind::Request,
            json!({"id":id,"method":"POST /sessions/{id}:cancel"}),
            vec![],
        );
        let result = if local {
            Err(Error::Refused("no active turn".into()))
        } else {
            // Repeated attempts share the earliest deadline. A refused attempt
            // cannot disarm a previous cancellation that the server accepted.
            let deadline = self
                .pending_abort
                .as_ref()
                .map(|cancel| cancel.deadline)
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(10));
            let reply = tokio::time::timeout_at(
                deadline,
                self.web.request(
                    Method::POST,
                    &(self.path.clone() + ":cancel"),
                    Some(json!({})),
                ),
            )
            .await;
            match reply {
                Ok(Ok(v)) if v["cancelled"] == true => {
                    self.arm_abort(deadline);
                    Ok(v)
                }
                Ok(Ok(v)) if v["cancelled"] == false => {
                    Err(Error::Refused("no active turn".into()))
                }
                Ok(Ok(_)) => Err(Error::Protocol(
                    "Kimi cancel response lacks a boolean cancelled field".into(),
                )),
                Ok(Err(e)) => {
                    if matches!(e, Error::Http(_) | Error::Json(_)) {
                        self.arm_abort(deadline);
                    }
                    Err(e)
                }
                Err(_) => {
                    self.arm_abort(deadline);
                    Err(Error::Timeout("Kimi cancellation".into()))
                }
            }
        };
        // The parent may have ended while its steer is still waiting for a native turn.
        if !local
            && idle
            && matches!(&result,Err(Error::Refused(reason)) if reason=="no active turn")
        {
            self.steers.clear();
            self.pending_abort = None;
        }
        let report = match &result {
            Ok(v) => json!({"id":id,"result":v}),
            Err(e) => json!({"id":id,"error":e.to_string()}),
        };
        self.record(
            if local {
                Direction::Host
            } else {
                Direction::FromRuntime
            },
            RecordKind::Response,
            report,
            vec![],
        );
        result
    }
    async fn command(&mut self, c: Command) -> Result<()> {
        match c {
            Command::Prompt(id, input, admission, ack, done) => {
                let res = if self.busy() || !self.queue.is_empty() {
                    Err(self.journal.busy_prompt(id, &input, false, &self.secrets))
                } else {
                    match crate::chat::Admission::commit(admission).await {
                        Ok(()) => self.submit(input, id, Some(done)).await,
                        Err(e) => Err(e),
                    }
                };
                Self::acknowledge_input(ack, res)?;
            }
            Command::Queue(id, input, ack, done) => {
                self.journal.queue_request(id, &input, &self.secrets);
                self.journal
                    .queue_accepted(input.request_id, id, true, None);
                self.queue.push_back((id, input, done));
                let _ = ack.send(Ok(()));
            }
            Command::Withdraw(id, ack) => {
                let _ = ack.send(self.journal.withdraw_held(&mut self.queue, id));
            }
            Command::Steer(input_id, input, target, ack) => {
                let Some((request, ack)) = self.journal.admit_steer(
                    input_id,
                    &input,
                    target,
                    target.is_none_or(|t| {
                        self.turn
                            .as_ref()
                            .is_some_and(|turn| Some(turn.request_id) == t.request_id)
                    }) && self.compaction.is_none(),
                    &self.secrets,
                    ack,
                ) else {
                    return Ok(());
                };
                let res = if self.compaction.is_some() {
                    Err(Error::Busy)
                } else if self.projection.active().is_none() && self.turn.is_none() {
                    Err(Error::Refused("no active turn".into()))
                } else {
                    self.submit(input, input_id, None)
                        .await
                        .map(|_| Value::Null)
                };
                self.journal.control_result(request, &res, &self.secrets);
                Self::acknowledge_input(ack, res)?;
            }
            Command::Abort(request) => {
                let Some(ack) = request.for_current(&self.journal) else {
                    return Ok(());
                };
                let request = self.journal.request_abort();
                let res = self.abort().await;
                self.journal.control_result(request, &res, &self.secrets);
                let _ = ack.send(res);
            }
            Command::StopTask(id, _, ack) => {
                let agent = self
                    .projection
                    .task_agents
                    .get(&id)
                    .map_or("main", String::as_str);
                self.ws_control(
                    json!({"type":"task.stop","task_id":id,"agent_id":agent}),
                    Some(ack),
                )
                .await?;
            }
            Command::Respond(id, reply, ack) => {
                let res =
                    if let Some((method, body)) = id.as_str().and_then(|s| self.requests.get(s)) {
                        match reply.payload(&self.journal, &id, method, body) {
                            Ok(value) => self.answer(id.as_str().unwrap(), value).await,
                            Err(e) => Err(e),
                        }
                    } else {
                        Err(Error::Refused(
                            "unknown or resolved Kimi interaction".into(),
                        ))
                    };
                let _ = ack.send(res);
            }
            Command::Compact(_, _, ack) => {
                let result = self.compact().await;
                let _ = ack.send(result);
            }
            Command::Rpc(m, _, _, tx) => {
                let res = if m == "models" {
                    self.web
                        .request(Method::GET, "/models", None)
                        .await
                        .and_then(|mut v| {
                            let rows = v["items"].as_array_mut().ok_or_else(|| {
                                Error::Protocol("invalid Kimi model catalog".into())
                            })?;
                            let mut data = json!(rows);
                            redact(&mut data, &self.secrets);
                            Ok(data)
                        })
                } else {
                    Err(Error::Unsupported("unknown Kimi RPC".into()))
                };
                let _ = tx.send(res);
            }
            Command::Send(_, tx) => {
                let _ = tx.send(Err(Error::Unsupported("raw Kimi input".into())));
            }
            Command::Close(_) => unreachable!(),
        }
        Ok(())
    }
    async fn compact(&mut self) -> Result<crate::ManualCompaction> {
        if self.busy() || !self.queue.is_empty() {
            return Err(Error::Busy);
        }
        self.record(
            Direction::ToRuntime,
            RecordKind::Request,
            json!({"method":"compact","session_id":self.id}),
            vec![Event::ManualCompactionRequested],
        );
        let request = self.journal.latest_record().unwrap();
        let (tx, rx) = oneshot::channel();
        let (mut op, handle) = Operation::from_request(Protocol::KimiKap, 0, &request, rx);
        let reply = self
            .web
            .request(
                Method::POST,
                &(self.path.clone() + ":compact"),
                Some(json!({})),
            )
            .await;
        let (native, status) = match &reply {
            Ok(v) => (v.clone(), CompactionStatus::Accepted),
            Err(Error::Refused(reason)) => (
                Value::Null,
                CompactionStatus::Rejected {
                    code: crate::CompactionRejection::RuntimeRefused,
                    reason: reason.clone(),
                },
            ),
            Err(_) => (
                Value::Null,
                CompactionStatus::Unconfirmed {
                    code: crate::CompactionUnconfirmed::ObservationFailed,
                },
            ),
        };
        self.record(Direction::FromRuntime, RecordKind::Response, native, vec![]);
        op.report.response = self.journal.latest_record().map(|r| r.cursor());
        op.report.status = status;
        op.publish(&self.journal);
        if op.report.status.is_pending() {
            self.compaction = Some(op);
        }
        let _ = tx.send(reply);
        Ok(handle)
    }
    pub async fn close(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let _ = self.socket.close(None).await;
        self.web.stop().await
    }
    pub async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut close_ack = None;
        let reason = loop {
            if !self.busy()
                && let Some((id, input, done)) = self.queue.pop_front()
            {
                self.journal.host_event(Event::InputRemoved {
                    input_id: id.to_string(),
                });
                if let Err(e) = self.submit(input.dispatch_queued(id), id, Some(done)).await {
                    break e.to_string();
                }
            }
            let deadline = self
                .controls
                .values()
                .map(|(d, _)| *d)
                .chain(self.pending_abort.as_ref().map(|cancel| cancel.deadline))
                .min();
            tokio::select! {
                ()=async {if let Some(at)=deadline{tokio::time::sleep_until(at).await}else{std::future::pending::<()>().await}}=>{
                    if self.pending_abort.as_ref().is_some_and(|cancel| cancel.deadline <= tokio::time::Instant::now()) { break "Kimi cancellation exceeded its deadline".into(); }
                    let expired:Vec<_>=self.controls.iter().filter(|(_,v)|v.0<=tokio::time::Instant::now()).map(|(id,_)|id.clone()).collect();
                    let mut failed=false;for id in expired{if let Some((_,Some(tx)))=self.controls.remove(&id){let _=tx.send(Err(Error::Timeout("Kimi WebSocket command".into())));}else{failed=true;}}
                    if failed{break "Kimi child subscription timed out".into();}
                }
                c=rx.recv()=>match c{None=>break "last session handle dropped".into(),Some(Command::Close(tx))=>{close_ack=Some((self.journal.request_dispose(),tx));break "closed by host".into()},Some(c)=>if let Err(e)=self.command(c).await{break e.to_string()}},
                v=web::read(&mut self.socket)=>match v{Ok(v)=>if let Err(e)=self.frame(v).await{break e.to_string()},Err(e)=>break e.to_string()},
            }
        };
        if let Some(op) = &mut self.compaction {
            op.report = op.report.unconfirmed(if close_ack.is_some() {
                crate::CompactionUnconfirmed::Disposed
            } else {
                crate::CompactionUnconfirmed::RuntimeExited
            });
            op.publish(&self.journal);
        }
        if let Some(t) = self.turn.take() {
            let _ = t.done.send(Err(Error::Closed(reason.clone())));
        }
        for (id, _, done) in self.queue.drain(..) {
            let _ = done.send(Err(Error::Closed(reason.clone())));
            self.journal.host_event(Event::InputDropped {
                input_id: id.to_string(),
            });
        }
        let status = self.close().await;
        self.journal
            .process_exited(close_ack.as_ref().map(|(id, _)| *id), reason, &status);
        self.journal.close();
        crate::session::reachability::reject_pending(
            &mut rx,
            &self.journal,
            Protocol::KimiKap,
            &self.secrets,
        )
        .await;
        if let Some((_, tx)) = close_ack {
            let _ = tx.send(status.map(|_| ()).map_err(Error::from));
        }
    }
}
