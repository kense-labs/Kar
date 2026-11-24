//! Cybion Agent API client. Each session attaches to a persistent remote thread.
use crate::{
    ApprovalMode, Direction, Error, Event, Outcome, RecordKind, Result, Session, SessionOptions,
    TurnReport, input::Input, stream::Journal, transport::Command,
};
use reqwest::{Client, Method, Url, header};
use serde_json::{Value, json};
use std::{collections::VecDeque, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Cybion {
    base: Url,
    client: Client,
}

impl Cybion {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self> {
        let base =
            Url::parse(base_url).map_err(|_| Error::Protocol("invalid Cybion URL".into()))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::Protocol(
                "expected an HTTP(S) deployment URL without credentials, query or fragment".into(),
            ));
        }
        if api_key.trim().is_empty() {
            return Err(Error::Refused("missing Cybion API key".into()));
        }
        let mut auth = header::HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| Error::Protocol("invalid Cybion authorization header".into()))?;
        auth.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { base, client })
    }

    pub fn from_env() -> Result<Self> {
        let read =
            |name| std::env::var(name).map_err(|_| Error::Refused(format!("missing {name}")));
        Self::new(&read("KAR_CYBION_BASE_URL")?, &read("KAR_CYBION_API_KEY")?)
    }

    async fn request(
        &self,
        method: Method,
        path: &[&str],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("HTTP base URL")
            .pop_if_empty()
            .extend(path);
        let mut req = self.client.request(method, url).timeout(timeout);
        if let Some(body) = body {
            req = req.json(body);
        }
        let response = req.send().await?;
        let status = response.status();
        if !status.is_success() {
            let reason = response.json::<Value>().await.unwrap_or(Value::Null);
            return Err(Error::Refused(format!(
                "Cybion HTTP {status}: {}",
                reason["error"].as_str().unwrap_or("request rejected")
            )));
        }
        Ok(response.json().await?)
    }

    pub async fn open(&self, options: SessionOptions) -> Result<Session> {
        options
            .settings
            .validate(crate::Protocol::Cybion, "cybion")?;
        self.open_thread(options, None, None).await
    }

    /// Optional title and reasoning effort are native Thread fields. A resumed thread keeps its settings.
    pub async fn open_thread(
        &self,
        options: SessionOptions,
        title: Option<&str>,
        effort: Option<&str>,
    ) -> Result<Session> {
        let journal = Journal::new(options.retained_bytes)?;
        if !options.mcp_servers.is_empty()
            || !options.env.is_empty()
            || !options.unset_env.is_empty()
            || options.approvals == ApprovalMode::Host
        {
            return Err(Error::Unsupported(
                "Cybion workers own their environment, tools and approvals".into(),
            ));
        }
        if options.resume.is_some() && title.is_some() {
            return Err(Error::Unsupported(
                "setting a title on a resumed Cybion thread".into(),
            ));
        }
        let timeout = options.handshake_timeout;
        let (info, id, seen) = tokio::time::timeout(timeout, async {
            let info = if let Some(id) = &options.resume {
                self.request(Method::GET, &["v1", "threads", id], None, timeout)
                    .await?
            } else {
                let mut fields = json!({});
                for (key, value) in [
                    ("model", options.model.as_deref()),
                    ("title", title),
                    ("reasoning_effort", effort),
                ] {
                    if let Some(value) = value {
                        fields[key] = value.into();
                    }
                }
                self.request(Method::POST, &["v1", "threads"], Some(&fields), timeout)
                    .await?
            };
            let id = info["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::Protocol("Cybion did not return a thread id".into()))?
                .to_owned();
            if options
                .resume
                .as_ref()
                .is_some_and(|requested| requested != &id)
            {
                return Err(Error::Protocol("Cybion returned a different thread".into()));
            }
            for (name, requested) in [
                ("model", options.model.as_deref()),
                ("reasoning_effort", effort),
            ] {
                if let (Some(expected), Some(actual)) = (requested, info[name].as_str())
                    && actual != expected
                {
                    return Err(Error::Refused(format!(
                        "Cybion thread {id} reports {name}={actual}, requested {expected}"
                    )));
                }
            }
            let history = self
                .request(
                    Method::GET,
                    &["v1", "threads", &id, "history"],
                    None,
                    timeout,
                )
                .await?;
            let seen = history_array(&history)?
                .iter()
                .filter_map(|v| v["id"].as_u64())
                .max()
                .unwrap_or(0);
            Ok((info, id, seen))
        })
        .await
        .map_err(|_| Error::Timeout("opening Cybion thread".into()))??;
        journal.bind_root(&id);
        let mut settings = Vec::new();
        if let Some(model) = info["model"].as_str() {
            settings.push(Event::Model {
                model: model.into(),
            });
        }
        if let Some(effort) = info["reasoning_effort"].as_str() {
            settings.push(Event::Effort {
                effort: effort.into(),
            });
        }
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            info.clone(),
            settings,
        );
        let (commands, inbox) = mpsc::channel(64);
        let owner = ThreadActor {
            api: self.clone(),
            id: id.clone(),
            journal: journal.clone(),
            seen,
            timeout,
            baseline: info["usage"].clone(),
            reported: Value::Null,
            model: info["model"].as_str().map(str::to_owned),
            turn: None,
            queue: VecDeque::new(),
        };
        tokio::spawn(owner.run(inbox));
        Ok(Session::remote(
            crate::Protocol::Cybion,
            options,
            commands,
            journal,
            id,
            info,
        ))
    }
}

