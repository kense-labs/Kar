use super::{
    projection::Projection,
    turn_id,
    web::{self, Socket, Web},
};
use crate::{
    ApprovalMode, Direction, Error, Event, Protocol, RecordKind, Result, SessionOptions,
    TurnReport,
    input::Input,
    stream::Journal,
    transport::{Command, redact},
};
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
type Reply<T> = oneshot::Sender<Result<T>>;
struct Turn {
    done: Reply<TurnReport>,
    text: Vec<String>,
    failure: Option<crate::TurnFailure>,
    claimed: bool,
    queued: bool,
}
struct Source {
    session: String,
    path: Vec<String>,
    projection: Projection,
}
pub(super) struct Actor {
    web: Web,
    ws: Socket,
    id: String,
    client: String,
    journal: Journal,
    mode: ApprovalMode,
    goals: bool,
    secrets: Vec<String>,
    sources: HashMap<String, Source>,
    requests: HashMap<String, (String, Value)>,
    sent: HashSet<String>,
    stopping: HashMap<String, Reply<Value>>,
    compacting: Option<super::compaction::Pending>,
    compact_reply: FuturesUnordered<BoxFuture<'static, Result<Value>>>,
    turns: HashMap<String, Turn>,
}
impl Actor {
    pub fn new(
        web: Web,
        ws: Socket,
        id: String,
        snapshot: &Value,
        client: String,
        opts: &SessionOptions,
        journal: Journal,
    ) -> Self {
        let mut p = Projection::default();
        p.cursor = snapshot["cursor"].as_i64();
        for row in snapshot["records"].as_array().into_iter().flatten() {
            p.observe_settlement(&row["event"]);
            match row["event"]["type"].as_str() {
                Some("turn/start") => p.active = row["event"]["data"]["turn"].as_u64(),
                Some("turn/end") => p.active = None,
                _ => {}
            }
        }
        p.start_attempt(&snapshot["assistantStream"]["activeAttempt"]);
        let root = Source {
            session: id.clone(),
            path: vec![],
            projection: p,
        };
        Self {
            web,
            ws,
            id,
            client,
            journal,
            mode: opts.approvals,
            goals: opts.settings.goals,
            secrets: crate::options::environment_secrets(&opts.env),
            sources: HashMap::from([("root".into(), root)]),
            requests: HashMap::new(),
            sent: HashSet::new(),
            stopping: HashMap::new(),
            compacting: None,
            compact_reply: FuturesUnordered::new(),
            turns: HashMap::new(),
        }
    }
    // Releasing agent-bound settings can wake input from a vendor creation
    // listener. Do this only after workspace, model, effort and goal checks.
    pub(super) async fn confirm_root_settings(
        &mut self,
        opts: &SessionOptions,
        snapshot: &Value,
    ) -> Result<()> {
        if let Some(binding) = self.web.patch.agent_binding() {
            self.web.confirm(binding.bind(&self.id)?).await?;
        } else if let Some(name) = &opts.settings.name {
            // A Web title on its own uses the public native API, including
            // installations that disable native add-ons.
            if opts.resume.is_some() {
                match snapshot.pointer("/projections/values/title") {
                    Some(Value::String(title)) if !title.is_empty() => return Ok(()),
                    Some(Value::Null) => (),
                    _ => {
                        return Err(Error::Protocol(
                            "DSH omitted its saved title projection".into(),
                        ));
                    }
                }
            }
            let receipt = self
                .web
                .request("session/rename", json!({"sessionId":self.id,"title":name}))
                .await?;
            if receipt["seq"].as_u64().is_none()
                || receipt["title"].as_str().is_none_or(str::is_empty)
            {
                return Err(Error::Protocol(
                    "DSH omitted the native title receipt".into(),
                ));
            }
        }
        Ok(())
    }
    pub(super) async fn close_web(&mut self) {
        let _ = self.web.stop().await;
    }
    pub(super) async fn initialize_goals(&mut self) -> Result<()> {
        loop {
            let frame = web::read(&mut self.ws).await?;
            let active = frame["streamId"] == "activate"
                && frame["value"]["type"] == "snapshot"
                && frame["value"]["header"]["id"] == self.id;
            self.frame(frame).await?;
            if active {
                break;
            }
        }
        self.goal_command("goals/get", Value::Null).await.map(drop)
    }
    async fn goal_command(&mut self, method: &str, args: Value) -> Result<Value> {
        let (seq, goal) = super::goals::control(&self.web, &self.id, method, args).await?;
        tokio::time::timeout(self.web.timeout, self.catch_up(seq))
            .await
            .map_err(|_| Error::Timeout("DSH goal change events".into()))??;
        if self.journal.snapshot().goal.as_ref() != Some(&goal) {
            self.journal
                .host_event(Event::GoalChanged { goal: goal.clone() });
        }
        Ok(serde_json::to_value(goal)?)
    }
    async fn catch_up(&mut self, seq: i64) -> Result<()> {
        while self.sources["root"]
            .projection
            .cursor
            .is_none_or(|c| c < seq)
        {
            let frame = web::read(&mut self.ws).await?;
            self.frame(frame).await?;
        }
        Ok(())
    }
    fn busy(&self) -> bool {
        self.compacting.is_some()
            || !self.turns.is_empty()
            || self.sources["root"].projection.active.is_some()
    }
    async fn submit(
        &mut self,
        input: Input,
        id: Uuid,
        done: Option<Reply<TurnReport>>,
        queued: bool,
    ) -> Result<()> {
        let steer = done.is_none();
        let mut req = json!({"sessionId":self.id,"requestId":id,"mode":if steer{"steer"}else{"queue"},"content":input.blocks(Protocol::Dsh)});
        let result = self
            .web
            .request("session/prompt", req.clone())
            .await
            .and_then(|res| {
                if res["accepted"] == true {
                    Ok(())
                } else {
                    Err(Error::Protocol("DSH omitted the input receipt".into()))
                }
            });
        if let Err(error) = result {
            if !steer {
                let request = self
                    .journal
                    .attempted_prompt(id, &input, false, &self.secrets);
                self.journal.prompt_error(request, &error, &self.secrets);
            }
            return Err(error);
        }
        redact(&mut req, &self.secrets);
        let text = super::projection::text(&req["content"], "text");
        let mut events = vec![];
        if queued {
            events.push(Event::InputQueued {
                input_id: id.to_string(),
            });
        } else if !steer {
            events.push(Event::PromptDispatched);
        }
        events.push(Event::UserMessage { text });
        let request = Uuid::new_v4();
        let stamp = crate::stream::RecordStamp::from(Direction::ToRuntime);
        self.journal.push(
            if steer {
                stamp
            } else {
                stamp.with_prompt_request(id, false, request)
            },
            RecordKind::Request,
            None,
            vec![],
            req,
            events,
        );
        if !steer {
            self.journal.control_decision(request, None);
        }
        self.sent.insert(id.to_string());
        if let Some(done) = done {
            self.turns.insert(
                id.to_string(),
                Turn {
                    done,
                    text: vec![],
                    failure: None,
                    queued,
                    claimed: false,
                },
            );
        }
        Ok(())
    }
    async fn interaction_result(&mut self, id: &str, outcome: Value) -> Result<()> {
        self.web
            .call(
                "$events/result",
                json!({"clientId":self.client,"eventId":id,"outcome":outcome}),
            )
            .await?;
        Ok(())
    }
    async fn interaction(&mut self, mut v: Value) -> Result<()> {
        let Some(id) = v["eventId"].as_str().map(str::to_owned) else {
            return Ok(());
        };
        if v["type"] == "cancel" {
            if self.requests.remove(&id).is_some() {
                self.journal
                    .host_event(Event::RequestResolved { id: id.into() });
            }
            return Ok(());
        }
        if v["type"] != "waterfall" || self.requests.contains_key(&id) {
            return Ok(());
        }
        let Some(source) = self.sources.values().find(|s| v["agentId"] == s.session) else {
            return self.interaction_result(&id, json!({"kind":"next"})).await;
        };
        let method = match v["event"].as_str() {
            Some("approval/request") => "dsh/approval",
            Some("user-questions/request") => "dsh/question",
            _ => return self.interaction_result(&id, json!({"kind":"next"})).await,
        };
        let body = v["request"].clone();
        self.requests.insert(id.clone(), (method.into(), body));
        redact(&mut v, &self.secrets);
        self.journal.push(
            Direction::FromRuntime,
            RecordKind::Request,
            Some(source.session.clone()),
            source.path.clone(),
            v["request"].clone(),
            vec![Event::ApprovalRequested {
                id: id.clone().into(),
                method: method.into(),
            }],
        );
        if self.mode != ApprovalMode::Host {
            let outcome = if method == "dsh/approval" {
                json!({"kind":"result","value":"rejected"})
            } else {
                json!({"kind":"rejected","error":{"name":"Error","message":"question dismissed"}})
            };
            self.answer(&id, outcome).await?;
        }
        Ok(())
    }
    async fn answer(&mut self, id: &str, outcome: Value) -> Result<()> {
        self.interaction_result(id, outcome).await?;
        self.requests.remove(id);
        self.journal
            .host_event(Event::RequestResolved { id: id.into() });
        Ok(())
    }
    async fn frame(&mut self, v: Value) -> Result<()> {
        let stream = v["streamId"].as_str().unwrap_or("");
        if stream == "source" {
            return Ok(());
        }
        if stream == "activate" {
            if v["type"] == "error" {
                return Err(Error::Refused(
                    "DSH could not activate the saved session".into(),
                ));
            }
            if v["value"]["type"] == "snapshot" {
                web::cancel(&mut self.ws, stream).await?;
            }
            return Ok(());
        }
        if v["type"] != "item" {
            if self.sources.get(stream).is_some_and(|s| !s.path.is_empty()) {
                self.sources.remove(stream);
                return Ok(());
            }
            return Err(Error::Closed(
                "DSH ended a required event subscription".into(),
            ));
        }
        let mut value = v["value"].clone();
        if stream == "interactions" {
            return self.interaction(value).await;
        }
        redact(&mut value, &self.secrets);
        if value["type"] == "snapshot" && stream != "root" {
            let source = self
                .sources
                .get(stream)
                .ok_or_else(|| Error::Protocol("unknown DSH stream".into()))?;
            if value["header"]["id"] != source.session {
                return Err(Error::Protocol(
                    "DSH child snapshot has the wrong identity".into(),
                ));
            }
            for row in value["records"].as_array().into_iter().flatten() {
                self.item(stream, row.clone()).await?;
            }
            return Ok(());
        }
        self.item(stream, value).await
    }
    async fn item(&mut self, stream: &str, value: Value) -> Result<()> {
        let Some(s) = self.sources.get_mut(stream) else {
            return Ok(());
        };
        let p = &mut s.projection;
        let (span, mut events, key) = match value["type"].as_str() {
            Some("assistant-stream") => {
                let f = &value["frame"];
                let events = p.chunk(f);
                (turn_id(&f["turn"]), events, p.stream_key(f).into())
            }
            Some("event") => {
                let e = &value["event"];
                let seq = e["seq"]
                    .as_i64()
                    .ok_or_else(|| Error::Protocol("missing DSH event sequence".into()))?;
                if p.cursor.is_some_and(|last| seq <= last) {
                    return Ok(());
                }
                if p.cursor
                    .is_some_and(|last| last.checked_add(1) != Some(seq))
                {
                    return Err(Error::Protocol("gap in the DSH session log".into()));
                }
                p.cursor = Some(seq);
                let span = turn_id(&e["data"]["turn"]).or_else(|| p.active.map(|n| n.to_string()));
                let key = if e["type"] == "assistant/message" {
                    p.message_key(&e["data"]).into()
                } else {
                    e["data"]["id"].clone()
                };
                let events = p.event(&s.session, e);
                (span, events, key)
            }
            _ => return Ok(()),
        };
        let root = s.path.is_empty();
        let mut finish = None;
        if root {
            let e = &value["event"];
            if e["type"] == "user/message"
                && let Some(id) = e["data"]["source"]["rpcId"].as_str()
            {
                if self.sent.remove(id) {
                    events.retain(|e| !matches!(e, Event::UserMessage { .. }));
                }
                if let Some(t) = self.turns.get_mut(id) {
                    t.claimed = true;
                    if t.queued {
                        events.push(Event::InputRemoved {
                            input_id: id.into(),
                        });
                    }
                }
            }
            for t in self.turns.values_mut().filter(|t| t.claimed) {
                for e in &events {
                    match e {
                        Event::Message { text } => t.text.push(text.clone()),
                        Event::TurnFailure { failure } => t.failure = Some(failure.clone()),
                        Event::TurnEnded { outcome } => finish = Some(outcome.clone()),
                        _ => {}
                    }
                }
            }
        }
        let linked = events.iter().find_map(|e| {
            if let Event::SessionLinked { child, .. } = e {
                Some(child.clone())
            } else {
                None
            }
        });
        let stopped = if root {
            events.iter().find_map(|event| {
                if let Event::Task {
                    id,
                    change: crate::TaskChange::Ended { status, .. },
                } = event
                {
                    self.stopping.remove(id).map(|ack| (ack, *status))
                } else {
                    None
                }
            })
        } else {
            None
        };
        let parent = s.session.clone();
        let mut path = s.path.clone();
        let mut raw = value;
        if !key.is_null() {
            raw["itemId"] = key;
        }
        let appended = self.journal.append(
            Direction::FromRuntime,
            RecordKind::Frame,
            Some(parent.clone()),
            path.clone(),
            span,
            raw,
            events,
        );
        if root
            && let Some(op) = &mut self.compacting
            && let Some(record) = appended
        {
            op.observe(&record);
        }
        if let Some((ack, status)) = stopped {
            let result = if status == crate::TaskStatus::Stopped {
                Ok(Value::Null)
            } else {
                Err(Error::Refused(
                    "DSH job ended without a confirmed kill".into(),
                ))
            };
            let _ = ack.send(result);
        }
        if let Some(outcome) = finish {
            for (_, t) in self.turns.extract_if(|_, t| t.claimed) {
                let _ = t.done.send(Ok(TurnReport {
                    native_input_receipt: None,
                    outcome: outcome.clone(),
                    text: t.text.join("\n"),
                    failure: t.failure,
                    structured_output: None,
                }));
            }
        }
        if let Some(child) = linked
            && !self.sources.values().any(|s| s.session == child)
        {
            let key = Uuid::new_v4().to_string();
            path.push(child.clone());
            let request = json!({"address":{"kind":"subagent","parentSessionId":parent,"childSessionId":child,"mode":"unknown"},"assistantStream":true});
            web::subscribe(
                &mut self.ws,
                &key,
                "session/follow",
                json!({"request":request}),
            )
            .await?;
            self.sources.insert(
                key,
                Source {
                    session: child,
                    path,
                    projection: Default::default(),
                },
            );
        }
        Ok(())
    }
    fn ack<T>(tx: Reply<T>, r: Result<T>) -> Result<()> {
        let unknown = matches!(
            r,
            Err(Error::Http(_) | Error::Json(_) | Error::Protocol(_) | Error::Timeout(_))
        );
        let _ = tx.send(r);
        if unknown {
            return Err(Error::Closed(
                "DSH input admission could not be confirmed".into(),
            ));
        }
        Ok(())
    }
    async fn withdraw(&mut self, id: Uuid) -> Result<()> {
        let key = id.to_string();
        let state = self
            .web
            .request("session/state", json!({"sessionId":self.id}))
            .await?;
        let native = state["queue"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["rpcId"] == key)
            .and_then(|i| i["id"].as_str())
            .ok_or_else(|| Error::Refused("DSH input is no longer queued".into()))?;
        self.web
            .request(
                "session/updateQueue",
                json!({"sessionId":self.id,"itemId":native,"action":{"kind":"remove"}}),
            )
            .await?;
        self.sent.remove(&key);
        if let Some(t) = self.turns.remove(&key) {
            let _ = t.done.send(Err(Error::Refused("input withdrawn".into())));
        }
        self.journal
            .host_event(Event::InputRemoved { input_id: key });
        Ok(())
    }
    async fn request_stop(&mut self, id: &str) -> Result<()> {
        if self.stopping.contains_key(id) {
            return Err(Error::Busy);
        }
        let task = self
            .journal
            .snapshot()
            .tasks
            .into_iter()
            .find(|t| {
                t.id == id
                    && t.source.path.is_empty()
                    && t.source.session_id.as_deref() == Some(&self.id)
                    && t.task_type == crate::TaskType::Shell
                    && !t.status.is_terminal()
            })
            .ok_or_else(|| Error::Refused("no running shell job in this DSH session".into()))?;
        let (Some(call), Some(kind)) = (task.details.tool_call_id, task.details.native_type) else {
            return Err(Error::Refused(
                "DSH omitted the initiating shell call".into(),
            ));
        };
        let records = self.journal.records();
        let invocation = records
            .iter()
            .rev()
            .filter(|r| r.agent_path.is_empty() && r.session_id.as_deref() == Some(&self.id))
            .map(|r| &r.native["event"])
            .find(|e| e["type"] == "tool/call" && e["data"]["callId"] == call);
        let background = invocation
            .filter(|e| e["data"]["name"] == kind)
            .and_then(|e| e["data"]["arguments"].as_str())
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .is_some_and(|args| args["run_in_background"] == true);
        if !background {
            return Err(Error::Refused(
                "DSH can stop only explicitly backgrounded shell calls".into(),
            ));
        }
        let receipt = self
            .web
            .request("job/kill", json!({"sessionId":self.id,"jobId":id}))
            .await?;
        if receipt["outcome"] != "requested" {
            return Err(Error::Refused("DSH job has already finished".into()));
        }
        Ok(())
    }

