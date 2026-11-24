mod codex_processes;
use crate::grok::turns::{INTERJECT, Turns as GrokTurns};
use crate::interaction::sensitive_field;
use crate::stream::RecordStamp;
use crate::wire::acp_prompt::{Completion, Steering, SupersedingPrompts};
use crate::{
    ApprovalMode, Direction, Error, Event, Outcome, Protocol, RecordKind, Result, stream::Journal,
    wire,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot},
};

pub(crate) const CLAUDE_COMPACT_CATALOG: &str = "kar/claudeCompactCatalog";
const GROK_COMPACT: &str = "_x.ai/compact_conversation";
fn claude_private(method: &str) -> bool {
    matches!(
        method,
        wire::usage::CLAUDE_BASELINE | CLAUDE_COMPACT_CATALOG | crate::model_request::CONFIRM
    )
}
const PRIVATE_CLAUDE_PREFIX: &str = "kar-private-";
const ANTIGRAVITY_STEER: &str = "kar/antigravitySupersedingPrompt";
const OPENCODE_V1_STEER: &str = "kar/opencodeConcurrentPrompt";
const OPENCODE_COMPACT: &str = "kar/opencodeCompact";
const CONTEXT_QUERY_PREFIX: &str = "kar-context-";
type RuntimeLines = tokio::io::Lines<BufReader<ChildStdout>>;
type BufferedLines = VecDeque<std::io::Result<String>>;
type Reply<T> = oneshot::Sender<Result<T>>;
pub(crate) enum HostAnswer {
    Native(Value),
    Checked(crate::Cursor, Value),
    Permission(crate::Cursor, crate::PermissionDecision),
    Question(crate::Cursor, crate::QuestionReply),
    Policy(crate::Cursor, crate::PolicyChoice),
    Resources(
        crate::Cursor,
        crate::PermissionDecision,
        Vec<crate::ResourceId>,
    ),
}
impl HostAnswer {
    pub(crate) fn payload(
        self,
        journal: &Journal,
        id: &Value,
        method: &str,
        body: &Value,
    ) -> Result<Value> {
        match self {
            Self::Native(value) => Ok(value),
            Self::Checked(cursor, value) if journal.has_request(id, &cursor) => Ok(value),
            Self::Question(cursor, reply) if journal.has_request(id, &cursor) => {
                crate::questions::answer(method, body, reply)
            }
            Self::Policy(cursor, choice) if journal.has_request(id, &cursor) => {
                crate::permissions::select(method, body, choice).ok_or_else(|| {
                    Error::Refused("policy option is not offered on this request".into())
                })
            }
            Self::Resources(cursor, scope, resources) if journal.has_request(id, &cursor) => {
                crate::permissions::grant(method, body, scope, &resources).ok_or_else(|| {
                    Error::Refused("invalid permission scope or resource selection".into())
                })
            }
            Self::Checked(_, _)
            | Self::Question(_, _)
            | Self::Policy(_, _)
            | Self::Resources(_, _, _) => Err(Error::Refused(
                "request is stale or belongs to a different stream".into(),
            )),
            Self::Permission(cursor, choice) => {
                if !journal.has_request(id, &cursor) {
                    return Err(Error::Refused(
                        "request is stale or belongs to a different stream".into(),
                    ));
                }
                crate::interaction::decision_payload(method, body, choice).ok_or_else(|| {
                    Error::Unsupported(
                        "this request does not offer that permission decision".into(),
                    )
                })
            }
        }
    }
}
pub(crate) enum Command {
    Compact(Option<u64>, Reply<crate::ManualCompaction>),
    Rpc(
        String,
        Value,
        Option<crate::opening::OpenStep>,
        Reply<Value>,
    ),
    Send(Value, Reply<()>),
    Prompt(
        crate::input::Input,
        Option<crate::chat::Admission>,
        Reply<()>,
        Reply<crate::TurnReport>,
    ),
    Queue(uuid::Uuid, String, Reply<()>, Reply<crate::TurnReport>),
    Withdraw(uuid::Uuid, Reply<()>),
    Abort(Reply<Value>),
    StopTask(String, Reply<Value>),
    Steer(crate::input::Input, Reply<Value>),
    Respond(Value, HostAnswer, Reply<()>),
    Close(Reply<()>),
}

#[derive(Default)]
pub(crate) struct Identity {
    pub session: Option<String>,
    pub claude_open_refusal: Option<Error>,
    pub acp_steering: Option<Steering>,
}
struct Queued {
    id: uuid::Uuid,
    text: String,
    done: Reply<crate::TurnReport>,
}
struct Turn {
    process_baseline: Option<crate::resources::turn::Baseline>,
    chat_input: Option<uuid::Uuid>,
    claude_input: Option<wire::claude_input::Input>,
    pi_inputs: Vec<u64>,
    pi_assistant: Option<u64>,
    pi_anchor: Option<String>,
    structured_output: Option<Value>,
    failure: Option<crate::TurnFailure>,
    messages: Vec<String>,
    partial: String,
    done: Reply<crate::TurnReport>,
    native: Option<String>,
    aborted: bool,
}
#[derive(Clone)]
struct SteeringReceipt {
    id: uuid::Uuid,
    owner: Option<uuid::Uuid>,
    text: String,
}
impl SteeringReceipt {
    fn for_request(
        method: &str,
        params: &Value,
        owner: Option<uuid::Uuid>,
        secrets: &[String],
    ) -> Option<Self> {
        if method == INTERJECT {
            let mut text = params["text"].as_str()?.to_owned();
            redact_text(&mut text, secrets);
            return Some(Self {
                id: params["interjectionId"].as_str()?.parse().ok()?,
                owner,
                text,
            });
        }
        if !matches!(
            method,
            wire::opencode::STEER | OPENCODE_V1_STEER | ANTIGRAVITY_STEER
        ) {
            return None;
        }
        let mut text = params["prompt"]
            .as_array()?
            .iter()
            .find_map(|block| block["text"].as_str())
            .unwrap_or_default()
            .to_owned();
        redact_text(&mut text, secrets);
        Some(Self {
            id: uuid::Uuid::new_v4(),
            owner,
            text,
        })
    }
    fn event(&self, status: crate::SteerStatus) -> Event {
        Event::SteeringInput {
            status,
            input_id: self.id,
            chat_turn: self.owner,
            text: self.text.clone(),
        }
    }
}
struct Pending {
    open: Option<crate::opening::OpenStep>,
    claude_interrupt: Option<uuid::Uuid>,
    method: String,
    reply: Option<Reply<Value>>,
    prompt_ack: Option<Reply<()>>,
    input: bool,
    starts_turn: bool,
    steer: Option<SteeringReceipt>,
}

pub(crate) struct LaunchFiles {
    pub pi_mcp: Option<crate::private_dir::PrivateDir>,
    pub dsh_patch: Option<crate::dsh::profile::Patch>,
    pub qwen_defaults: Option<crate::private_dir::PrivateDir>,
    pub prompts: Vec<tempfile::NamedTempFile>,
    pub capture: Option<crate::model_request::Owner>,
    pub pi: Option<crate::model_request::pi::Owner>,
}