fn history_array(history: &Value) -> Result<&Vec<Value>> {
    history
        .as_array()
        .ok_or_else(|| Error::Protocol("expected Cybion history array".into()))
}

fn parts(value: &Value, only_output: bool) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| !only_output || part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect()
}

pub(crate) fn history_events(row: &Value) -> Vec<Event> {
    let payload = &row["payload"];
    let call = payload["call_id"].as_str().filter(|s| !s.is_empty());
    let event = match (row["kind"].as_str(), payload["type"].as_str()) {
        (Some("input"), _) => Some(Event::UserMessage {
            text: payload["content"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| parts(&payload["content"], false)),
        }),
        (Some("response_output"), Some("message")) => {
            let text = parts(&payload["content"], true);
            (!text.is_empty()).then_some(Event::TextDelta { text })
        }
        (Some("response_output"), Some("reasoning")) => {
            let mut text = parts(&payload["summary"], false);
            if text.is_empty() {
                text = parts(&payload["content"], false);
            }
            Some(Event::ReasoningDelta { text })
        }
        (Some("response_output"), Some("function_call" | "custom_tool_call")) => {
            let Some((id, name)) = call.zip(payload["name"].as_str().filter(|n| !n.is_empty()))
            else {
                return vec![];
            };
            let mut events = vec![Event::ToolStarted {
                id: id.into(),
                name: name.into(),
            }];
            if let Some(args) = payload
                .get("arguments")
                .filter(|v| !v.is_null())
                .or_else(|| payload.get("input"))
            {
                let input = args
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| args.to_string());
                events.push(Event::ToolInput {
                    id: id.into(),
                    input: input.into(),
                });
            }
            return events;
        }
        (Some("tool_output"), _) => {
            let Some(id) = call else { return vec![] };
            let mut events = vec![];
            if let Some(output) = payload.get("output") {
                let part = match output.as_str() {
                    Some(s) => crate::OutputPart::Text { text: s.into() },
                    None => crate::OutputPart::Other {
                        value: output.clone(),
                    },
                };
                events.push(Event::ToolResult {
                    id: id.into(),
                    result: crate::ToolResult {
                        content: Some(vec![part]),
                        ..Default::default()
                    },
                });
            }
            events.push(Event::ToolEnded { id: id.into() });
            return events;
        }
        _ => None,
    };
    event.into_iter().collect()
}

type Report = oneshot::Sender<Result<TurnReport>>;
struct Active {
    done: Report,
    after: u64,
    worked: bool,
    steer_pending: bool,
    canceled: bool,
    quiet: u8,
    waiting: u8,
    failures: u8,
    due: Instant,
    interval: Duration,
    messages: Vec<String>,
}
struct ThreadActor {
    api: Cybion,
    id: String,
    journal: Journal,
    seen: u64,
    timeout: Duration,
    baseline: Value,
    reported: Value,
    model: Option<String>,
    turn: Option<Active>,
    queue: VecDeque<(Uuid, String, Report)>,
}