    async fn abort(&mut self) -> Result<Value> {
        if !self.busy() {
            return Err(Error::Refused("no active turn".into()));
        }
        if self.compacting.as_ref().is_some_and(|op| !op.can_abort()) {
            return Err(Error::Refused("no running DSH compaction to cancel".into()));
        }
        let receipt = self
            .web
            .request("session/cancel", json!({"sessionId":self.id}))
            .await?;
        if receipt["accepted"] != true {
            return Err(Error::Protocol(
                "DSH omitted the cancellation receipt".into(),
            ));
        }
        if let Some(op) = &mut self.compacting {
            op.cancel_requested = true;
        }
        Ok(receipt)
    }
    async fn start_compaction(&mut self) -> Result<crate::ManualCompaction> {
        if self.busy() {
            return Err(Error::Busy);
        }
        let state = self
            .web
            .request("session/state", json!({"sessionId":self.id}))
            .await?;
        if state["running"] != false || state["queue"].as_array().is_some_and(|q| !q.is_empty()) {
            return Err(Error::Busy);
        }
        let args = json!({"agentId":self.id,"line":"/compact","submittedAttachments":[]});
        let (op, handle) = super::compaction::Pending::start(&self.journal, &args);
        self.compacting = Some(op);
        self.compact_reply
            .push(self.web.rpc_future("commands/execute", args, None));
        Ok(handle)
    }