pub(crate) struct Actor {
    output_schema: Option<serde_json::Map<String, Value>>,
    acp_failures: wire::failure::AcpFailures,
    superseding: SupersedingPrompts,
    grok: Option<GrokTurns>,
    grok_terminals: Option<crate::grok::terminal::Terminals>,
    end_turn_processes: bool,
    grok_interjections: HashMap<uuid::Uuid, (SteeringReceipt, crate::SteerStatus)>,
    codex_quota: wire::failure::CodexQuota,
    codex_processes: Option<codex_processes::Guard>,
    factory: Option<crate::factory::Projection>,
    usage: wire::usage::Projection,
    opencode: Option<wire::opencode::Children>,
    cursor_children: Option<wire::cursor_children::Children>,
    protocol: Protocol,
    mode: ApprovalMode,
    child: crate::process::OwnedProcess,
    stdin: Option<ChildStdin>,
    close_ack: Option<Reply<()>>,
    files: LaunchFiles,
    journal: Journal,
    identity: Arc<Mutex<Identity>>,
    next: u64,
    pending: HashMap<u64, Pending>,
    private_list_ids: HashSet<u64>,
    requests: HashMap<String, (String, Value)>,
    pi_deadlines: HashMap<String, (tokio::time::Instant, Value)>,
    // Modern Pi closes a prompt after retries and compaction have settled.
    pi_pending_end: Option<Vec<Event>>,
    pi_leaf: Option<String>,
    turn: Option<Turn>,
    queue: VecDeque<Queued>,
    secrets: Vec<String>,
    arguments: wire::argument_stream::ArgumentStreams,
    manual_compaction: Option<crate::compaction::Operation>,
    command_revision: u64,
    compactions: wire::compaction::Compactions,
    child_tasks: wire::tasks::CodexTasks,
    ancestry: HashMap<String, Vec<String>>,
    last_assistant_anchor: Option<String>,
    claude_followup: Option<uuid::Uuid>,
    claude_errors: HashMap<Vec<String>, Value>,
    claude_resume: Option<String>,
    claude_limit_reset: Option<u64>,
    claude_context: crate::context::ClaudeContext,
    claude_cancel_queued: bool,
    claude_notifications: wire::claude_input::Notifications,
    claude_steering: wire::claude_input::Steering,
    context_waiters: HashMap<u64, Vec<Reply<Value>>>,
    qwen_context: bool,
    qoder_daemon: bool,
    qoder_startup: Option<Vec<Value>>,
}
impl Actor {
    pub fn start(
        runtime: &crate::Runtime,
        options: &crate::SessionOptions,
        files: LaunchFiles,
        mut child: crate::process::OwnedProcess,
        journal: Journal,
        identity: Arc<Mutex<Identity>>,
        secrets: Vec<String>,
    ) -> mpsc::Sender<Command> {
        let stdout = child.inner().stdout.take().expect("stdout configured");
        let stdin = child.inner().stdin.take().expect("stdin configured");
        let (tx, rx) = mpsc::channel(64);
        let actor = Self {
            close_ack: None,
            protocol: runtime.protocol,
            usage: wire::usage::Projection::new(
                runtime.protocol,
                &runtime.id,
                options.resume.as_deref(),
            ),
            factory: (runtime.protocol == Protocol::Factory).then(Default::default),
            acp_failures: if runtime.protocol == Protocol::Acp {
                wire::failure::AcpFailures::new(&runtime.id)
            } else {
                wire::failure::AcpFailures::Generic
            },
            superseding: Default::default(),
            grok: (runtime.protocol == Protocol::Acp && runtime.id == "grok")
                .then(Default::default),
            grok_interjections: Default::default(),
            grok_terminals: (runtime.protocol == Protocol::Acp && runtime.id == "grok")
                .then(|| crate::grok::terminal::Terminals::new(options, child.id())),
            end_turn_processes: options.settings.end_turn_processes_on_abort,
            codex_quota: Default::default(),
            codex_processes: (runtime.protocol == Protocol::Codex
                && options.settings.end_turn_processes_on_abort)
                .then(|| codex_processes::Guard::new(child.id(), journal.clone())),
            cursor_children: (runtime.protocol == Protocol::Acp && runtime.id == "cursor")
                .then(Default::default),
            opencode: (runtime.protocol == Protocol::Acp && runtime.id == "opencode")
                .then(Default::default),
            output_schema: options.settings.output_schema.clone(),
            files,
            mode: options.approvals,
            child,
            stdin: Some(stdin),
            journal,
            identity,
            next: 0,
            pending: HashMap::new(),
            private_list_ids: HashSet::new(),
            requests: HashMap::new(),
            pi_deadlines: HashMap::new(),
            pi_pending_end: None,
            pi_leaf: None,
            turn: None,
            queue: VecDeque::new(),
            secrets,
            ancestry: HashMap::new(),
            arguments: Default::default(),
            manual_compaction: None,
            command_revision: 0,
            compactions: Default::default(),
            child_tasks: Default::default(),
            last_assistant_anchor: None,
            claude_followup: None,
            claude_errors: HashMap::new(),
            claude_resume: options
                .resume
                .clone()
                .filter(|_| runtime.protocol == Protocol::Claude),
            claude_limit_reset: None,
            claude_context: Default::default(),
            claude_cancel_queued: false,
            claude_notifications: Default::default(),
            claude_steering: Default::default(),
            context_waiters: HashMap::new(),
            qwen_context: false,
            qoder_daemon: false,
            qoder_startup: (runtime.protocol == Protocol::Qoder).then(Vec::new),
        };
        tokio::spawn(actor.run(stdout, rx));
        tx
    }
    fn observed_root_turn(&self) -> Option<&str> {
        self.child_tasks.turn(&self.session()?)
    }
    fn grok_owner(&self) -> Option<uuid::Uuid> {
        self.grok.as_ref().and_then(GrokTurns::owner)
    }
    fn session(&self) -> Option<String> {
        self.identity.lock().unwrap().session.clone()
    }
    fn source(&self, packet: &Value) -> Option<String> {
        if self.opencode.is_some()
            && let Some(notice) = wire::opencode::notice(packet, self.session().as_deref())
        {
            return Some(notice.source.to_owned());
        }
        wire::session_id(self.protocol, packet)
    }
    fn record(
        &self,
        origin: impl Into<RecordStamp>,
        kind: RecordKind,
        mut v: Value,
        mut events: Vec<Event>,
        path: Vec<String>,
    ) {
        let stamp = origin.into();
        let direction = stamp.direction;
        let session = self.source(&v).or_else(|| self.session());
        if direction == Direction::ToRuntime {
            crate::options::instructions::redact_open(self.protocol, &mut v);
        }
        if self.protocol == Protocol::Factory {
            crate::factory::redact_frame(&mut v);
        }
        redact(&mut v, &self.secrets);
        if direction == Direction::ToRuntime
            && kind == RecordKind::Request
            && !events.iter().any(|e| {
                matches!(
                    e,
                    Event::ManualCompactionRequested | Event::SteeringInput { .. }
                )
            })
            && let Some(text) = crate::observe::sent_prompt_text(&v)
        {
            events.push(Event::UserMessage { text });
        }
        if self.protocol == Protocol::Claude {
            let group = events
                .iter()
                .find_map(|event| match event {
                    Event::NotificationActivity { id, .. } => Some(*id),
                    _ => None,
                })
                .or_else(|| self.turn.as_ref()?.claude_input.as_ref().map(|i| i.id));
            self.journal.append(
                stamp,
                kind,
                session,
                path,
                group.map(|id| id.to_string()),
                v,
                events,
            );
        } else {
            self.journal.push(stamp, kind, session, path, v, events);
        }
    }
    async fn write(&mut self, packet: Value, kind: RecordKind) -> Result<()> {
        self.write_packet(packet, kind, None).await
    }
    async fn write_packet(
        &mut self,
        mut packet: Value,
        kind: RecordKind,
        explicit_events: Option<Vec<Event>>,
    ) -> Result<()> {
        if self.qoder_daemon
            && !packet["request"]["type"]
                .as_str()
                .is_some_and(|s| s.starts_with("daemon."))
        {
            packet["session_id"] = self.session().into();
        }
        let mut bytes = serde_json::to_vec(&packet)?;
        bytes.push(b'\n');
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::Closed("agent stdin closed".into()))?;
        tokio::time::timeout(Duration::from_secs(10), stdin.write_all(&bytes))
            .await
            .map_err(|_| Error::Timeout("writing to agent".into()))??;
        if (self.protocol == Protocol::Acp && packet["method"] == "session/list")
            || (self.protocol == Protocol::Pi && packet["type"] == "get_entries")
        {
            return Ok(());
        }
        if self.protocol == Protocol::Qoder
            && packet["request_id"]
                .as_str()
                .is_some_and(|s| s.starts_with(crate::qoder::PRIVATE))
        {
            return Ok(());
        }
        if self.protocol == Protocol::Claude
            && packet["request_id"]
                .as_str()
                .is_some_and(|id| id.starts_with(PRIVATE_CLAUDE_PREFIX))
        {
            return Ok(());
        }
        if self.protocol == Protocol::Claude
            && (matches!(
                packet.pointer("/request/subtype").and_then(Value::as_str),
                Some("get_status" | "get_settings" | "get_context_usage")
            ) || (packet["request"]["subtype"] == "get_usage"
                && packet["request"]["skip_behaviors"] == true))
        {
            return Ok(());
        }
        let concurrent_steer = packet["id"]
            .as_u64()
            .and_then(|id| self.pending.get(&id))
            .filter(|p| {
                matches!(p.method.as_str(), OPENCODE_V1_STEER | ANTIGRAVITY_STEER)
                    && packet["method"] == "session/prompt"
                    || p.method == INTERJECT
            })
            .and_then(|p| p.steer.as_ref());
        let claude_steer = (self.protocol == Protocol::Claude && packet["type"] == "user")
            .then(|| self.claude_steering.submitted(&packet))
            .flatten();
        let events = if let Some(events) = explicit_events {
            events
        } else if let Some(event) = claude_steer {
            vec![event]
        } else if let Some(receipt) = concurrent_steer {
            vec![receipt.event(crate::SteerStatus::Submitted)]
        } else if kind == RecordKind::Request
            && (matches!(
                packet["method"].as_str(),
                Some("turn/start" | "session/prompt")
            ) || matches!(packet["type"].as_str(), Some("user" | "prompt"))
                || (packet["method"] == "droid.add_user_message"
                    && self
                        .turn
                        .as_ref()
                        .and_then(|t| t.native.as_deref())
                        .is_some_and(|id| packet["params"]["messageId"] == id)))
        {
            vec![Event::PromptDispatched]
        } else if kind == RecordKind::Response
            && let Some(id) = packet
                .pointer("/response/request_id")
                .or_else(|| packet.get("id"))
        {
            let id = id.clone();
            if let Some((method, body)) = self.requests.get(&id.to_string()) {
                let payload = if packet["type"] == "control_response" {
                    packet.pointer_mut("/response/response")
                } else {
                    packet.get_mut("result")
                };
                if let Some(payload) = payload {
                    crate::questions::redact_answer(method, body, payload);
                }
            }
            vec![Event::RequestResolved { id }]
        } else {
            vec![]
        };
        self.record(Direction::ToRuntime, kind, packet, events, vec![]);
        Ok(())
    }
    async fn rpc(
        &mut self,
        method: &str,
        params: Value,
        reply: Option<Reply<Value>>,
        input: bool,
        starts_turn: bool,
    ) -> Result<()> {
        if method == "initialize"
            && let Some(children) = &mut self.opencode
        {
            children.extended =
                params["clientCapabilities"]["_meta"][wire::opencode::CAPABILITY] == true;
        }
        self.usage.request(method, &params);
        self.next += 1;
        let id = self.next;
        if (self.protocol == Protocol::Acp && method == "session/list")
            || (self.protocol == Protocol::Pi && matches!(method, "get_entries" | "fork" | "clone"))
        {
            self.private_list_ids.insert(id);
        }
        if method == "droid.add_user_message"
            && let Some(factory) = &mut self.factory
        {
            factory.sent(id, &params, starts_turn);
        }
        let steer = SteeringReceipt::for_request(
            method,
            &params,
            self.turn
                .as_ref()
                .and_then(|t| t.chat_input)
                .or_else(|| self.grok_owner()),
            &self.secrets,
        );
        if method == INTERJECT
            && let Some(receipt) = &steer
        {
            self.grok_interjections
                .insert(receipt.id, (receipt.clone(), crate::SteerStatus::Submitted));
        }
        self.pending.insert(
            id,
            Pending {
                open: None,
                claude_interrupt: if method == "interrupt" {
                    self.claude_notifications.interrupt_target(
                        self.turn.as_ref().and_then(|t| t.claude_input.as_ref()),
                        params["cancel_queued"] == true,
                    )
                } else {
                    None
                },
                method: method.to_owned(),
                reply,
                prompt_ack: None,
                input,
                starts_turn,
                steer,
            },
        );
        if method == ANTIGRAVITY_STEER
            || (method == "session/prompt"
                && starts_turn
                && self.identity.lock().unwrap().acp_steering == Some(Steering::SupersedingPrompt))
        {
            self.superseding.sent(id);
        }
        let wire_method = match method {
            wire::usage::CLAUDE_BASELINE => "get_usage",
            CLAUDE_COMPACT_CATALOG => "initialize",
            crate::model_request::CONFIRM => "get_settings",
            OPENCODE_COMPACT | OPENCODE_V1_STEER | ANTIGRAVITY_STEER => "session/prompt",
            _ => method,
        };
        let mut packet = wire::request(self.protocol, id, wire_method, params);
        if self.protocol == Protocol::Qoder && crate::qoder::private_method(method) {
            packet["request_id"] = format!("{}{id}", crate::qoder::PRIVATE).into();
            if method == "daemon.initialize" {
                self.qoder_daemon = true;
            }
            if method.starts_with("daemon.") || (self.qoder_daemon && method == "get_models") {
                let body = packet["request"].as_object_mut().expect("control body");
                body.remove("subtype");
                body.insert(
                    "type".into(),
                    if method == "get_models" {
                        "daemon.get_models"
                    } else {
                        method
                    }
                    .into(),
                );
            }
        }
        if claude_private(method) {
            packet["request_id"] = format!("{PRIVATE_CLAUDE_PREFIX}{id}").into();
        }
        self.write_packet(
            packet,
            RecordKind::Request,
            matches!(
                method,
                "thread/compact/start"
                    | "compact"
                    | "droid.compact_session"
                    | GROK_COMPACT
                    | OPENCODE_COMPACT
            )
            .then(|| vec![Event::ManualCompactionRequested]),
        )
        .await
    }
    fn finish(&mut self, result: Result<Outcome>) {
        self.pi_pending_end = None;
        if let Some(turn) = self.turn.take() {
            if let Some(capture) = &self.files.capture {
                capture.reader.ended(&result);
            }
            let mut messages = turn.messages;
            if !turn.partial.is_empty() {
                messages.push(turn.partial);
            }
            let _ = turn.done.send(result.map(|outcome| crate::TurnReport {
                outcome,
                structured_output: turn.structured_output,
                failure: turn.failure,
                text: messages.join("\n"),
            }));
        }
    }
    async fn dispatch(
        &mut self,
        prompt: crate::input::Input,
        input_id: Option<uuid::Uuid>,
        done: Reply<crate::TurnReport>,
    ) -> Result<()> {
        let id = self.session();
        self.claude_errors.clear();
        let claude_uuid = (self.protocol == Protocol::Claude)
            .then(|| input_id.unwrap_or_else(uuid::Uuid::new_v4));
        self.turn = Some(Turn {
            process_baseline: None,
            chat_input: input_id,
            claude_input: claude_uuid.map(wire::claude_input::Input::written),
            pi_inputs: Vec::new(),
            pi_assistant: None,
            pi_anchor: None,
            structured_output: None,
            failure: None,
            messages: Vec::new(),
            partial: String::new(),
            done,
            native: None,
            aborted: false,
        });
        match self.protocol {
            Protocol::Cybion | Protocol::Morph | Protocol::KimiKap | Protocol::Dsh => {
                return Err(Error::Protocol("expected a stdio runtime".into()));
            }
            Protocol::Factory => {
                let id = input_id.unwrap_or_else(uuid::Uuid::new_v4);
                self.turn.as_mut().expect("dispatch started a turn").native = Some(id.to_string());
                self.rpc(
                    "droid.add_user_message",
                    prompt.factory(id),
                    None,
                    true,
                    true,
                )
                .await?;
            }
            Protocol::Codex => {
                let mut args = json!({"threadId":id,"input":prompt.blocks(self.protocol)});
                if let Some(schema) = &self.output_schema {
                    args["outputSchema"] = Value::Object(schema.clone());
                }
                self.rpc("turn/start", args, None, true, true).await?;
            }
            Protocol::Acp => {
                self.acp_failures.start();
                let mut args = json!({"sessionId":id,"prompt":prompt.blocks(self.protocol)});
                if let Some(grok) = &mut self.grok {
                    args["_meta"]["promptId"] = grok.name(input_id).into();
                }
                if let Some(schema) = &self.output_schema {
                    args["_meta"]["outputSchema"] = Value::Object(schema.clone());
                }
                self.rpc("session/prompt", args, None, true, true).await?;
            }
            Protocol::Qoder => {
                self.write(
                    crate::qoder::input(id.as_deref(), &prompt.text),
                    RecordKind::Request,
                )
                .await?;
            }
            Protocol::Claude | Protocol::Qwen => {
                let msg = prompt.stream_user(
                    self.protocol,
                    id.as_deref(),
                    claude_uuid.unwrap_or_else(uuid::Uuid::new_v4),
                );
                self.write(msg, RecordKind::Request).await?;
            }
            Protocol::Pi => self.rpc("prompt", prompt.pi(), None, true, true).await?,
        }
        Ok(())
    }
    async fn codex_interrupt(&mut self, turn: &str, tx: Reply<Value>) -> Result<()> {
        self.rpc(
            "turn/interrupt",
            json!({"threadId":self.session(),"turnId":turn}),
            Some(tx),
            false,
            false,
        )
        .await?;
        if let Some(guard) = &mut self.codex_processes {
            guard.sent(turn, self.next);
        }
        Ok(())
    }
    async fn command(&mut self, c: Command) -> Result<bool> {
        let id = self.session();
        match c {
            Command::Compact(revision, tx) => {
                if tx.is_closed() {
                    return Ok(true);
                }
                if self.turn.is_some()
                    || self.claude_notifications.active()
                    || self.claude_steering.waiting()
                    || self.observed_root_turn().is_some()
                    || self.grok_owner().is_some()
                    || !self.queue.is_empty()
                    || self.manual_compaction.is_some()
                    || (self.protocol == Protocol::Codex
                        && self
                            .journal
                            .snapshot()
                            .goal
                            .flatten()
                            .is_some_and(|g| g.status == "active"))
                {
                    let _ = tx.send(Err(Error::Busy));
                    return Ok(true);
                }
                let (method, params) = match self.protocol {
                    Protocol::Codex => ("thread/compact/start", json!({"threadId":id})),
                    Protocol::Pi => ("compact", json!({})),
                    Protocol::Factory => ("droid.compact_session", json!({})),
                    Protocol::Qoder => ("user", crate::qoder::input(id.as_deref(), "/compact")),
                    Protocol::Claude => {
                        if revision != Some(self.command_revision) {
                            let _ = tx.send(Err(Error::Refused(
                                "Claude commands changed before dispatch".into(),
                            )));
                            return Ok(true);
                        }
                        (
                            "user",
                            json!({"type":"user","uuid":uuid::Uuid::new_v4(),"session_id":id,"parent_tool_use_id":null,
                            "message":{"role":"user","content":[{"type":"text","text":"/compact"}]}}),
                        )
                    }
                    Protocol::Acp
                        if matches!(self.acp_failures, wire::failure::AcpFailures::Grok { .. }) =>
                    {
                        (GROK_COMPACT, json!({"sessionId":id}))
                    }
                    Protocol::Acp if self.opencode.is_some() => (
                        OPENCODE_COMPACT,
                        json!({"sessionId":id,"prompt":[{"type":"text","text":"/compact"}]}),
                    ),
                    _ => {
                        let _ = tx.send(Err(Error::Unsupported(
                            "manual compaction through this protocol".into(),
                        )));
                        return Ok(true);
                    }
                };
                let (raw, native) = oneshot::channel();
                if matches!(self.protocol, Protocol::Claude | Protocol::Qoder) {
                    self.write_packet(
                        params,
                        RecordKind::Request,
                        Some(vec![Event::ManualCompactionRequested]),
                    )
                    .await?;
                    let _ = raw.send(Ok(Value::Null));
                } else {
                    self.rpc(method, params, Some(raw), false, false).await?;
                }
                let request = self
                    .journal
                    .latest_record()
                    .expect("native compaction request was recorded");
                let (op, handle) = crate::compaction::Operation::from_request(
                    self.protocol,
                    self.next,
                    &request,
                    native,
                );
                self.manual_compaction = Some(op);
                let _ = tx.send(Ok(handle));
            }
            Command::Close(tx) => {
                self.close_ack = Some(tx);
                return Ok(false);
            }
            Command::Rpc(method, params, open, tx) => {
                if self.protocol == Protocol::Claude
                    && method == "get_context_usage"
                    && let Some((&id, _)) = self.pending.iter().find(|(_, p)| {
                        p.method == method && p.reply.as_ref().is_some_and(|r| !r.is_closed())
                    })
                {
                    self.context_waiters.entry(id).or_default().push(tx);
                    return Ok(true);
                }
                self.rpc(&method, params, Some(tx), false, false).await?;
                self.pending
                    .get_mut(&self.next)
                    .expect("RPC was queued")
                    .open = open;
            }
            Command::StopTask(task, tx) => match self.protocol {
                Protocol::Codex => match id
                    .as_deref()
                    .ok_or_else(|| Error::Closed("session identity unavailable".into()))
                    .and_then(|root| self.child_tasks.active_child(root, &task))
                {
                    Ok(turn) => {
                        self.rpc(
                            "turn/interrupt",
                            json!({"threadId":task,"turnId":turn}),
                            Some(tx),
                            false,
                            false,
                        )
                        .await?
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                },
                Protocol::Claude | Protocol::Qoder => {
                    self.rpc("stop_task", json!({"task_id":task}), Some(tx), false, false)
                        .await?
                }
                _ => {
                    let _ = tx.send(Err(Error::Unsupported(
                        "native task stop on this adapter".into(),
                    )));
                }
            },
            Command::Send(packet, tx) => {
                self.write(packet, RecordKind::Frame).await?;
                let _ = tx.send(Ok(()));
            }
            Command::Respond(id, answer, tx) => {
                self.expire_pi_dialogs();
                let key = id.to_string();
                let Some((method, body)) = self.requests.get(&key) else {
                    let _ = tx.send(Err(Error::Refused(
                        "request is unknown or already answered".into(),
                    )));
                    return Ok(true);
                };
                let answer = match answer.payload(&self.journal, &id, method, body) {
                    Ok(value) => value,
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        return Ok(true);
                    }
                };
                let packet = wire::answer(self.protocol, &id, method, Some(answer));
                self.write(packet, RecordKind::Response).await?;
                self.requests.remove(&key);
                self.pi_deadlines.remove(&key);
                let _ = tx.send(Ok(()));
            }
            Command::Prompt(prompt, admission, tx, done) => {
                if self.turn.is_some()
                    || self.claude_notifications.active()
                    || self.claude_steering.waiting()
                    || self.observed_root_turn().is_some()
                    || self.grok_owner().is_some()
                    || self.manual_compaction.is_some()
                {
                    let _ = tx.send(Err(Error::Busy));
                    return Ok(true);
                }
                let input_id = admission.as_ref().map(crate::chat::Admission::request_id);
                if let Err(err) = crate::chat::Admission::commit(admission).await {
                    let _ = tx.send(Err(err));
                    return Ok(true);
                }
                let baseline = self
                    .codex_processes
                    .as_ref()
                    .and_then(|_| crate::resources::turn::baseline());
                self.dispatch(prompt, input_id, done).await?;
                if let Some(turn) = &mut self.turn {
                    turn.process_baseline = baseline;
                }
                if self.protocol == Protocol::Factory {
                    self.pending
                        .get_mut(&self.next)
                        .expect("Factory prompt RPC is pending")
                        .prompt_ack = Some(tx);
                } else {
                    let _ = tx.send(Ok(()));
                }
            }
            Command::Queue(_, _, tx, _)
                if self.protocol == Protocol::Codex && self.manual_compaction.is_some() =>
            {
                let _ = tx.send(Err(Error::Busy));
            }
            Command::Queue(input, text, tx, done) => {
                self.record(
                    Direction::Host,
                    RecordKind::Request,
                    json!({"type":"queue","inputId":input,"text":text}),
                    vec![Event::InputQueued {
                        input_id: input.to_string(),
                    }],
                    vec![],
                );
                self.queue.push_back(Queued {
                    id: input,
                    text,
                    done,
                });
                let _ = tx.send(Ok(()));
            }
            Command::Withdraw(input, tx) => {
                let Some(pos) = self.queue.iter().position(|e| e.id == input) else {
                    let _ = tx.send(Err(Error::Refused("input is not queued".into())));
                    return Ok(true);
                };
                let entry = self.queue.remove(pos).expect("queue position exists");
                let _ = entry.done.send(Ok(crate::TurnReport {
                    outcome: Outcome::Aborted,
                    structured_output: None,
                    failure: None,
                    text: String::new(),
                }));
                self.record(
                    Direction::Host,
                    RecordKind::Response,
                    json!({"type":"withdrawn","inputId":input}),
                    vec![Event::InputRemoved {
                        input_id: input.to_string(),
                    }],
                    vec![],
                );
                let _ = tx.send(Ok(()));
            }
            Command::Steer(_, tx) if self.manual_compaction.is_some() => {
                let _ = tx.send(Err(Error::Busy));
            }
            Command::Abort(tx) | Command::Steer(_, tx)
                if self.turn.is_none()
                    && !self.claude_notifications.active()
                    && !(self.claude_cancel_queued && self.claude_steering.waiting())
                    && self.observed_root_turn().is_none()
                    && self.grok_owner().is_none()
                    && self.manual_compaction.is_none() =>
            {
                let _ = tx.send(Err(Error::Refused("no active turn".into())));
            }
            Command::Abort(tx) => match self.protocol {
                Protocol::Cybion | Protocol::Morph | Protocol::KimiKap | Protocol::Dsh => {
                    return Err(Error::Protocol("expected a stdio runtime".into()));
                }
                Protocol::Codex => {
                    let turn_id = match self.manual_compaction.as_ref() {
                        Some(op) => op.codex_turn(),
                        None => self
                            .turn
                            .as_ref()
                            .and_then(|t| t.native.as_deref())
                            .or_else(|| self.observed_root_turn()),
                    };
                    let Some(turn_id) = turn_id else {
                        let _ = tx.send(Err(Error::Refused(
                            "runtime has not acknowledged this turn".into(),
                        )));
                        return Ok(true);
                    };
                    let turn_id = turn_id.to_owned();
                    let tx = if let Some(guard) = &mut self.codex_processes {
                        let baseline = self
                            .turn
                            .as_ref()
                            .filter(|t| t.native.as_ref() == Some(&turn_id))
                            .and_then(|t| t.process_baseline.clone());
                        guard.request(&turn_id, baseline, tx)
                    } else {
                        Some(tx)
                    };
                    if let Some(tx) = tx {
                        self.codex_interrupt(&turn_id, tx).await?;
                    }
                }
                Protocol::Claude | Protocol::Qoder | Protocol::Qwen => {
                    let cancel_queued = self.claude_cancel_queued
                        && (self.claude_steering.waiting()
                            || self
                                .turn
                                .as_ref()
                                .and_then(|t| t.claude_input.as_ref())
                                .is_some_and(|i| i.unstarted()));
                    let params = if cancel_queued {
                        json!({"cancel_queued":true})
                    } else {
                        json!({})
                    };
                    self.rpc("interrupt", params, Some(tx), false, false)
                        .await?
                }
                Protocol::Factory => {
                    self.rpc("droid.interrupt_session", json!({}), Some(tx), false, false)
                        .await?
                }
                Protocol::Pi => self.rpc("abort", json!({}), Some(tx), false, false).await?,
                Protocol::Acp => {
                    let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}});
                    if self.end_turn_processes
                        && self.manual_compaction.is_none()
                        && let Some(turn) = self
                            .grok
                            .as_ref()
                            .and_then(GrokTurns::active_prompt)
                            .map(str::to_owned)
                    {
                        self.next += 1;
                        let request_id = self.next;
                        self.write_packet(
                            cancel,
                            RecordKind::Request,
                            Some(vec![Event::TurnAbortRequested {
                                turn_id: turn.clone(),
                                request_id,
                            }]),
                        )
                        .await?;
                        self.grok_terminals
                            .as_mut()
                            .expect("Grok terminal host")
                            .aborted(&turn, request_id);
                    } else {
                        self.write(cancel, RecordKind::Request).await?;
                    }
                    if let Some(grok) = &mut self.grok {
                        grok.abort();
                    }
                    if (self.grok.is_some()
                        || self.identity.lock().unwrap().acp_steering
                            == Some(Steering::SupersedingPrompt))
                        && let Some(turn) = &mut self.turn
                    {
                        turn.aborted = true;
                    }
                    let _ = tx.send(Ok(Value::Null));
                }
            },
            Command::Steer(message, tx) => match self.protocol {
                Protocol::Claude => {
                    let Some(owner) = self
                        .claude_notifications
                        .id()
                        .or_else(|| self.turn.as_ref()?.claude_input.as_ref().map(|i| i.id))
                    else {
                        let _ = tx.send(Err(Error::Refused("no active turn".into())));
                        return Ok(true);
                    };
                    let input = uuid::Uuid::new_v4();
                    let packet = message.stream_user(Protocol::Claude, id.as_deref(), input);
                    let mut text = message.text;
                    redact_text(&mut text, &self.secrets);
                    self.claude_steering.submit(input, owner, text);
                    self.write(packet, RecordKind::Request).await?;
                    let _ = tx.send(Ok(Value::Null));
                }
                Protocol::Acp => {
                    let mode = self.identity.lock().unwrap().acp_steering;
                    let params = json!({"sessionId":id,"prompt":message.blocks(Protocol::Acp)});
                    match mode {
                        Some(Steering::GrokInterject) => {
                            if self.turn.as_ref().is_some_and(|t| t.aborted)
                                || self.grok.as_ref().is_some_and(GrokTurns::aborting)
                            {
                                let _ = tx.send(Err(Error::Refused("no active turn".into())));
                                return Ok(true);
                            }
                            let mut packet = json!({"sessionId":id,"text":message.text,"interjectionId":uuid::Uuid::new_v4()});
                            if !message.images.is_empty() {
                                packet["content"] = params["prompt"].clone();
                            }
                            self.rpc(INTERJECT, packet, Some(tx), false, false).await?;
                        }
                        Some(Steering::OpenCodeExtension) => {
                            self.rpc(wire::opencode::STEER, params, Some(tx), false, false)
                                .await?;
                        }
                        Some(mode @ (Steering::ConcurrentPrompt | Steering::SupersedingPrompt)) => {
                            let method = if mode == Steering::SupersedingPrompt {
                                if self.turn.as_ref().is_some_and(|t| t.aborted) {
                                    let _ = tx.send(Err(Error::Refused("no active turn".into())));
                                    return Ok(true);
                                }
                                ANTIGRAVITY_STEER
                            } else {
                                OPENCODE_V1_STEER
                            };
                            self.rpc(method, params, None, false, false).await?;
                            let _ = tx.send(Ok(Value::Null));
                        }
                        None => {
                            let _ = tx
                                .send(Err(Error::Unsupported("steering on this transport".into())));
                        }
                    }
                }
                Protocol::Factory => {
                    let params = message.factory(uuid::Uuid::new_v4());
                    self.rpc("droid.add_user_message", params, Some(tx), false, false)
                        .await?;
                }
                Protocol::Qoder => {
                    let result = if self.manual_compaction.is_some() {
                        Err(Error::Busy)
                    } else if self.turn.is_none() {
                        Err(Error::Refused("no active turn".into()))
                    } else {
                        self.write(
                            crate::qoder::input(id.as_deref(), &message.text),
                            RecordKind::Request,
                        )
                        .await?;
                        Ok(Value::Null)
                    };
                    let _ = tx.send(result);
                }
                Protocol::Codex => {
                    let Some(turn_id) = self
                        .turn
                        .as_ref()
                        .and_then(|t| t.native.clone())
                        .or_else(|| self.observed_root_turn().map(str::to_owned))
                    else {
                        let _ = tx.send(Err(Error::Refused(
                            "runtime has not acknowledged this turn".into(),
                        )));
                        return Ok(true);
                    };
                    self.rpc("turn/steer",json!({"threadId":id,"expectedTurnId":turn_id,"input":message.blocks(Protocol::Codex)}),Some(tx),false,false).await?;
                }
                Protocol::Pi => {
                    self.rpc("steer", message.pi(), Some(tx), false, false)
                        .await?
                }
                _ => {
                    let _ = tx.send(Err(Error::Unsupported("steering on this transport".into())));
                }
            },
        }
        Ok(true)
    }
    async fn receive(&mut self, v: Value, context: Option<crate::ContextUsage>) -> Result<()> {
        if self.protocol == Protocol::Factory
            && self
                .session()
                .zip(wire::session_id(self.protocol, &v))
                .is_some_and(|(expected, actual)| expected != actual)
        {
            return Err(Error::Protocol(
                "Factory sent a frame for another session".into(),
            ));
        }
        if self.protocol == Protocol::Qoder {
            if (self.qoder_daemon || (v["type"] == "system" && v["subtype"] == "init"))
                && v.get("session_id")
                    .is_some_and(|s| !s.is_null() && s.as_str() != self.session().as_deref())
            {
                return Err(Error::Protocol(
                    "Qoder sent a frame for another session".into(),
                ));
            }
            if v["type"] == "keep_alive" {
                return Ok(());
            }
            if let Some(id) = wire::response_id(self.protocol, &v)
                .as_ref()
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix(crate::qoder::PRIVATE))
            {
                crate::qoder::remember(&v, &mut self.secrets);
                self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
                if let Some(p) = id.parse().ok().and_then(|n: u64| self.pending.remove(&n))
                    && let Some(tx) = p.reply
                {
                    let mut result =
                        crate::qoder::private_reply(&p.method, &v, self.session().as_deref());
                    if let Ok(body) = &mut result {
                        if p.method == "initialize" {
                            // Startup may precede the private reply that supplies credential values.
                            let mut model = None;
                            for frame in self.qoder_startup.take().unwrap_or_default() {
                                if frame["type"] == "system"
                                    && frame["subtype"] == "init"
                                    && frame["parent_tool_use_id"].is_null()
                                {
                                    model = frame["model"].as_str().map(str::to_owned);
                                }
                                self.receive_record(frame, None).await?;
                            }
                            *body = json!({"model":model});
                        }
                        redact(body, &self.secrets);
                        if p.method == "get_settings"
                            && let Some(effort) = body["effort"].as_str()
                        {
                            self.journal.host_event(Event::Effort {
                                effort: effort.into(),
                            });
                        }
                    }
                    let _ = tx.send(result);
                }
                return Ok(());
            }
            if let Some(startup) = &mut self.qoder_startup {
                startup.push(v);
                return Ok(());
            }
        }
        self.receive_record(v, context).await
    }
    async fn receive_record(
        &mut self,
        mut v: Value,
        context: Option<crate::ContextUsage>,
    ) -> Result<()> {
        let received = RecordStamp::from(Direction::FromRuntime);
        let grok_turn = self
            .grok
            .as_ref()
            .and_then(GrokTurns::active_prompt)
            .map(str::to_owned);
        if let Some(method) = v["method"]
            .as_str()
            .filter(|m| crate::grok::terminal::method(m))
            && let Some(id) = v
                .get("id")
                .filter(|id| id.is_string() || id.is_number())
                .cloned()
            && self.grok_terminals.is_some()
        {
            let root = self.session();
            let p = &v["params"];
            let session = p["sessionId"].as_str().filter(|s| !s.is_empty());
            let reply = if root.is_none() || session.is_none() || p["_meta"]["isReplay"] == true {
                Some(
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"terminal needs a live session"}}),
                )
            } else {
                let owner = (root.as_deref() == session && self.manual_compaction.is_none())
                    .then(|| grok_turn.clone())
                    .flatten();
                self.grok_terminals
                    .as_mut()
                    .expect("Grok terminal host")
                    .request(method, id, p, owner)
            };
            if let Some(vars) = v.pointer_mut("/params/env").and_then(Value::as_array_mut) {
                for var in vars {
                    if let Some(value) = var.get_mut("value") {
                        *value = "[redacted]".into();
                    }
                }
            }
            self.record(received, RecordKind::Request, v, vec![], vec![]);
            if let Some(reply) = reply {
                self.write_packet(reply, RecordKind::Response, Some(vec![]))
                    .await?;
            }
            return Ok(());
        }
        if self.protocol == Protocol::Claude
            && let Some(id) = wire::claude_input::result_id(&v)
            && self
                .source(&v)
                .is_none_or(|source| self.session().as_deref() == Some(&source))
            && !self.journal.first_claude_result(id)
        {
            // Keep the raw packet observable without completing a later input or
            // accounting the same native usage a second time.
            self.record(received, RecordKind::Frame, v, vec![], vec![]);
            return Ok(());
        }
        let wire_id = wire::response_id(self.protocol, &v);
        let private = wire_id
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix(PRIVATE_CLAUDE_PREFIX));
        let number = match private {
            Some(id) => id.parse().ok(),
            None => wire_id
                .as_ref()
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())),
        };
        if self.protocol == Protocol::Claude
            && number
                .and_then(|id| self.pending.get(&id))
                .is_some_and(|p| claude_private(&p.method) != private.is_some())
        {
            return Ok(());
        }
        let response = number.and_then(|id| self.pending.remove(&id));
        if response.is_none() && number.is_some_and(|id| self.private_list_ids.contains(&id)) {
            return Ok(());
        }
        if self.protocol == Protocol::Pi
            && response
                .as_ref()
                .is_some_and(|p| matches!(p.method.as_str(), "get_entries" | "fork" | "clone"))
        {
            // Fork replies can contain the discarded next prompt, and get_entries
            // can contain other branches. Neither belongs in a session's journal.
            let p = response.unwrap();
            let result = if v["command"] != p.method {
                Err(Error::Protocol("mismatched Pi fork response".into()))
            } else if v["success"] != true {
                redact(&mut v, &self.secrets);
                Err(Error::Refused(
                    v["error"]
                        .as_str()
                        .unwrap_or("Pi refused the fork operation")
                        .into(),
                ))
            } else if p.method == "get_entries" {
                Ok(v["data"].take())
            } else {
                Ok(json!({"cancelled":v["data"]["cancelled"]}))
            };
            if let Some(tx) = p.reply {
                let _ = tx.send(result);
            }
            return Ok(());
        }
        if self.claude_resume.is_some() {
            if response.as_ref().is_some_and(|p| p.method == "initialize") {
                self.claude_resume = None;
            } else if v["type"] == "result"
                && v["subtype"] == "error_during_execution"
                && !v["parent_tool_use_id"].is_string()
                && let Some(first) = v["errors"][0].as_str()
                && (first.starts_with("No conversation found with session ID")
                    || first.contains("is not a UUID and does not match any session title"))
            {
                let missing = crate::OpenError::SessionNotFound {
                    session_id: self.claude_resume.take().unwrap(),
                    message: v["errors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    cause: crate::NativeCause {
                        method: "initialize".into(),
                        native: v,
                    },
                }
                .redacted(&self.secrets);
                self.identity.lock().unwrap().claude_open_refusal = Some(missing);
                return Err(Error::Closed("Claude declined session resume".into()));
            }
        }
        let open_error = response
            .as_ref()
            .and_then(|p| {
                p.open.as_ref()?.error(
                    self.protocol,
                    &self.acp_failures,
                    &p.method,
                    v.get("error")?,
                )
            })
            .map(|e| e.redacted(&self.secrets));
        if self.protocol == Protocol::Acp
            && response
                .as_ref()
                .is_some_and(|p| p.method == "session/list" && p.open.is_some())
        {
            // An unfiltered list can contain titles and paths from unrelated
            // workspaces. Only the directory guard consumes successful pages.
            if v.get("error").is_some() {
                redact(&mut v, &self.secrets);
            }
            if let Some(tx) = response.and_then(|p| p.reply) {
                let _ = tx.send(open_error.map_or_else(|| wire::result(self.protocol, &v), Err));
            }
            return Ok(());
        }
        if let Some(capture) = &self.files.capture {
            if response
                .as_ref()
                .is_some_and(|p| p.method == crate::model_request::CONFIRM)
            {
                if let Some(tx) = response.and_then(|p| p.reply) {
                    let _ = tx.send(capture.reader.verify(&v));
                }
                return Ok(());
            }
            if response.as_ref().is_some_and(|p| p.method == "initialize") {
                capture.reader.provider(&v);
            }
            capture.reader.observe(&v);
        }
        if private.is_some() && response.is_none() {
            // Duplicate or late private replies can still contain the full account inventory.
            return Ok(());
        }
        if self.protocol == Protocol::Claude
            && v["type"] == "system"
            && v["subtype"] == "commands_changed"
            && !v["parent_tool_use_id"].is_string()
        {
            self.command_revision += 1;
        }
        if response
            .as_ref()
            .is_some_and(|p| p.method == CLAUDE_COMPACT_CATALOG)
        {
            if let Some(tx) = response.and_then(|p| p.reply) {
                let confirmed = v
                    .pointer("/response/subtype")
                    .is_some_and(|v| v == "success")
                    && v.pointer("/response/response/commands")
                        .and_then(Value::as_array)
                        .is_some_and(|commands| {
                            let mut matches = commands
                                .iter()
                                .filter(|command| command["name"] == "compact")
                                .peekable();
                            matches.peek().is_some()
                                && matches.all(|command| command["builtin"] == true)
                        });
                let _ = tx.send(if confirmed {
                    Ok(self.command_revision.into())
                } else {
                    Err(Error::Unsupported(
                        "Claude did not confirm an unshadowed builtin compact command".into(),
                    ))
                });
            }
            return Ok(());
        }
        if response
            .as_ref()
            .is_some_and(|p| p.method == wire::usage::CLAUDE_BASELINE)
        {
            if let Some(tx) = response.and_then(|p| p.reply)
                && !tx.is_closed()
            {
                if let Ok(body) = wire::result(self.protocol, &v) {
                    self.usage.claude_baseline(&body);
                }
                let _ = tx.send(Ok(Value::Null));
            }
            return Ok(());
        }
        // Keep effective user configuration on the private RPC path. The opener needs
        // verbatim instruction text; a redacted version would change the model input.
        if self.protocol == Protocol::Codex
            && response.as_ref().is_some_and(|p| p.method == "config/read")
        {
            let result = if v.get("error").is_some() {
                redact(&mut v, &self.secrets);
                wire::result(self.protocol, &v)
            } else {
                crate::options::instructions::codex_config(&v["result"])
            };
            if let Some(tx) = response.and_then(|p| p.reply) {
                let _ = tx.send(result);
            }
            return Ok(());
        }
        // Permission replies must round-trip the vendor's original arguments, including values hidden in the UI.
        let original_request = if self.mode == ApprovalMode::Host {
            wire::host_request(self.protocol, &v)
                .map(|(_, m)| crate::interaction::request_body(&m, &v).clone())
        } else {
            None
        };
        // Sanitize session and call identities before either projection uses them. Argument
        // text is masked by its block across packet boundaries, before it enters the journal.
        if self.protocol == Protocol::Factory {
            crate::factory::redact_frame(&mut v);
        }
        let fragment = v.pointer_mut("/event/delta/partial_json").map(Value::take);
        redact(&mut v, &self.secrets);
        if let Some(fragment) = fragment {
            v["event"]["delta"]["partial_json"] = fragment;
        }
        let argument = if matches!(
            self.protocol,
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen
        ) {
            self.arguments.observe(&mut v, &self.secrets)
        } else {
            None
        };
        if self.protocol == Protocol::Claude
            && response
                .as_ref()
                .is_some_and(|p| p.method == "get_context_usage")
        {
            let result = wire::result(self.protocol, &v)
                .and_then(|b| crate::context::breakdown(&b))
                .and_then(|b| Ok(serde_json::to_value(b)?));
            let id = v
                .pointer("/response/request_id")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok());
            for tx in id
                .and_then(|i| self.context_waiters.remove(&i))
                .unwrap_or_default()
            {
                let _ = tx.send(
                    result
                        .as_ref()
                        .cloned()
                        .map_err(|e| Error::Refused(e.to_string())),
                );
            }
            if let Some(tx) = response.and_then(|p| p.reply) {
                let _ = tx.send(result);
            }
            return Ok(());
        }
        // get_settings includes merged user settings. Retain only the applied effort.
        if self.protocol == Protocol::Claude
            && response
                .as_ref()
                .is_some_and(|p| p.method == "get_settings")
        {
            let effort = v
                .pointer("/response/response/applied/effort")
                .and_then(Value::as_str);
            if let Some(value) = effort {
                self.journal.host_event(Event::Effort {
                    effort: value.into(),
                });
            }
            if let Some(tx) = response.and_then(|p| p.reply) {
                let _ =
                    tx.send(effort.map(|s| Value::String(s.into())).ok_or_else(|| {
                        Error::Refused("Claude did not report applied effort".into())
                    }));
            }
            return Ok(());
        }
        // Claude status also contains account details. Only the fork identity leaves this readback.
        if self.protocol == Protocol::Claude
            && response.as_ref().is_some_and(|p| p.method == "get_status")
        {
            if let Some(tx) = response.and_then(|p| p.reply) {
                let id = v
                    .pointer("/response/response/sections")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .flat_map(|s| s["rows"].as_array().into_iter().flatten())
                    .find(|row| row["label"] == "Session ID")
                    .and_then(|row| row["value"].as_str());
                let _ = tx.send(id.map(|s| Value::String(s.to_owned())).ok_or_else(|| {
                    Error::Refused("Claude status did not report a session id".into())
                }));
            }
            return Ok(());
        }
        let mut request = wire::host_request(self.protocol, &v);
        // Cursor uses JSON-RPC requests for these post-tool notifications and ignores the result.
        let cursor_notice = request.take_if(|(_, method)| {
            self.cursor_children.is_some()
                && matches!(
                    method.as_str(),
                    "cursor/task" | "cursor/update_todos" | "cursor/generate_image"
                )
        });
        if let Some(grok) = &mut self.grok
            && let Some(p) = &response
            && v["result"].is_object()
            && v.get("error").is_none()
        {
            let reported = v["result"]["sessionId"].as_str().filter(|s| !s.is_empty());
            let confirmed = match (p.method.as_str(), p.open.as_ref()) {
                ("session/new" | "session/fork", Some(_)) => reported,
                ("session/load" | "session/resume", Some(crate::opening::OpenStep::Target(id)))
                    if reported.is_none_or(|s| s == id) =>
                {
                    Some(id.as_str())
                }
                _ => None,
            };
            if let Some(id) = confirmed {
                grok.root = Some(id.into());
                self.identity.lock().unwrap().session = Some(id.into());
            }
        }
        let root = self.session();
        let mut events = self
            .opencode
            .as_mut()
            .and_then(|c| c.project(root.as_deref(), &v, &mut self.compactions))
            .unwrap_or_else(|| {
                let mut events = wire::project(self.protocol, &v, self.cursor_children.is_some());
                events.extend(self.compactions.observe(self.protocol, &v));
                events
            });
        if self.protocol == Protocol::Pi {
            if let Some(turn) = &mut self.turn
                && matches!(v["type"].as_str(), Some("message_start" | "message_end"))
                && let Some(stamp) = v["message"]["timestamp"].as_u64()
            {
                match v["message"]["role"].as_str() {
                    Some("user") if turn.pi_inputs.last() != Some(&stamp) => {
                        turn.pi_inputs.push(stamp)
                    }
                    Some("assistant") if v["type"] == "message_end" => {
                        turn.pi_assistant = Some(stamp)
                    }
                    _ => {}
                }
            }
            match v["type"].as_str() {
                Some("agent_end") if v["willRetry"].is_boolean() => {
                    self.pi_pending_end = Some(
                        events
                            .extract_if(.., |e| {
                                matches!(e, Event::TurnFailure { .. } | Event::TurnEnded { .. })
                            })
                            .collect(),
                    );
                }
                Some("agent_start") if self.pi_pending_end.is_some() => {
                    events.retain(|e| *e != Event::TurnStarted);
                }
                Some("agent_settled") => {
                    if let Some(end) = self.pi_pending_end.take() {
                        if v["aborted"] == true {
                            events.push(Event::TurnEnded {
                                outcome: Outcome::Aborted,
                            });
                        } else {
                            events.extend(end);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(factory) = &mut self.factory {
            events.extend(factory.notice(&v, self.turn.as_ref().and_then(|t| t.native.as_deref())));
            if v.get("error").is_some()
                && let Some(id) = wire_id.as_ref().and_then(Value::as_str)
            {
                factory.refused(id);
            }
        }
        events.extend(argument);
        if let Some(usage) = context {
            let terminal = events
                .iter()
                .position(|e| matches!(e, Event::TurnEnded { .. }))
                .unwrap_or(events.len());
            events.insert(terminal, Event::ContextUsage { usage });
        }
        if self.protocol == Protocol::Codex {
            self.codex_quota.observe(root.as_deref(), &v, &mut events);
            self.child_tasks
                .observe(self.session().as_deref(), &v, &mut events);
        }
        let cancelled = match (v["type"].as_str(), v["method"].as_str()) {
            (Some("control_cancel_request"), _) => v.get("request_id"),
            (_, Some("serverRequest/resolved")) => v.pointer("/params/requestId"),
            _ => None,
        };
        if let Some(id) = cancelled {
            self.requests.remove(&id.to_string());
            events.push(Event::RequestResolved { id: id.clone() });
        }
        if self.protocol == Protocol::Factory
            && let Some((method, ids)) = crate::factory::resolved_requests(&v)
        {
            for id in ids {
                let key = id.to_string();
                if self
                    .requests
                    .get(&key)
                    .is_some_and(|(pending, _)| pending == method)
                {
                    self.requests.remove(&key);
                    events.push(Event::RequestResolved { id: id.clone() });
                }
            }
        }
        let mut outcome_error = None;
        let reported_session = self.source(&v);
        if let Some(children) = &mut self.cursor_children {
            if response.as_ref().is_some_and(|p| p.method == "initialize") {
                children.negotiated = crate::opening::advertised(
                    &v["result"]["agentCapabilities"]["sessionCapabilities"],
                    "subagents",
                );
            }
            children.observe(root.as_deref(), &v, &mut events);
        }
        let parent = v["parent_tool_use_id"].as_str();
        let path = self
            .cursor_children
            .as_ref()
            .and_then(|children| children.path(reported_session.as_deref()?))
            .or_else(|| {
                parent.map(|p| {
                    self.ancestry
                        .get(p)
                        .cloned()
                        .unwrap_or_else(|| vec![p.to_owned()])
                })
            })
            .unwrap_or_default();
        if matches!(
            self.protocol,
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen
        ) {
            events.retain(|event| {
                let Event::ToolStarted { id, .. } = event else {
                    return true;
                };
                if self.ancestry.contains_key(id) {
                    return false;
                }
                let mut child_path = path.clone();
                child_path.push(id.clone());
                self.ancestry.insert(id.clone(), child_path);
                true
            });
        }
        let is_root = path.is_empty()
            && (reported_session.is_none()
                || self.session().is_none()
                || reported_session == self.session());
        if is_root && let Some(grok) = &mut self.grok {
            grok.observe(&v, self.turn.is_some(), &mut events);
            if v["method"] == "_x.ai/session/interjection"
                && grok.live(&v["params"])
                && let Some(id) = v["params"]["interjectionId"]
                    .as_str()
                    .and_then(|s| s.parse::<uuid::Uuid>().ok())
                && let Some((receipt, _)) = self.grok_interjections.remove(&id)
            {
                events.push(receipt.event(crate::SteerStatus::Received));
            }
        }
        if self.protocol == Protocol::Acp && is_root && self.turn.is_some() {
            self.acp_failures.observe(&v, received.timestamp_ms);
        }
        if let Some(report) = self.usage.notice(&v, &path, root.as_deref()) {
            let at = events
                .iter()
                .position(|e| matches!(e, Event::TurnEnded { .. }))
                .unwrap_or(events.len());
            events.insert(at, report);
        }
        if let Some(ref req) = request {
            events.push(Event::ApprovalRequested {
                id: req.0.clone(),
                method: req.1.clone(),
            })
        }
        let mut reply = None;
        let mut prompt_ack = None;
        if let Some(p) = response {
            let mut result =
                if p.method == crate::cursor_models::LIST && v["error"]["code"] == -32601 {
                    Err(Error::Unsupported(
                        "Cursor does not implement its parameterized model catalog".into(),
                    ))
                } else {
                    open_error.map_or_else(|| wire::result(self.protocol, &v), Err)
                };
            let supersedes = self.identity.lock().unwrap().acp_steering
                == Some(Steering::SupersedingPrompt)
                && (p.method == ANTIGRAVITY_STEER
                    || (p.method == "session/prompt" && p.starts_turn));
            let request_id = number.expect("response matched a pending numeric id");
            if p.method == "turn/interrupt"
                && let Some(guard) = &mut self.codex_processes
            {
                guard.reply(request_id, result.is_ok(), root.clone());
            }
            let refused_steer = p.method == ANTIGRAVITY_STEER
                && result.is_err()
                && self
                    .superseding
                    .refused(request_id, v["error"]["code"].as_i64());
            if let Some(steer) = p.steer {
                match (&result, p.method.as_str()) {
                    (_, INTERJECT) => {
                        let status = crate::grok::turns::interjection_reply(&mut result);
                        // An echo may already have resolved and removed this interjection.
                        if let Some((_, prior)) = self.grok_interjections.get_mut(&steer.id) {
                            *prior = status;
                            events.push(steer.event(status));
                            if status == crate::SteerStatus::Refused {
                                self.grok_interjections.remove(&steer.id);
                            }
                        }
                    }
                    (_, ANTIGRAVITY_STEER) if refused_steer => {
                        events.push(steer.event(crate::SteerStatus::Refused));
                        events.push(Event::InputDropped {
                            input_id: steer.id.to_string(),
                        });
                    }
                    (_, ANTIGRAVITY_STEER) => {
                        events.push(steer.event(crate::SteerStatus::Accepted))
                    }
                    (Ok(body), wire::opencode::STEER) => match body["outcome"].as_str() {
                        Some("injected") => events.push(steer.event(crate::SteerStatus::Accepted)),
                        Some("promptRequired") => {
                            result = Err(Error::Refused("no active turn".into()))
                        }
                        _ => {
                            result = Err(Error::Protocol(
                                "OpenCode returned an unknown steering outcome".into(),
                            ))
                        }
                    },
                    (Ok(body), OPENCODE_V1_STEER)
                        if matches!(
                            body["stopReason"].as_str(),
                            Some(
                                "end_turn"
                                    | "max_tokens"
                                    | "max_turn_requests"
                                    | "refusal"
                                    | "cancelled"
                            )
                        ) =>
                    {
                        events.push(steer.event(crate::SteerStatus::Accepted))
                    }
                    (Err(_), OPENCODE_V1_STEER) => {
                        events.push(steer.event(crate::SteerStatus::Refused));
                        events.push(Event::InputDropped {
                            input_id: steer.id.to_string(),
                        });
                    }
                    _ => {}
                }
            }
            if let Some(tx) = p.prompt_ack {
                prompt_ack = Some((
                    tx,
                    result
                        .as_ref()
                        .map(|_| ())
                        .map_err(|e| Error::Refused(e.to_string())),
                ));
            }
            if p.method == GROK_COMPACT {
                let state = match &result {
                    Ok(value) if value.is_object() => Some("completed"),
                    Err(_) => match v.pointer("/error/data/kind").and_then(Value::as_str) {
                        Some("compact_cancelled") => Some("aborted"),
                        Some("compact_failed") => Some("failed"),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(state) = state {
                    events.push(Event::Compaction {
                        state: state.into(),
                    });
                }
            }
            if self.protocol == Protocol::Codex
                && matches!(
                    p.method.as_str(),
                    "thread/goal/set" | "thread/goal/get" | "thread/goal/clear"
                )
                && let Ok(body) = &result
            {
                match crate::goal::observation(body, &p.method, self.session().as_deref()) {
                    Ok(goal) => events.push(Event::GoalChanged { goal }),
                    Err(e) => result = Err(e),
                }
            }
            if let Ok(body) = &result {
                if let Some(report) = self.usage.reply(
                    if matches!(p.method.as_str(), OPENCODE_COMPACT | ANTIGRAVITY_STEER) {
                        "session/prompt"
                    } else {
                        &p.method
                    },
                    body,
                ) {
                    events.push(report)
                }
                if self.protocol == Protocol::Qwen && p.method == "initialize" {
                    self.qwen_context = body
                        .pointer("/capabilities/can_get_context_usage")
                        .is_some_and(|v| v == true);
                }
                if self.protocol == Protocol::Qwen
                    && p.method == "get_context_usage"
                    && let Some(usage) = crate::context::qwen(body)
                {
                    events.push(Event::ContextUsage { usage });
                }
                if self.protocol == Protocol::Factory
                    && p.method == "droid.get_context_stats"
                    && let Some(usage) = crate::factory::context(body)
                {
                    events.push(Event::ContextUsage { usage });
                }
                events.extend(wire::reply_settings(
                    self.protocol,
                    &p.method,
                    body,
                    self.cursor_children.is_some(),
                ));
            }
            if p.starts_turn
                && self.protocol == Protocol::Codex
                && let (Some(t), Ok(r)) = (&mut self.turn, &result)
            {
                t.native = r
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }
            if self.protocol == Protocol::Acp
                && (p.method == "session/prompt" || p.method == ANTIGRAVITY_STEER)
            {
                let mut end = (!refused_steer).then(|| {
                    Completion::from_reply(
                        &self.acp_failures,
                        &result,
                        &v,
                        received.timestamp_ms,
                        supersedes && self.turn.as_ref().is_some_and(|t| t.aborted),
                        p.starts_turn,
                    )
                });
                if supersedes {
                    end = self.superseding.settle(request_id, end);
                    // The next quota notice must belong to the next native prompt response.
                    if result.is_ok() {
                        self.acp_failures.start();
                    }
                }
                if let Some(mut end) = end {
                    if matches!(self.acp_failures, wire::failure::AcpFailures::Grok { .. })
                        && let Ok(body) = &result
                    {
                        if let Some(usage) = crate::context::grok(body) {
                            events.push(Event::ContextUsage { usage });
                        }
                        end.grok_output(body, &mut events);
                    }
                    outcome_error = end.publish(&mut events);
                }
            } else if p.input
                && let Err(e) = &result
            {
                outcome_error = Some(Error::Refused(e.to_string()));
                let failure = crate::TurnFailure::new(
                    crate::FailureClass::Unknown,
                    v["error"]["message"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| e.to_string()),
                );
                events.push(Event::TurnFailure { failure });
                events.push(Event::TurnEnded {
                    outcome: Outcome::Failed,
                });
            }
            if p.method == "interrupt" && result.is_ok() {
                self.claude_notifications
                    .acknowledge_interrupt(p.claude_interrupt);
                if let Some(t) = &mut self.turn
                    && (self.protocol != Protocol::Claude
                        || t.claude_input.as_ref().map(|i| i.id) == p.claude_interrupt)
                {
                    t.aborted = true;
                }
            }
            if let Some(tx) = p.reply {
                reply = Some((tx, result));
            }
        }
        let reported_turn = v
            .pointer("/params/turn/id")
            .or_else(|| v.pointer("/params/turnId"))
            .and_then(Value::as_str);
        let mut current_turn = is_root
            && (self.protocol != Protocol::Codex
                || self
                    .turn
                    .as_ref()
                    .and_then(|t| t.native.as_deref())
                    .zip(reported_turn)
                    .is_none_or(|(active, reported)| active == reported));
        if current_turn
            && self.protocol == Protocol::Codex
            && v["method"] == "turn/started"
            && let Some(t) = &mut self.turn
            && t.native.is_none()
        {
            t.native = reported_turn.map(str::to_owned);
        }
        if self.protocol == Protocol::Claude {
            if is_root {
                if v["type"] == "system"
                    && v["subtype"] == "init"
                    && let Some(capabilities) = v["capabilities"].as_array()
                {
                    self.claude_cancel_queued = capabilities
                        .iter()
                        .any(|c| c == "interrupt_cancel_queued_v1");
                }
                if let Some(input) = self.turn.as_mut().and_then(|t| t.claude_input.as_mut()) {
                    input.observe(&v, &mut events);
                }
                if self.manual_compaction.is_none() {
                    let input = self.turn.as_ref().and_then(|t| t.claude_input.as_ref());
                    let steering = self.claude_steering.observe(
                        &v,
                        input.map(|i| i.id),
                        self.claude_notifications.id(),
                    );
                    self.claude_notifications
                        .observe(input, &v, &steering, &mut events);
                    // A late receipt belongs to the activity that submitted it. Keep it
                    // separate from a model frame that may close an entirely different turn.
                    for event in steering.updates {
                        self.journal.host_event(event);
                    }
                    events.extend(steering.moved);
                    if let Some((input_id, from)) = steering.new_turn {
                        self.claude_followup = Some(input_id);
                        self.last_assistant_anchor = None;
                        events.push(Event::SteeringTurnStarted { input_id, from });
                    }
                    if v["type"] == "result"
                        && let Some(group) = events
                            .iter()
                            .find_map(|e| match e {
                                Event::NotificationActivity { id } => Some(*id),
                                _ => None,
                            })
                            .or_else(|| input.map(|i| i.id))
                    {
                        self.claude_steering.ended(group);
                    }
                }
                current_turn &= !events
                    .iter()
                    .any(|e| matches!(e, Event::NotificationActivity { .. }));
            } else if !path.is_empty()
                && (reported_session.is_none() || reported_session == self.session())
            {
                self.claude_notifications.child(&path, &mut events);
            }
            if is_root
                || (!path.is_empty()
                    && (reported_session.is_none() || reported_session == self.session()))
            {
                self.claude_notifications.track_tools(&events);
            }
            if is_root && let Some(usage) = self.claude_context.observe(&v) {
                let i = events
                    .iter()
                    .position(|e| matches!(e, Event::TurnEnded { .. }))
                    .unwrap_or(events.len());
                events.insert(i, Event::ContextUsage { usage });
            }
            match v["type"].as_str() {
                Some("rate_limit_event") if is_root => {
                    self.claude_limit_reset = wire::failure::claude_reset(&v["rate_limit_info"]);
                }
                Some("assistant") if self.turn.is_some() || self.claude_notifications.active() => {
                    if v["is_api_error_message"] == true
                        || v["error"].is_string()
                        || v.pointer("/message/model")
                            .is_some_and(|m| m == "<synthetic>")
                        || v.pointer("/message/stop_reason")
                            .is_some_and(|r| r == "refusal")
                    {
                        self.claude_errors.insert(path.clone(), v.clone());
                    } else {
                        self.claude_errors.remove(&path);
                    }
                }
                Some("result") => {
                    let last = self.claude_errors.remove(&path).unwrap_or(Value::Null);
                    if let Some(failure) = wire::failure::claude(&v, &last, self.claude_limit_reset)
                        && let Some(i) = events
                            .iter()
                            .position(|e| matches!(e, Event::TurnEnded { .. }))
                    {
                        events[i] = Event::TurnEnded {
                            outcome: Outcome::Failed,
                        };
                        events.insert(i, Event::TurnFailure { failure });
                    }
                }
                _ => {}
            }
        }
        if matches!(
            self.protocol,
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen
        ) && events.iter().any(|e| matches!(e, Event::TurnEnded { .. }))
            && ((current_turn && self.turn.as_ref().is_some_and(|t| t.aborted))
                || (is_root && self.claude_notifications.was_interrupted(&events)))
        {
            events.retain(|e| !matches!(e, Event::TurnFailure { .. }));
            for e in &mut events {
                if let Event::TurnEnded { outcome } = e {
                    *outcome = Outcome::Aborted;
                }
            }
        }
        if matches!(
            self.protocol,
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen
        ) && v["type"] == "result"
            && let Some(i) = events.iter().position(|e| {
                matches!(
                    e,
                    Event::TurnEnded {
                        outcome: Outcome::Completed
                    }
                )
            })
            && let Some(value) = v.get(if self.protocol != Protocol::Qwen {
                "structured_output"
            } else {
                "structured_result"
            })
        {
            events.insert(
                i,
                Event::StructuredOutput {
                    value: value.clone(),
                },
            );
        }
        if current_turn && let Some(turn) = &mut self.turn {
            for event in &events {
                match event {
                    Event::StructuredOutput { value } => {
                        turn.structured_output = Some(value.clone())
                    }
                    Event::TurnFailure { failure } => turn.failure = Some(failure.clone()),
                    Event::TextDelta { text } => turn.partial.push_str(text),
                    Event::Message { text } => {
                        turn.partial.clear();
                        turn.messages.push(text.clone());
                    }
                    _ => {}
                }
            }
        }
        let outcome = events.iter().find_map(|e| match e {
            Event::TurnEnded { outcome } => Some(outcome.clone()),
            _ => None,
        });
        let dropped = events.iter().find_map(|e| match e {
            Event::NativeInput { receipt, .. } => receipt.dropped_outcome(),
            _ => None,
        });
        if is_root {
            let followup = self.claude_followup.is_some_and(|wanted| {
                events
                    .iter()
                    .any(|e| matches!(e, Event::NotificationActivity { id } if *id == wanted))
            });
            if followup && outcome.is_some() {
                self.claude_followup = None;
            }
            let anchor = match self.protocol {
                Protocol::Pi if outcome == Some(Outcome::Completed) => {
                    self.turn.as_mut().and_then(|t| t.pi_anchor.take())
                }
                Protocol::Codex if outcome.is_some() => v
                    .pointer("/params/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                Protocol::Claude if current_turn || followup => {
                    let blocks = v.pointer("/message/content").and_then(Value::as_array);
                    if (v["type"] == "assistant"
                        && blocks.is_some_and(|bs| bs.iter().any(|b| b["type"] == "tool_use")))
                        || (v["type"] == "user"
                            && blocks
                                .is_some_and(|bs| bs.iter().any(|b| b["type"] == "tool_result")))
                    {
                        self.last_assistant_anchor = None;
                    } else if v["type"] == "assistant"
                        && let Some(id) = v["uuid"].as_str()
                    {
                        self.last_assistant_anchor = Some(id.to_owned());
                    }
                    outcome.as_ref().and_then(|o| {
                        self.last_assistant_anchor
                            .take()
                            .filter(|_| *o != Outcome::Aborted)
                    })
                }
                _ => None,
            };
            if let Some(anchor) = anchor {
                events.push(Event::ForkAnchor { anchor });
            }
        }
        if current_turn
            && self.turn.is_some()
            && outcome.is_some()
            && let Some(grok) = &mut self.grok
        {
            grok.host_ended(&mut events);
        }
        self.record(
            received,
            wire::classify(self.protocol, &v),
            v.clone(),
            events,
            path,
        );
        if current_turn
            && let Some(outcome) = &outcome
            && let Some(turn) = grok_turn
            && let Some(terminals) = &mut self.grok_terminals
        {
            terminals.ended(&turn, outcome, root.clone(), self.journal.clone());
        }
        if is_root
            && v["method"] == "turn/completed"
            && root
                .as_deref()
                .is_some_and(|id| v["params"]["threadId"] == id)
            && let Some(guard) = &mut self.codex_processes
        {
            let turn = &v["params"]["turn"];
            guard.ended(
                turn["id"].as_str(),
                turn["status"].as_str().unwrap_or("unknown"),
                root.clone(),
            );
        }
        if is_root
            && let Some(op) = self.manual_compaction.as_mut()
            && let Some(frame) = self.journal.latest_record()
            && op.observe(&frame)
        {
            op.publish(&self.journal);
            if !op.report.status.is_pending() {
                self.manual_compaction = None;
            }
        }
        if current_turn && let Some(outcome) = outcome.or(dropped) {
            self.finish(outcome_error.map_or(Ok(outcome), Err));
            let mut started = Vec::new();
            if let Some(grok) = &mut self.grok {
                grok.publish(&mut started);
            }
            if !started.is_empty() {
                self.record(
                    Direction::Host,
                    RecordKind::Frame,
                    Value::Null,
                    started,
                    vec![],
                );
            }
        }
        if let Some((tx, result)) = prompt_ack {
            let _ = tx.send(result);
        }
        if let Some((tx, result)) = reply {
            let _ = tx.send(result);
        }
        if let Some((id, method)) = cursor_notice {
            self.write_packet(
                wire::answer(self.protocol, &id, &method, Some(json!({}))),
                RecordKind::Response,
                Some(Vec::new()),
            )
            .await?;
        }
        if let Some((id, method)) = request {
            if self.mode == ApprovalMode::Host {
                if self.protocol == Protocol::Pi {
                    let key = id.to_string();
                    self.pi_deadlines.remove(&key);
                    if let Some(ms) = v["timeout"].as_u64().filter(|ms| *ms > 0)
                        && let Some(at) =
                            tokio::time::Instant::now().checked_add(Duration::from_millis(ms))
                    {
                        self.pi_deadlines.insert(key, (at, id.clone()));
                    }
                }
                self.requests.insert(
                    id.to_string(),
                    (method, original_request.unwrap_or(Value::Null)),
                );
            } else {
                self.write(
                    wire::answer(
                        self.protocol,
                        &id,
                        &method,
                        (self.protocol == Protocol::Factory
                            && method == "droid.request_permission")
                            .then(|| {
                                crate::interaction::decision_payload(
                                    &method,
                                    &v["params"],
                                    crate::PermissionDecision::Cancel,
                                )
                            })
                            .flatten(),
                    ),
                    RecordKind::Response,
                )
                .await?;
            }
        }
        Ok(())
    }
    fn expire_pi_dialogs(&mut self) {
        let now = tokio::time::Instant::now();
        let expired: Vec<_> = self
            .pi_deadlines
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(key, (_, id))| (key.clone(), id.clone()))
            .collect();
        for (key, id) in expired {
            self.pi_deadlines.remove(&key);
            if self.requests.remove(&key).is_some() {
                self.record(
                    Direction::Host,
                    RecordKind::Frame,
                    json!({"type":"extension_ui_timeout","id":id}),
                    vec![Event::RequestResolved { id }],
                    vec![],
                );
            }
        }
    }
    // Pi and Qwen expose the latest context via RPC. Attach the private readback to
    // the original terminal event, preserving subsequent wire order and bounding the wait.
    async fn turn_context(
        &mut self,
        packet: &Value,
        lines: &mut RuntimeLines,
        buffer: &mut BufferedLines,
    ) -> Option<crate::ContextUsage> {
        let pi = self.protocol == Protocol::Pi
            && ((packet["type"] == "agent_end" && !packet["willRetry"].is_boolean())
                || (packet["type"] == "agent_settled" && self.pi_pending_end.is_some()));
        let qwen = self.protocol == Protocol::Qwen
            && self.qwen_context
            && packet["type"] == "result"
            && !packet["parent_tool_use_id"].is_string();
        let factory = self.protocol == Protocol::Factory
            && ((packet["method"] == "droid.session_notification"
                && packet["params"]["notification"]["type"] == "agent_turn_completed"
                && self.session().is_some_and(|id| {
                    packet["params"]["sessionId"]
                        .as_str()
                        .is_none_or(|reported| reported == id)
                })
                && self
                    .turn
                    .as_ref()
                    .and_then(|t| t.native.as_deref())
                    .is_some_and(|id| packet["params"]["notification"]["turnId"] == id))
                || (self.manual_compaction.is_some()
                    && wire::response_id(self.protocol, packet)
                        .and_then(|id| id.as_str()?.parse::<u64>().ok())
                        .and_then(|id| self.pending.get(&id))
                        .is_some_and(|p| p.method == "droid.compact_session")
                    && self
                        .session()
                        .is_some_and(|id| packet["result"]["newSessionId"] == id)
                    && packet["result"]["removedCount"]
                        .as_u64()
                        .is_some_and(|n| n > 0)));
        if !pi && !qwen && !factory {
            return None;
        }
        let id = format!("{CONTEXT_QUERY_PREFIX}{}", uuid::Uuid::new_v4());
        let query = if factory {
            crate::factory::request(&id, "droid.get_context_stats", json!({}))
        } else if pi {
            json!({"type":"get_session_stats","id":id})
        } else {
            json!({"type":"control_request","request_id":id,"request":{"subtype":"get_context_usage","show_details":false}})
        };
        let body = self.private_readback(&query, &id, lines, buffer).await?;
        if factory {
            return crate::factory::context(&body);
        }
        if !pi {
            return crate::context::qwen(&body);
        }
        if packet["type"] == "agent_settled"
            && packet["aborted"] == false
            && self.session().is_some_and(|root| body["sessionId"] == root)
            && let Some((users, assistant)) = self
                .turn
                .as_ref()
                .and_then(|t| Some((t.pi_inputs.clone(), t.pi_assistant?)))
            && !users.is_empty()
        {
            let id = format!("{CONTEXT_QUERY_PREFIX}{}", uuid::Uuid::new_v4());
            let mut query = json!({"type":"get_entries","id":id});
            if let Some(since) = &self.pi_leaf {
                query["since"] = json!(since);
            }
            if let Some(entries) = self.private_readback(&query, &id, lines, buffer).await {
                let anchor = crate::pi_fork::settled_anchor(&entries, &users, assistant);
                self.pi_leaf = entries["leafId"].as_str().map(str::to_owned);
                if let Some(t) = &mut self.turn {
                    t.pi_anchor = anchor;
                }
            } else {
                self.pi_leaf = None;
            }
        }
        body.get("contextUsage")
            .filter(|v| v.is_object())
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    async fn private_readback(
        &mut self,
        query: &Value,
        id: &str,
        lines: &mut RuntimeLines,
        buffer: &mut BufferedLines,
    ) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut bytes = serde_json::to_vec(query).ok()?;
        bytes.push(b'\n');
        let stdin = self.stdin.as_mut()?;
        // A cancelled write() sends zero bytes. After a short write, finish its JSON
        // line or close the pipe before another command can be appended to it.
        let written = tokio::time::timeout_at(deadline, stdin.write(&bytes))
            .await
            .ok()?
            .ok()?;
        if written == 0 {
            return None;
        }
        if written < bytes.len()
            && !matches!(
                tokio::time::timeout_at(deadline, stdin.write_all(&bytes[written..])).await,
                Ok(Ok(()))
            )
        {
            self.stdin.take();
            return None;
        }
        tokio::time::timeout_at(deadline, async {
            let mut size = 0;
            while buffer.len() < 128 && size < 1024 * 1024 {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => return None,
                    Err(e) => {
                        buffer.push_back(Err(e));
                        return None;
                    }
                };
                if let Ok(mut reply) = serde_json::from_str::<Value>(&line)
                    && wire::response_id(self.protocol, &reply)
                        .as_ref()
                        .and_then(Value::as_str)
                        == Some(id)
                {
                    if self.protocol == Protocol::Pi {
                        return (reply["command"] == query["type"] && reply["success"] == true)
                            .then(|| reply["data"].take());
                    }
                    return wire::result(self.protocol, &reply).ok();
                }
                size += line.len();
                buffer.push_back(Ok(line));
            }
            None
        })
        .await
        .ok()
        .flatten()
    }
    async fn stop_child(&mut self) {
        if self.files.pi_mcp.is_some()
            || (matches!(
                self.protocol,
                Protocol::Qwen | Protocol::Qoder | Protocol::Factory
            ) && self.turn.is_none())
        {
            self.stdin.take();
            if matches!(
                tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await,
                Ok(Ok(_))
            ) {
                return;
            }
        }
        self.child.stop().await;
    }
    async fn run(mut self, stdout: ChildStdout, mut commands: mpsc::Receiver<Command>) {
        let mut lines = BufReader::new(stdout).lines();
        let mut buffered = BufferedLines::new();
        let reason = loop {
            if self.turn.is_none()
                && !self.claude_notifications.active()
                && !self.claude_steering.waiting()
                && self.observed_root_turn().is_none()
                && self.grok_owner().is_none()
                && self.manual_compaction.is_none()
                && let Some(input) = self.queue.pop_front()
            {
                self.record(
                    Direction::Host,
                    RecordKind::Response,
                    json!({"type":"dequeued","inputId":input.id}),
                    vec![
                        Event::InputRemoved {
                            input_id: input.id.to_string(),
                        },
                        Event::PromptDispatched,
                    ],
                    vec![],
                );
                if let Err(e) = self
                    .dispatch(
                        crate::input::Input::text(input.text),
                        Some(input.id),
                        input.done,
                    )
                    .await
                {
                    break e.to_string();
                }
            }
            let terminal_reply = self
                .grok_terminals
                .as_ref()
                .is_some_and(crate::grok::terminal::Terminals::pending);
            let preparing_abort = self
                .codex_processes
                .as_ref()
                .is_some_and(codex_processes::Guard::reading);
            let deadline = self.pi_deadlines.values().map(|(at, _)| *at).min();
            tokio::select! {
                () = async { if let Some(at)=deadline { tokio::time::sleep_until(at).await; } else { std::future::pending::<()>().await; } } => self.expire_pi_dialogs(),
                prepared = async { self.codex_processes.as_mut().expect("enabled guard").prepared().await }, if preparing_abort => {
                    if let Some((turn, waiting)) = prepared {
                        let mut result = Ok(());
                        for tx in waiting {
                            result = self.codex_interrupt(&turn, tx).await;
                            if result.is_err() { break; }
                        }
                        if let Err(error) = result { break error.to_string(); }
                    }
                }
                reply = async { self.grok_terminals.as_mut().expect("Grok terminal host").reply().await }, if terminal_reply => {
                    if let Some(reply) = reply && let Err(e) = self.write_packet(reply, RecordKind::Response, Some(vec![])).await {
                        break e.to_string();
                    }
                }
                command = commands.recv() => {
                    let Some(command)=command else{break "last session handle dropped".into()};
                    match self.command(command).await{
                        Ok(true)=>{},
                        Ok(false)=>break "closed by host".into(),
                        Err(error)=>break error.to_string(),
                    }
                }
                line = async {
                    match buffered.pop_front() {Some(line)=>line.map(Some),None=>lines.next_line().await}
                } => {
                    let line=match line{
                        Ok(Some(line))=>line,
                        Ok(None)=>break "runtime closed stdout".into(),
                        Err(error)=>break error.to_string(),
                    };
                    if line.trim().is_empty(){continue;}
                    let packet=match serde_json::from_str(&line){
                        Ok(packet)=>packet,
                        Err(error)=>break format!("invalid runtime JSON at column {}",error.column()),
                    };
                    if wire::response_id(self.protocol,&packet).as_ref().and_then(Value::as_str).is_some_and(|id|id.starts_with(CONTEXT_QUERY_PREFIX)) {continue;}
                    let context=self.turn_context(&packet,&mut lines,&mut buffered).await;
                    if let Err(error)=self.receive(packet,context).await{break error.to_string();}
                }
            }
        };
        for receipt in self
            .pending
            .values()
            .filter(|p| matches!(p.method.as_str(), ANTIGRAVITY_STEER | OPENCODE_V1_STEER))
            .filter_map(|p| p.steer.as_ref())
        {
            self.record(
                Direction::Host,
                RecordKind::Frame,
                Value::Null,
                vec![receipt.event(crate::SteerStatus::Unconfirmed)],
                vec![],
            );
        }
        for (receipt, status) in std::mem::take(&mut self.grok_interjections).into_values() {
            if !status.is_waiting() {
                continue;
            }
            self.record(
                Direction::Host,
                RecordKind::Frame,
                Value::Null,
                vec![receipt.event(crate::SteerStatus::Unconfirmed)],
                vec![],
            );
        }
        for event in std::mem::take(&mut self.claude_steering).disconnect() {
            self.record(
                Direction::Host,
                RecordKind::Frame,
                Value::Null,
                vec![event],
                vec![],
            );
        }
        if let Some(mut op) = self.manual_compaction.take() {
            op.report = op.report.unconfirmed(if self.close_ack.is_some() {
                crate::CompactionUnconfirmed::Disposed
            } else {
                crate::CompactionUnconfirmed::RuntimeExited
            });
            op.publish(&self.journal);
        }
        if let Some(capture) = &self.files.capture {
            capture.reader.close();
        }
        if let Some(pi) = &self.files.pi {
            pi.reader.close();
        }
        let session_id = self.session();
        if let Some(terminals) = &mut self.grok_terminals {
            terminals.close(session_id.clone(), &self.journal).await;
        }
        if let Some(guard) = &mut self.codex_processes {
            guard.exited(session_id).await;
        }
        self.stop_child().await;
        self.files.capture.take();
        self.files.pi.take();
        self.files.pi_mcp.take();
        self.files.qwen_defaults.take();
        self.files.dsh_patch.take();
        self.files.prompts.clear();
        let reason = self
            .secrets
            .iter()
            .fold(reason, |msg, key| msg.replace(key, "[redacted]"));
        self.record(
            Direction::Host,
            RecordKind::Response,
            json!({"type":"exited","reason":reason}),
            vec![Event::Exited {
                reason: reason.clone(),
            }],
            vec![],
        );
        self.finish(Err(Error::Closed(reason.clone())));
        for p in self.pending.into_values() {
            if let Some(tx) = p.reply {
                let _ = tx.send(Err(Error::Closed(reason.clone())));
            }
        }
        for input in self.queue {
            let _ = input.done.send(Err(Error::Closed(reason.clone())));
        }
        self.journal.close();
        if let Some(tx) = self.close_ack {
            let _ = tx.send(Ok(()));
        }
    }
}

pub(crate) fn redact(v: &mut Value, secrets: &[String]) {
    redact_value(v, secrets, false)
}

pub(crate) fn redact_native_error(v: &mut Value, secrets: &[String]) {
    redact_value(v, secrets, true)
}

fn redact_text(text: &mut String, secrets: &[String]) {
    for secret in secrets {
        if text.contains(secret) {
            *text = text.replace(secret, "[redacted]");
        }
    }
}

fn redact_value(v: &mut Value, secrets: &[String], error_fields: bool) {
    match v {
        Value::String(text) => redact_text(text, secrets),
        Value::Array(values) => {
            for val in values {
                redact_value(val, secrets, error_fields)
            }
        }
        Value::Object(obj) => {
            for (k, val) in obj {
                if matches!(k.as_str(), "requestedSchema" | "requested_schema")
                    && val["properties"].is_object()
                {
                    // Property names describe fields, including secret fields; keep their schema
                    // usable by remote forms while withholding any supplied secret defaults.
                    let mut properties = val.get_mut("properties").unwrap().take();
                    redact_value(val, secrets, error_fields);
                    for (key, definition) in properties.as_object_mut().unwrap() {
                        if (sensitive_field(key)
                            || definition["writeOnly"] == true
                            || definition["format"] == "password")
                            && let Some(fields) = definition.as_object_mut()
                        {
                            fields.remove("default");
                            fields.remove("examples");
                        }
                        redact_value(definition, secrets, error_fields);
                    }
                    val["properties"] = properties;
                } else if sensitive_field(k)
                    || (error_fields
                        && matches!(
                            k.to_ascii_lowercase().as_str(),
                            "token" | "cookie" | "set-cookie" | "client_secret" | "idtoken"
                        ))
                {
                    *val = Value::String("[redacted]".into());
                } else {
                    redact_value(val, secrets, error_fields || k == "error");
                }
            }
        }
        _ => {}
    }
}