impl ThreadActor {
    fn record(
        &self,
        direction: impl Into<crate::stream::RecordStamp>,
        kind: RecordKind,
        native: Value,
        events: Vec<Event>,
    ) {
        self.journal
            .push(direction, kind, None, Vec::new(), native, events);
    }

    async fn submit(&self, input: Input, id: Option<Uuid>) -> Result<Value> {
        let mut body = json!({"input":input.text});
        if !input.images.is_empty() {
            body["images"] = input
                .images
                .iter()
                .map(|image| image.data_url())
                .collect::<Vec<_>>()
                .into();
        }
        let request = Uuid::new_v4();
        let stamp = crate::stream::RecordStamp::from(Direction::ToRuntime);
        self.record(
            if let Some(id) = id {
                stamp.with_prompt_request(id, false, request)
            } else {
                stamp
            },
            RecordKind::Request,
            body.clone(),
            vec![],
        );
        let answer = self
            .api
            .request(
                Method::POST,
                &["v1", "threads", &self.id, "inputs"],
                Some(&body),
                self.timeout,
            )
            .await;
        self.control_result(&answer);
        if id.is_some() {
            match &answer {
                Ok(_) => self.journal.prompt_decision(request, None),
                Err(error) => self.journal.prompt_error(request, error, &[]),
            }
        }
        answer
    }

    fn control_result(&self, answer: &Result<Value>) {
        match answer {
            Ok(native) => self.record(
                Direction::FromRuntime,
                RecordKind::Response,
                native.clone(),
                vec![],
            ),
            Err(error) => self.record(
                Direction::Host,
                RecordKind::Response,
                json!({"error":error.to_string()}),
                vec![],
            ),
        }
    }

    async fn start(&mut self, id: Uuid, input: Input, done: Report) -> Result<()> {
        let answer = match self.submit(input, Some(id)).await {
            Ok(value) => value,
            Err(error) => {
                let _ = done.send(Err(Error::Refused(error.to_string())));
                if !matches!(error, Error::Refused(_)) {
                    self.clear_queue("delivery status unknown");
                }
                return Err(error);
            }
        };
        self.turn = Some(Active {
            done,
            after: answer["record_idx"].as_u64().unwrap_or(self.seen),
            worked: false,
            steer_pending: false,
            canceled: false,
            quiet: 0,
            waiting: 0,
            failures: 0,
            due: Instant::now(),
            interval: Duration::from_millis(250),
            messages: vec![],
        });
        self.record(
            Direction::Host,
            RecordKind::Frame,
            Value::Null,
            vec![Event::TurnStarted],
        );
        Ok(())
    }

    fn clear_queue(&mut self, reason: &str) {
        for (id, _, done) in self.queue.drain(..) {
            self.journal.host_event(Event::InputDropped {
                input_id: id.to_string(),
            });
            let _ = done.send(Err(Error::Closed(reason.into())));
        }
    }

    fn finish(&mut self, native: Value, outcome: Outcome, tracker_error: Option<&str>) {
        if let Some(active) = self.turn.take() {
            let failure = (outcome == Outcome::Failed).then(|| {
                let reason = tracker_error
                    .or_else(|| native["error"].as_str().filter(|s| !s.is_empty()))
                    .unwrap_or(if native["status"] == "failed" {
                        "The Cybion Thread failed; its history holds the reason"
                    } else if native["display_status"] == "blocked" {
                        "The Cybion Thread is blocked on its owner"
                    } else {
                        "Cybion turn failed"
                    });
                crate::TurnFailure::new(crate::FailureClass::Unknown, reason)
            });
            let mut events = Vec::new();
            if let Some(failure) = failure.clone() {
                events.push(Event::TurnFailure { failure });
            }
            events.push(Event::TurnEnded {
                outcome: outcome.clone(),
            });
            self.record(Direction::FromRuntime, RecordKind::Frame, native, events);
            let _ = active.done.send(Ok(TurnReport {
                native_input_receipt: None,
                structured_output: None,
                failure,
                outcome,
                text: active.messages.join("\n"),
            }));
        }
    }