    async fn finish_compaction(&mut self, reply: Result<Value>) -> Result<()> {
        // The response and the WebSocket use independent connections. Drain
        // through a fixed cut which includes command/done before classifying.
        if reply.as_ref().is_ok_and(|v| v["commandId"].is_string()) {
            let state = self
                .web
                .request("session/state", json!({"sessionId":self.id}))
                .await?;
            let seq = state["cursor"]
                .as_i64()
                .ok_or_else(|| Error::Protocol("DSH omitted the post-command cursor".into()))?;
            tokio::time::timeout(self.web.timeout, self.catch_up(seq))
                .await
                .map_err(|_| Error::Timeout("DSH command evidence".into()))??;
        }
        let reply = reply.map(|mut v| {
            redact(&mut v, &self.secrets);
            v
        });
        if let Some(op) = self.compacting.take() {
            op.finish(&self.journal, reply);
        }
        Ok(())
    }

    async fn command(&mut self, c: Command) -> Result<()> {
        match c {
            Command::Prompt(id, input, a, ack, done) => {
                let result = if self.busy() {
                    Err(self.journal.busy_prompt(id, &input, false, &self.secrets))
                } else {
                    match crate::chat::Admission::commit(a).await {
                        Ok(()) => self.submit(input, id, Some(done), false).await,
                        Err(e) => Err(e),
                    }
                };
                Self::ack(ack, result)?;
            }
            Command::Steer(input_id, input, ack) => {
                let request = self.journal.steer_request(input_id, &input, &self.secrets);
                let r = if self.compacting.is_some() {
                    Err(Error::Busy)
                } else if self.busy() {
                    self.submit(input, input_id, None, false)
                        .await
                        .map(|_| Value::Null)
                } else {
                    Err(Error::Refused("no active DSH turn".into()))
                };
                self.journal.control_result(request, &r, &self.secrets);
                Self::ack(ack, r)?;
            }
            Command::Queue(id, text, ack, done) => {
                let r = if self.compacting.is_some() {
                    Err(Error::Busy)
                } else {
                    self.submit(Input::text(text), id, Some(done), true).await
                };
                Self::ack(ack, r)?;
            }
            Command::Withdraw(id, ack) => {
                let r = self.withdraw(id).await;
                let _ = ack.send(r);
            }
            Command::Abort(ack) => {
                let request = self.journal.request_abort();
                let r = self.abort().await;
                self.journal.control_result(request, &r, &self.secrets);
                let _ = ack.send(r);
            }
            Command::StopTask(id, ack) => {
                self.stopping.retain(|_, ack| !ack.is_closed());
                match self.request_stop(&id).await {
                    Ok(()) => {
                        self.stopping.insert(id, ack);
                    }
                    Err(error) => {
                        let _ = ack.send(Err(error));
                    }
                }
            }
            Command::Respond(id, reply, ack) => {
                let r = if let Some((m, v)) = id.as_str().and_then(|k| self.requests.get(k)) {
                    match reply.payload(&self.journal, &id, m, v) {
                        Ok(value) => {
                            let outcome = if m == "dsh/question" && value["cancelled"] == true {
                                json!({"kind":"rejected","error":{"name":"Error","code":"ASK_CANCELLED","message":"the user cancelled ask_user_question"}})
                            } else {
                                json!({"kind":"result","value":value})
                            };
                            self.answer(id.as_str().unwrap(), outcome).await
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(Error::Refused("unknown DSH interaction".into()))
                };
                let _ = ack.send(r);
            }
            Command::Rpc(m, args, _, ack) => {
                let r = match m.as_str() {
                    "goals/get" | "goals/set" | "goals/clear" if self.goals => {
                        self.goal_command(&m, args).await
                    }
                    "sessionFile" => self.web.session_file(&self.id).await,
                    "models" => self.web.call("session/modelCatalog", json!({})).await,
                    "commands" => {
                        self.web
                            .call("commands/list", json!({"agentId":self.id}))
                            .await
                    }
                    "skills" => {
                        self.web
                            .request("skills/list", json!({"sessionId":self.id}))
                            .await
                    }
                    _ => Err(Error::Unsupported("unknown DSH RPC".into())),
                };
                let _ = ack.send(r);
            }
            Command::Compact(_, ack) => {
                let r = self.start_compaction().await;
                let _ = ack.send(r);
            }
            Command::Send(_, ack) => {
                let _ = ack.send(Err(Error::Unsupported("raw DSH input".into())));
            }
            Command::Close(_) => unreachable!(),
        };
        Ok(())
    }
    pub async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut close = None;
        let reason = loop {
            let r = tokio::select! {
                Some(reply) = self.compact_reply.next(), if !self.compact_reply.is_empty() => self.finish_compaction(reply).await,
                packet=web::read(&mut self.ws)=>match packet{Ok(v)=>self.frame(v).await,Err(e)=>Err(e)},
                cmd=rx.recv()=>match cmd{Some(Command::Close(ack))=>{close=Some((self.journal.request_dispose(),ack));break "closed".into()},
                    Some(c)=>self.command(c).await,None=>break "controller closed".into()},
            };
            if let Err(e) = r {
                break e.to_string();
            }
        };
        if let Some(op) = self.compacting.take() {
            let why = if close.is_some() {
                crate::CompactionUnconfirmed::Disposed
            } else {
                crate::CompactionUnconfirmed::RuntimeExited
            };
            op.close(&self.journal, why);
        }
        // The native inbox still owns accepted inputs after this controller exits.
        for (_, t) in self.turns.drain() {
            let _ = t.done.send(Err(Error::Closed(reason.clone())));
        }
        let _ = self.web.session_file(&self.id).await;
        let status = self.web.stop().await;
        self.journal
            .process_exited(close.as_ref().map(|(id, _)| *id), reason, &status);
        self.journal.close();
        if let Some((_, tx)) = close {
            let _ = tx.send(status.map(|_| ()).map_err(Error::from));
        }
    }
}