    fn observe(&mut self, info: Value, history: Value) -> Result<()> {
        if info["id"].as_str() != Some(&self.id) || info["status"].as_str().is_none() {
            return Err(Error::Protocol("invalid Cybion thread snapshot".into()));
        }
        let rows = history_array(&history)?;
        for row in rows {
            let Some(id) = row["id"].as_u64().filter(|id| *id > self.seen) else {
                continue;
            };
            self.seen = id;
            let active = self.turn.as_mut().expect("snapshot of active turn");
            if id > active.after && row["kind"] != "input" {
                active.worked = true;
                active.steer_pending = false;
                active.quiet = 0;
            }
            let events = history_events(row);
            active
                .messages
                .extend(events.iter().filter_map(|event| match event {
                    Event::TextDelta { text } => Some(text.clone()),
                    _ => None,
                }));
            self.record(
                Direction::FromRuntime,
                RecordKind::Frame,
                row.clone(),
                events,
            );
        }
        let mut usage = json!({});
        for field in ["input_tokens", "output_tokens", "cached_tokens"] {
            if let Some(total) = info["usage"][field].as_u64() {
                usage[field] = total
                    .saturating_sub(self.baseline[field].as_u64().unwrap_or_default())
                    .into();
            }
        }
        if let Some(context) = info["context_tokens"].as_u64() {
            usage["context_tokens"] = context.into();
        }
        let mut events = Vec::new();
        if usage != json!({}) && usage != self.reported {
            self.reported = usage.clone();
            if let (Some(input), Some(output)) = (
                usage["input_tokens"].as_i64(),
                usage["output_tokens"].as_i64(),
            ) {
                events.push(Event::TokenUsage {
                    reading: crate::TokenReading {
                        cache_write_by_ttl: None,
                        tokens: Some(crate::TokenTotals {
                            input,
                            output,
                            cache_read: usage["cached_tokens"].as_i64().filter(|n| *n != 0),
                            ..Default::default()
                        }),
                        total: None,
                    },
                });
            }
            events.push(Event::Usage { usage });
            if let Some(tokens) = info["context_tokens"].as_u64() {
                events.push(Event::ContextUsage {
                    usage: crate::ContextUsage::counts(Some(tokens), None, false),
                });
            }
        }
        if let Some(model) = info["model"].as_str().filter(|s| !s.is_empty())
            && self.model.as_deref() != Some(model)
        {
            self.model = Some(model.into());
            events.push(Event::Model {
                model: model.into(),
            });
        }
        if !events.is_empty() {
            self.record(
                Direction::FromRuntime,
                RecordKind::Frame,
                info.clone(),
                events,
            );
        }
        let active = self.turn.as_mut().expect("snapshot of active turn");
        active.failures = 0;
        if info["status"] == "running" {
            active.worked = true;
            active.steer_pending = false;
            active.quiet = 0;
        } else if active.worked {
            active.quiet += 1;
        } else {
            active.waiting += 1;
        }
        let outcome = if info["display_status"] == "stopped" && (active.worked || active.canceled) {
            Some(Outcome::Aborted)
        } else if !active.worked && active.waiting >= 60
            || active.steer_pending && active.quiet >= 60
        {
            Some(Outcome::Failed)
        } else if active.quiet >= 2 && !active.steer_pending {
            Some(
                if info["status"] == "failed" || info["display_status"] == "blocked" {
                    Outcome::Failed
                } else {
                    Outcome::Completed
                },
            )
        } else {
            None
        };
        if let Some(outcome) = outcome {
            let reason = if active.steer_pending {
                Some("Cybion accepted a steered input but the Thread never ran it")
            } else if !active.worked {
                Some("Cybion recorded the input but the Thread never ran a turn")
            } else {
                None
            };
            self.finish(info, outcome, reason);
        }
        Ok(())
    }

    async fn command(&mut self, command: Command) -> bool {
        match command {
            Command::Prompt(id, input, admission, ack, done) => {
                let result = if self.turn.is_some() || !self.queue.is_empty() {
                    Err(self.journal.busy_prompt(id, &input, false, &[]))
                } else {
                    match crate::chat::Admission::commit(admission).await {
                        Ok(()) => self.start(id, input, done).await,
                        Err(e) => Err(e),
                    }
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
                    .position(|entry| entry.0 == id)
                    .and_then(|index| self.queue.remove(index))
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
                let answer = if self.turn.is_some() {
                    self.submit(input, None).await
                } else {
                    Err(Error::Refused("no active turn".into()))
                };
                if let (Some(active), Ok(value)) = (&mut self.turn, &answer) {
                    active.after = active
                        .after
                        .max(value["record_idx"].as_u64().unwrap_or(self.seen));
                    active.steer_pending = true;
                    active.waiting = 0;
                    active.quiet = 0;
                    active.due = Instant::now();
                }
                let _ = ack.send(answer);
            }
            Command::Abort(ack) => {
                let result = if self.turn.is_none() {
                    Err(Error::Refused("no active turn".into()))
                } else {
                    self.record(
                        Direction::ToRuntime,
                        RecordKind::Request,
                        json!({"action":"cancel"}),
                        vec![],
                    );
                    let answer = self
                        .api
                        .request(
                            Method::POST,
                            &["api", "threads", &self.id, "cancel"],
                            Some(&json!({})),
                            self.timeout,
                        )
                        .await;
                    self.control_result(&answer);
                    if let (Some(active), Ok(view)) = (&mut self.turn, &answer) {
                        active.canceled = view["display_status"] == "stopped";
                        active.due = Instant::now();
                    }
                    answer
                };
                let _ = ack.send(result);
            }
            Command::Close(ack) => {
                let id = self.journal.request_dispose();
                self.clear_queue("detached from Cybion thread");
                // Cybion owns the turn and can finish it while a controller detaches.
                let _ = tokio::time::timeout(Duration::from_secs(3), async {
                    while self.turn.is_some() {
                        let (view, history) = self.poll_status().await?;
                        self.observe(view, history)?;
                        if self.turn.is_some() {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                        }
                    }
                    Ok::<(), Error>(())
                })
                .await;
                self.release(Some(id));
                let _ = ack.send(Ok(()));
                return false;
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
            Command::Rpc(method, _, _, ack) => {
                let _ = ack.send(Err(Error::Unsupported(method)));
            }
            Command::Send(_, ack) | Command::Respond(_, _, ack) => {
                let _ = ack.send(Err(Error::Unsupported(
                    "Cybion exposes no stdio or approval channel".into(),
                )));
            }
        }
        true
    }

    fn release(&mut self, id: Option<Uuid>) {
        if let Some(active) = self.turn.take() {
            let _ = active
                .done
                .send(Err(Error::Closed("detached from Cybion thread".into())));
        }
        self.clear_queue("detached from Cybion thread");
        let event = Event::Exited {
            reason: "detached".into(),
        };
        if let Some(id) = id {
            self.journal
                .disposed(id, json!({"type":"accepted", "action":"detach"}), event);
        } else {
            self.record(
                Direction::Host,
                RecordKind::Frame,
                json!({"action":"detach"}),
                vec![event],
            );
        }
        self.journal.close();
    }

    async fn poll_status(&self) -> Result<(Value, Value)> {
        let thread = ["v1", "threads", &self.id];
        let history = ["v1", "threads", &self.id, "history"];
        tokio::try_join!(
            self.api.request(Method::GET, &thread, None, self.timeout),
            self.api.request(Method::GET, &history, None, self.timeout)
        )
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<Command>) {
        loop {
            if self.turn.is_none()
                && let Some((id, text, done)) = self.queue.pop_front()
            {
                let _ = self.start(id, Input::text(text), done).await;
                self.journal.host_event(Event::InputRemoved {
                    input_id: id.to_string(),
                });
                continue;
            }
            let due = self.turn.as_ref().map(|turn| turn.due);
            // A control can interrupt read-only polls. Writes are serialized and never retried.
            let poll = async {
                tokio::time::sleep_until(due.expect("only polled with an active turn")).await;
                self.poll_status().await
            };
            tokio::select! {
                incoming = inbox.recv() => match incoming {
                    Some(command) => { if !self.command(command).await { break; } }
                    None => { self.release(None); break; }
                },
                result = poll, if due.is_some() => {
                    let result = result.and_then(|(view,rows)| self.observe(view,rows));
                    if let Some(active) = self.turn.as_mut() {
                        active.due = Instant::now() + active.interval;
                        active.interval = (active.interval * 3 / 2).min(Duration::from_secs(2));
                        if let Err(error) = result {
                            active.failures += 1;
                            if active.failures >= 8 {
                                self.finish(json!({"error":error.to_string()}), Outcome::Failed,Some("Cybion stopped answering; the Thread may still be running there"));
                                self.clear_queue("Cybion thread status unknown");
                            }
                        }
                    }
                }
            }
        }
    }
}
