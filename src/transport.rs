mod codex_admission;
mod codex_processes;
mod codex_queue;
mod codex_steering;
mod interrupt_timeout;
mod suggestions;
use suggestions::private_suggestion;
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
struct AbortReply {
    request: uuid::Uuid,
    client: Reply<Value>,
}
impl AbortReply {
    fn finish(self, journal: &Journal, result: Result<Value>) {
        journal.control_result(self.request, &result, &[]);
        let _ = self.client.send(result);
    }
}

pub(crate) enum HostAnswer {
    Native(Value),
    Checked(crate::Cursor, Value),
    Permission(crate::Cursor, crate::PermissionDecision),
    Question(crate::Cursor, crate::QuestionReply, std::collections::BTreeMap<String,String>),
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
            Self::Question(cursor, reply, files) if journal.has_request(id, &cursor) => {
                crate::questions::answer_with_files(method, body, reply, files)
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
            | Self::Question(_, _, _)
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
/// Identity of one active root prompt. Pin it again at actor dispatch so a
/// later attempt cannot inherit a delayed steer or interrupt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TurnTarget {
    pub input_id: uuid::Uuid,
    pub request_id: Option<uuid::Uuid>,
}
impl TurnTarget {
    pub(crate) fn pin(self, journal: &Journal) -> Option<Self> {
        let (input, request) = journal.active_prompt()?;
        let request = uuid::Uuid::parse_str(&request).ok()?;
        if self.input_id.to_string() != input
            || self.request_id.is_some_and(|target| target != request)
        {
            return None;
        }
        Some(Self {
            request_id: Some(request),
            ..self
        })
    }
}
pub(crate) struct AbortRequest {
    pub target_request: Option<String>,
    pub reply: Reply<Value>,
}
impl AbortRequest {
    pub(crate) const STALE: &'static str = "prompt no longer owns the active turn";

    pub(crate) fn stale() -> Error {
        Error::Refused(Self::STALE.into())
    }

    pub(crate) fn for_current(self, journal: &Journal) -> Option<Reply<Value>> {
        if let Some(target) = self.target_request
            && !journal
                .active_prompt()
                .is_some_and(|(_, request)| request == target)
        {
            let _ = self.reply.send(Err(Self::stale()));
            return None;
        }
        Some(self.reply)
    }
}
pub(crate) enum Command {
    Compact(uuid::Uuid, Option<u64>, Reply<crate::ManualCompaction>),
    Rpc(
        String,
        Value,
        Option<crate::opening::OpenStep>,
        Reply<Value>,
    ),
    Send(Value, Reply<()>),
    Prompt(
        uuid::Uuid,
        crate::input::Input,
        Option<crate::chat::Admission>,
        Reply<()>,
        Reply<crate::TurnReport>,
    ),
    Queue(
        uuid::Uuid,
        crate::input::Input,
        Reply<()>,
        Reply<crate::TurnReport>,
    ),
    Withdraw(uuid::Uuid, Reply<()>),
    Abort(AbortRequest),
    StopTask(String, Option<crate::Cursor>, Reply<Value>),
    Steer(
        uuid::Uuid,
        crate::input::Input,
        Option<TurnTarget>,
        Reply<Value>,
    ),
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
    input: crate::input::Input,
    done: Reply<crate::TurnReport>,
}
struct Turn {
    input_receipt: Option<crate::InputReceipt>,
    prompt_request: uuid::Uuid,
    carried: Option<uuid::Uuid>,
    image_submission: Option<crate::ImageSubmission>,
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
        intent: RpcIntent,
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
        let mut text = match method {
            "steer" if matches!(intent, RpcIntent::Steer { .. }) => {
                params["message"].as_str()?.to_owned()
            }
            "turn/steer" if matches!(intent, RpcIntent::Steer { .. }) => {
                crate::observe::input_text(&params["input"]).unwrap_or_default()
            }
            wire::opencode::STEER | OPENCODE_V1_STEER | ANTIGRAVITY_STEER => params["prompt"]
                .as_array()?
                .iter()
                .find_map(|block| block["text"].as_str())
                .unwrap_or_default()
                .to_owned(),
            _ => return None,
        };
        redact_text(&mut text, secrets);
        Some(Self {
            id: match intent {
                RpcIntent::Steer { input_id, .. } => input_id,
                _ => uuid::Uuid::new_v4(),
            },
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
#[derive(Clone, Copy)]
enum RpcIntent {
    Control,
    Queue(uuid::Uuid),
    Abort(uuid::Uuid),
    Prompt,
    Steer {
        input_id: uuid::Uuid,
        request: uuid::Uuid,
    },
}
struct Pending {
    open: Option<crate::opening::OpenStep>,
    claude_interrupt: Option<uuid::Uuid>,
    method: String,
    reply: Option<Reply<Value>>,
    prompt_ack: Option<Reply<()>>,
    prompt_request: Option<uuid::Uuid>,
    codex_prompt_input: Option<uuid::Uuid>,
    control_request: Option<uuid::Uuid>,
    starts_turn: bool,
    steer: Option<SteeringReceipt>,
}

pub(crate) struct Launch {
    pub opencode_v2: bool,
    pub pi_mcp: Option<crate::pi_mcp::Config>,
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
    codex_queue: codex_queue::NativeQueue,
    suggestions: Option<crate::prompt_suggestions::Native>,
    codex_steering: codex_steering::Inputs,
    codex_processes: Option<codex_processes::Guard>,
    interrupt_timeout: interrupt_timeout::InterruptTimeout,
    factory: Option<crate::factory::Projection>,
    usage: wire::usage::Projection,
    opencode: Option<wire::opencode::Children>,
    cursor_children: Option<wire::cursor_children::Children>,
    protocol: Protocol,
    mode: ApprovalMode,
    child: crate::process::OwnedProcess,
    stdin: Option<ChildStdin>,
    close_ack: Option<(uuid::Uuid, Reply<()>)>,
    files: Launch,
    journal: Journal,
    identity: Arc<Mutex<Identity>>,
    next: u64,
    pending: HashMap<u64, Pending>,
    private_list_ids: HashSet<u64>,
    requests: HashMap<String, (String, Value)>,
    pi_deadlines: HashMap<String, (tokio::time::Instant, Value)>,
    // Modern Pi closes a prompt after retries and compaction have settled.
    pi_pending_end: Option<Vec<Event>>,
    // A native Pi run can start and settle while the host command is in preflight.
    pi_independent: Option<uuid::Uuid>,
    pi_leaf: Option<String>,
    turn: Option<Turn>,
    queue: VecDeque<Queued>,
    secrets: Vec<String>,
    arguments: wire::argument_stream::ArgumentStreams,
    qwen_content: wire::qwen_content::QwenContent,
    manual_compaction: Option<crate::compaction::Operation>,
    command_revision: u64,
    compactions: wire::compaction::Compactions,
    child_tasks: wire::tasks::CodexTasks,
    ancestry: HashMap<String, Vec<String>>,
    claude_child_models: HashMap<Vec<String>, String>,
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
    qwen_initialized: bool,
    qoder_daemon: bool,
    qoder_startup: Option<Vec<Value>>,
}
impl Actor {
    pub fn start(
        runtime: &crate::Runtime,
        options: &crate::SessionOptions,
        files: Launch,
        mut child: crate::process::OwnedProcess,
        journal: Journal,
        identity: Arc<Mutex<Identity>>,
        secrets: Vec<String>,
    ) -> mpsc::Sender<Command> {
        let stdout = child.inner().stdout.take().expect("stdout configured");
        let stdin = child.inner().stdin.take().expect("stdin configured");
        let (tx, rx) = mpsc::channel(64);
        let mut startup_stderr = tokio::task::JoinSet::new();
        if let Some(stderr) = child.inner().stderr.take() {
            let mut sensitive = secrets.clone();
            for arg in &options.launch_args {
                sensitive.push(arg.clone());
                if let Some((_, value)) = arg.split_once('=') {
                    sensitive.push(value.into());
                }
            }
            startup_stderr.spawn(crate::process::diagnostic::startup_tail(stderr, sensitive));
        }
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
            codex_queue: Default::default(),
            suggestions: options
                .prompt_suggestions
                .clone()
                .filter(|_| matches!(runtime.protocol, Protocol::Claude | Protocol::Codex))
                .map(crate::prompt_suggestions::Native::open),
            codex_steering: Default::default(),
            interrupt_timeout: Default::default(),
            codex_processes: (runtime.protocol == Protocol::Codex
                && options.settings.end_turn_processes_on_abort)
                .then(|| codex_processes::Guard::new(child.id(), journal.clone())),
            cursor_children: (runtime.protocol == Protocol::Acp && runtime.id == "cursor")
                .then(Default::default),
            opencode: (runtime.protocol == Protocol::Acp && runtime.id == "opencode")
                .then(|| wire::opencode::Children::new(files.opencode_v2)),
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
            pi_independent: None,
            pi_leaf: None,
            turn: None,
            queue: VecDeque::new(),
            secrets,
            ancestry: HashMap::new(),
            claude_child_models: HashMap::new(),
            arguments: Default::default(),
            qwen_content: Default::default(),
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
            qwen_initialized: false,
            qoder_daemon: false,
            qoder_startup: (runtime.protocol == Protocol::Qoder).then(Vec::new),
        };
        tokio::spawn(actor.run(stdout, rx, startup_stderr));
        tx
    }
    fn awaiting_pi_admission(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| {
            self.pending
                .values()
                .any(|p| p.prompt_request == Some(t.prompt_request))
        })
    }
    fn idle_for_interrupt(&self) -> bool {
        self.turn.is_none()
            && self.pi_independent.is_none()
            && !self.claude_notifications.active()
            && !(self.claude_cancel_queued && self.claude_steering.waiting())
            && self.observed_root_turn().is_none()
            && self.grok_owner().is_none()
            && self.manual_compaction.is_none()
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
        let mut stamp = origin.into();
        if stamp.direction == Direction::ToRuntime
            && kind == RecordKind::Request
            && events.contains(&Event::PromptDispatched)
            && let Some(turn) = &self.turn
            && let Some(id) = turn.chat_input
        {
            stamp.carried = turn.carried;
            stamp = stamp.with_prompt_request(
                id,
                self.protocol == Protocol::Pi,
                turn.prompt_request,
                turn.image_submission.clone(),
            );
        }
        let direction = stamp.direction;
        let session = self.source(&v).or_else(|| self.session());
        if direction == Direction::ToRuntime {
            crate::options::instructions::redact_open(self.protocol, &mut v);
        }
        if self.protocol == Protocol::Codex {
            crate::input::omit_codex_input_bytes(&mut v);
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
        if self.suggestions.is_some() && private_suggestion(&packet) {
            return Ok(());
        }
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
        intent: RpcIntent,
    ) -> Result<()> {
        let mut params = params;
        if self.protocol == Protocol::Claude && method == "initialize"
            && self.suggestions.as_mut().is_some_and(|s| s.initialize_claude())
        {
            params["promptSuggestions"] = true.into();
        }
        let starts_turn = matches!(intent, RpcIntent::Prompt);
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
            self.pi_independent
                .or_else(|| self.turn.as_ref().and_then(|t| t.chat_input))
                .or_else(|| {
                    self.codex_queue
                        .active(self.observed_root_turn())
                        .map(|(id, _)| id)
                })
                .or_else(|| self.grok_owner()),
            intent,
            &self.secrets,
        );
        if method == "turn/steer"
            && let RpcIntent::Steer { request, .. } = intent
            && let (Some(turn), Some(receipt)) = (params["expectedTurnId"].as_str(), &steer)
        {
            self.codex_steering.submit(turn, request, receipt.clone());
        }
        if method == INTERJECT
            && let Some(receipt) = &steer
        {
            self.grok_interjections
                .insert(receipt.id, (receipt.clone(), crate::SteerStatus::Submitted));
        }
        if matches!(intent, RpcIntent::Abort(_))
            && matches!(self.protocol, Protocol::Codex | Protocol::Claude)
        {
            self.interrupt_timeout.arm(id);
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
                control_request: match intent {
                    RpcIntent::Abort(request) | RpcIntent::Queue(request) => Some(request),
                    RpcIntent::Steer { request, .. } => Some(request),
                    RpcIntent::Prompt => self.turn.as_ref().map(|t| t.prompt_request),
                    _ => None,
                },
                reply,
                prompt_ack: None,
                codex_prompt_input: (starts_turn && self.protocol == Protocol::Codex)
                    .then(|| self.turn.as_ref()?.chat_input)
                    .flatten(),
                prompt_request: if starts_turn
                    && matches!(
                        self.protocol,
                        Protocol::Pi | Protocol::Factory | Protocol::Codex
                    ) {
                    self.turn.as_ref().map(|t| t.prompt_request)
                } else {
                    None
                },
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
            if method == codex_queue::ADD {
                RecordKind::Frame
            } else {
                RecordKind::Request
            },
            if matches!(
                (self.protocol, method),
                (Protocol::Pi, "steer") | (Protocol::Codex, "turn/steer")
            ) {
                self.pending
                    .get(&id)
                    .and_then(|p| p.steer.as_ref())
                    .map(|s| vec![s.event(crate::SteerStatus::Submitted)])
            } else {
                matches!(
                    method,
                    "thread/compact/start"
                        | "compact"
                        | "droid.compact_session"
                        | GROK_COMPACT
                        | OPENCODE_COMPACT
                )
                .then(|| vec![Event::ManualCompactionRequested])
            },
        )
        .await
    }
    fn finish(&mut self, result: Result<Outcome>) {
        if self.pi_independent.is_none() {
            self.pi_pending_end = None;
        }
        if let Some(turn) = self.turn.take() {
            if let Some(capture) = &self.files.capture {
                capture.reader.ended(&result);
            }
            let mut messages = turn.messages;
            if !turn.partial.is_empty() {
                messages.push(turn.partial);
            }
            let _ = turn.done.send(result.map(|outcome| crate::TurnReport {
                native_input_receipt: turn.input_receipt,
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
        input_id: uuid::Uuid,
        done: Reply<crate::TurnReport>,
    ) -> Result<()> {
        let id = self.session();
        if let Some(suggestions) = &mut self.suggestions {
            suggestions.start(Some(input_id));
        }
        self.configure_prompt_suggestions().await?;
        self.claude_errors.clear();
        let request = prompt.request_id;
        self.turn = Some(Turn {
            input_receipt: None,
            prompt_request: request,
            carried: prompt.carried,
            image_submission: crate::ImageSubmission::loaded(
                &prompt.text,
                &prompt.images,
                &self.secrets,
            ),
            process_baseline: None,
            chat_input: Some(input_id),
            claude_input: (self.protocol == Protocol::Claude)
                .then(|| wire::claude_input::Input::written(input_id)),
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
                self.turn.as_mut().expect("dispatch started a turn").native =
                    Some(input_id.to_string());
                self.rpc(
                    "droid.add_user_message",
                    prompt.factory(input_id),
                    None,
                    RpcIntent::Prompt,
                )
                .await?;
            }
            Protocol::Codex => {
                let mut args = json!({"threadId":id,"input":prompt.blocks(self.protocol),"clientUserMessageId":input_id});
                if let Some(schema) = &self.output_schema {
                    args["outputSchema"] = Value::Object(schema.clone());
                }
                self.rpc("turn/start", args, None, RpcIntent::Prompt)
                    .await?;
            }
            Protocol::Acp => {
                self.acp_failures.start();
                let mut args = json!({"sessionId":id,"prompt":prompt.blocks(self.protocol)});
                if let Some(grok) = &mut self.grok {
                    args["_meta"]["promptId"] = grok.name(Some(input_id)).into();
                }
                if let Some(schema) = &self.output_schema {
                    args["_meta"]["outputSchema"] = Value::Object(schema.clone());
                }
                self.rpc("session/prompt", args, None, RpcIntent::Prompt)
                    .await?;
            }
            Protocol::Qoder => {
                self.write(
                    crate::qoder::input(id.as_deref(), &prompt.text),
                    RecordKind::Request,
                )
                .await?;
            }
            Protocol::Claude | Protocol::Qwen => {
                let msg = prompt.stream_user(self.protocol, id.as_deref(), input_id);
                self.write(msg, RecordKind::Request).await?;
            }
            Protocol::Pi => {
                self.rpc("prompt", prompt.pi(), None, RpcIntent::Prompt)
                    .await?
            }
        }
        if !matches!(
            self.protocol,
            Protocol::Pi | Protocol::Factory | Protocol::Codex
        ) {
            self.journal.control_decision(request, None);
        }
        Ok(())
    }
    async fn codex_interrupt(&mut self, turn: &str, abort: AbortReply) -> Result<()> {
        self.rpc(
            "turn/interrupt",
            json!({"threadId":self.session(),"turnId":turn}),
            Some(abort.client),
            RpcIntent::Abort(abort.request),
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
            Command::Compact(_, revision, tx) => {
                if tx.is_closed() {
                    return Ok(true);
                }
                if self.turn.is_some()
                    || self.pi_independent.is_some()
                    || self.claude_notifications.active()
                    || self.claude_steering.waiting()
                    || self.observed_root_turn().is_some()
                    || self.grok_owner().is_some()
                    || !self.queue.is_empty()
                    || self.codex_queue.has_entries()
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
                    self.rpc(method, params, Some(raw), RpcIntent::Control)
                        .await?;
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
                self.close_ack = Some((self.journal.request_dispose(), tx));
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
                self.rpc(&method, params, Some(tx), RpcIntent::Control)
                    .await?;
                self.pending
                    .get_mut(&self.next)
                    .expect("RPC was queued")
                    .open = open;
            }
            Command::StopTask(task, expected, tx) => match self.protocol {
                Protocol::Claude if expected.as_ref().is_some_and(|occurrence| {
                    let snapshot = self.journal.snapshot();
                    !snapshot.tasks.iter().any(|row| {
                        row.id == task
                            && row.source.path.is_empty()
                            && row.source.session_id.as_deref() == Some(snapshot.id.as_str())
                            && row.task_type == crate::TaskType::Workflow
                            && row.status == crate::TaskStatus::Running
                            && row.started_at.as_ref() == Some(occurrence)
                    })
                }) => {
                    let _ = tx.send(Err(Error::Refused("workflow occurrence is no longer running".into())));
                }
                _ if tx.is_closed() => {},
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
                            RpcIntent::Control,
                        )
                        .await?
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                },
                Protocol::Claude | Protocol::Qoder => {
                    self.rpc(
                        "stop_task",
                        json!({"task_id":task}),
                        Some(tx),
                        RpcIntent::Control,
                    )
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
            Command::Prompt(id, prompt, admission, tx, done) => {
                if self.turn.is_some()
                    || self.pi_independent.is_some()
                    || self.claude_notifications.active()
                    || self.claude_steering.waiting()
                    || self.observed_root_turn().is_some()
                    || self.grok_owner().is_some()
                    || self.manual_compaction.is_some()
                {
                    let error = self.journal.busy_prompt(
                        id,
                        &prompt,
                        self.protocol == Protocol::Pi,
                        &self.secrets,
                    );
                    let _ = tx.send(Err(error));
                    return Ok(true);
                }
                if let Err(err) = crate::chat::Admission::commit(admission).await {
                    let _ = tx.send(Err(err));
                    return Ok(true);
                }
                let baseline = self
                    .codex_processes
                    .as_ref()
                    .and_then(|_| crate::resources::turn::baseline());
                self.dispatch(prompt, id, done).await?;
                if let Some(turn) = &mut self.turn {
                    turn.process_baseline = baseline;
                }
                if matches!(
                    self.protocol,
                    Protocol::Pi | Protocol::Factory | Protocol::Codex
                ) {
                    self.pending
                        .get_mut(&self.next)
                        .expect("native prompt admission is pending")
                        .prompt_ack = Some(tx);
                } else {
                    let _ = tx.send(Ok(()));
                }
            }
            Command::Queue(id, input, tx, done) => {
                self.journal.record_queue_request(
                    id,
                    &input,
                    &self.secrets,
                    self.protocol == Protocol::Codex,
                );
                if self.protocol == Protocol::Codex {
                    if let Err(error) = self.codex_queue.check(
                        id,
                        self.manual_compaction.is_some(),
                        self.output_schema.is_some(),
                    ) {
                        let code = match &error {
                            Error::Busy => "busy",
                            Error::Unsupported(_) => "unsupported",
                            Error::Refused(_) if self.codex_queue.interrupted() => "no_active_turn",
                            _ => "runtime_refused",
                        };
                        let reason = match &error {
                            Error::Unsupported(s) | Error::Refused(s) => s.as_str(),
                            _ => "a compaction is running",
                        };
                        self.journal
                            .control_decision(input.request_id, Some((code, reason)));
                        let _ = tx.send(Err(error));
                        return Ok(true);
                    }
                    let params = json!({"threadId":self.session(),"input":input.blocks(Protocol::Codex),"clientUserMessageId":id});
                    self.codex_queue.sent(id, input.request_id, tx, done);
                    self.rpc(
                        codex_queue::ADD,
                        params,
                        None,
                        RpcIntent::Queue(input.request_id),
                    )
                    .await?;
                    return Ok(true);
                }
                self.journal
                    .queue_accepted(input.request_id, id, true, None);
                self.queue.push_back(Queued { id, input, done });
                let _ = tx.send(Ok(()));
            }
            Command::Withdraw(input, tx) => {
                let request = self
                    .journal
                    .control_request(json!({"type":"withdraw","inputId":input}));
                if self.protocol == Protocol::Codex {
                    let reason = "Codex owns the native queue; this adapter does not expose experimental thread/queue/delete";
                    self.journal
                        .control_decision(request, Some(("unsupported", reason)));
                    let _ = tx.send(Err(Error::Unsupported(reason.into())));
                    return Ok(true);
                }
                let mut removed = false;
                while let Some(pos) = self.queue.iter().position(|e| e.id == input) {
                    let entry = self.queue.remove(pos).expect("queued entry was present");
                    let _ = entry.done.send(Ok(crate::TurnReport {
                        native_input_receipt: None,
                        outcome: Outcome::Aborted,
                        structured_output: None,
                        failure: None,
                        text: String::new(),
                    }));
                    removed = true;
                }
                if !removed {
                    self.journal
                        .control_decision(request, Some(("not_queued", "input is not queued")));
                    let _ = tx.send(Err(Error::Refused("input is not queued".into())));
                    return Ok(true);
                }
                self.record(
                    Direction::Host,
                    RecordKind::Response,
                    json!({"type":"withdrawn","inputId":input}),
                    vec![Event::InputRemoved {
                        input_id: input.to_string(),
                    }],
                    vec![],
                );
                self.journal.control_decision(request, None);
                let _ = tx.send(Ok(()));
            }
            Command::Abort(request) => {
                if request.target_request.is_some() && self.pi_independent.is_some() {
                    let _ = request.reply.send(Err(AbortRequest::stale()));
                } else if let Some(reply) = request.for_current(&self.journal) {
                    self.abort(reply).await?;
                }
            }
            Command::Steer(input, message, target, tx) => {
                self.steer(input, message, target, tx).await?
            }
        }
        Ok(true)
    }
    async fn abort(&mut self, client: Reply<Value>) -> Result<()> {
        let abort = AbortReply {
            request: self.journal.request_abort(),
            client,
        };
        if self.idle_for_interrupt() {
            abort.finish(&self.journal, Err(Error::Refused("no active turn".into())));
            return Ok(());
        }
        if self.protocol == Protocol::Pi
            && self.pi_independent.is_none()
            && self.awaiting_pi_admission()
        {
            self.turn.as_mut().expect("pending prompt").aborted = true;
            abort.finish(&self.journal, Ok(Value::Null));
            return Ok(());
        }
        match self.protocol {
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
                    abort.finish(
                        &self.journal,
                        Err(Error::Refused(
                            "runtime has not acknowledged this turn".into(),
                        )),
                    );
                    return Ok(());
                };
                let turn_id = turn_id.to_owned();
                let tx = if let Some(guard) = &mut self.codex_processes {
                    let baseline = self
                        .turn
                        .as_ref()
                        .filter(|t| t.native.as_ref() == Some(&turn_id))
                        .and_then(|t| t.process_baseline.clone());
                    guard.request(&turn_id, baseline, abort)
                } else {
                    Some(abort)
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
                self.rpc(
                    "interrupt",
                    params,
                    Some(abort.client),
                    RpcIntent::Abort(abort.request),
                )
                .await?;
                if self.protocol == Protocol::Qwen
                    && let Some(turn) = &mut self.turn
                {
                    // A root result can arrive before the interrupt's control response.
                    turn.aborted = true;
                }
            }
            Protocol::Factory => {
                self.rpc(
                    "droid.interrupt_session",
                    json!({}),
                    Some(abort.client),
                    RpcIntent::Abort(abort.request),
                )
                .await?
            }
            Protocol::Pi => {
                self.rpc(
                    "abort",
                    json!({}),
                    Some(abort.client),
                    RpcIntent::Abort(abort.request),
                )
                .await?
            }
            Protocol::Acp => {
                if let Err(error) = self.cancel_acp().await {
                    abort.finish(
                        &self.journal,
                        Err(Error::Closed("could not send ACP cancellation".into())),
                    );
                    return Err(error);
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
                abort.finish(&self.journal, Ok(Value::Null));
            }
        }
        Ok(())
    }
    async fn cancel_acp(&mut self) -> Result<()> {
        let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.session()}});
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
        Ok(())
    }
    async fn steer(
        &mut self,
        input_id: uuid::Uuid,
        message: crate::input::Input,
        target: Option<TurnTarget>,
        tx: Reply<Value>,
    ) -> Result<()> {
        let native_owner = target.is_none_or(|target| {
            self.pi_independent.is_none()
                && !self.claude_notifications.active()
                && (self
                    .turn
                    .as_ref()
                    .is_some_and(|t| Some(t.prompt_request) == target.request_id)
                    || self
                        .codex_queue
                        .active(self.observed_root_turn())
                        .is_some_and(|(input, request)| {
                            Some(request) == target.request_id && input == target.input_id
                        }))
        });
        let Some((request, tx)) =
            self.journal
                .admit_steer(input_id, &message, target, native_owner, &self.secrets, tx)
        else {
            return Ok(());
        };
        let intent = RpcIntent::Steer { input_id, request };
        let id = self.session();
        let result = if self.manual_compaction.is_some() {
            Err(Error::Busy)
        } else if self.idle_for_interrupt() {
            Err(Error::Refused("no active turn".into()))
        } else {
            match self.protocol {
                Protocol::Claude => {
                    if let Some(owner) = self
                        .claude_notifications
                        .id()
                        .or_else(|| self.turn.as_ref()?.claude_input.as_ref().map(|i| i.id))
                    {
                        let packet = message.stream_user(Protocol::Claude, id.as_deref(), input_id);
                        let mut text = message.text;
                        redact_text(&mut text, &self.secrets);
                        self.claude_steering.submit(input_id, owner, text);
                        self.write(packet, RecordKind::Request)
                            .await
                            .map(|()| Value::Null)
                    } else {
                        Err(Error::Refused("no active turn".into()))
                    }
                }
                Protocol::Acp => {
                    let mode = self.identity.lock().unwrap().acp_steering;
                    let params = json!({"sessionId":id,"prompt":message.blocks(Protocol::Acp)});
                    match mode {
                        Some(Steering::GrokInterject)
                            if self.grok.as_ref().is_some_and(GrokTurns::aborting)
                                || self.turn.as_ref().is_some_and(|t| t.aborted) =>
                        {
                            Err(Error::Refused("no active turn".into()))
                        }
                        Some(Steering::GrokInterject) => {
                            let mut packet = json!({"sessionId":id,"text":message.text,"interjectionId":input_id});
                            if !message.images.is_empty() {
                                packet["content"] = params["prompt"].clone();
                            }
                            return self.rpc(INTERJECT, packet, Some(tx), intent).await;
                        }
                        Some(Steering::OpenCodeExtension) => {
                            return self
                                .rpc(wire::opencode::STEER, params, Some(tx), intent)
                                .await;
                        }
                        Some(Steering::SupersedingPrompt)
                            if self.turn.as_ref().is_some_and(|t| t.aborted) =>
                        {
                            Err(Error::Refused("no active turn".into()))
                        }
                        Some(mode @ (Steering::ConcurrentPrompt | Steering::SupersedingPrompt)) => {
                            let method = if mode == Steering::SupersedingPrompt {
                                ANTIGRAVITY_STEER
                            } else {
                                OPENCODE_V1_STEER
                            };
                            self.rpc(method, params, None, intent)
                                .await
                                .map(|()| Value::Null)
                        }
                        None => Err(Error::Unsupported("steering on this transport".into())),
                    }
                }
                Protocol::Factory => {
                    return self
                        .rpc(
                            "droid.add_user_message",
                            message.factory(input_id),
                            Some(tx),
                            intent,
                        )
                        .await;
                }
                Protocol::Qoder if self.turn.is_some() => self
                    .write(
                        crate::qoder::input(id.as_deref(), &message.text),
                        RecordKind::Request,
                    )
                    .await
                    .map(|()| Value::Null),
                Protocol::Qoder => Err(Error::Refused("no active turn".into())),
                Protocol::Codex => {
                    if let Some(turn_id) = self
                        .turn
                        .as_ref()
                        .and_then(|t| t.native.as_deref())
                        .or_else(|| self.observed_root_turn())
                    {
                        if self.codex_steering.contains(turn_id, input_id) {
                            Err(Error::Refused(
                                "Codex already has this steering input for the target turn".into(),
                            ))
                        } else {
                            return self.rpc("turn/steer",json!({"threadId":id,"expectedTurnId":turn_id,"clientUserMessageId":input_id,"input":message.blocks(Protocol::Codex)}),Some(tx),intent).await;
                        }
                    } else {
                        Err(Error::Refused(
                            "runtime has not acknowledged this turn".into(),
                        ))
                    }
                }
                Protocol::Pi => return self.rpc("steer", message.pi(), Some(tx), intent).await,
                _ => Err(Error::Unsupported("steering on this transport".into())),
            }
        };
        self.journal.control_result(request, &result, &self.secrets);
        // A failed write may have reached the process in part. Stop this transport
        // after publishing the decision, as for a failed RPC write.
        let transport = match &result {
            Ok(_) | Err(Error::Busy | Error::Refused(_) | Error::Unsupported(_)) => Ok(()),
            Err(error) => Err(Error::Closed(error.to_string())),
        };
        let _ = tx.send(result);
        transport
    }
    async fn receive(&mut self, v: Value, context: Option<crate::ContextUsage>) -> Result<()> {
        if self.protocol == Protocol::Qwen
            && let Some(error) = crate::opening::qwen_missing_auth(&v)
            && let Some(id) = self
                .pending
                .iter()
                .find_map(|(id, p)| (p.method == "initialize").then_some(*id))
            && let Some(tx) = self.pending.remove(&id).and_then(|p| p.reply)
        {
            self.record(Direction::FromRuntime, RecordKind::Frame, v, vec![], vec![]);
            let _ = tx.send(Err(error.into()));
            return Ok(());
        }
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
        if self.suggestion_frame(&v) {
            return Ok(());
        }
        let mut received = RecordStamp::from(Direction::FromRuntime);
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
        if let Some(id) = number
            && let Some(pending) = self.pending.get(&id)
            && (pending.prompt_request.is_some() || pending.control_request.is_some())
        {
            let exact_id = match self.protocol {
                Protocol::Codex | Protocol::Acp => v["id"].as_u64() == Some(id),
                _ => wire_id.as_ref().and_then(Value::as_str) == Some(id.to_string().as_str()),
            };
            // Pi also echoes the command. Preserve unrelated public input frames
            // without consuming an attempt or exposing a private query's payload.
            if !exact_id || (self.protocol == Protocol::Pi && v["command"] != pending.method) {
                if self.protocol == Protocol::Pi
                    && matches!(v["command"].as_str(), Some("prompt" | "steer"))
                {
                    self.record(received, RecordKind::Frame, v, vec![], vec![]);
                }
                return Ok(());
            }
            if self.protocol == Protocol::Pi && !v["success"].is_boolean() {
                self.record(received, RecordKind::Frame, v, vec![], vec![]);
                return Err(Error::Protocol(
                    "Pi input reply omitted its success flag".into(),
                ));
            }
        }
        let response = number.and_then(|id| self.pending.remove(&id));
        if let Some(request) = response
            .as_ref()
            .and_then(|p| p.control_request.or(p.prompt_request))
        {
            received.response_to = Some(request.to_string());
        }
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
            .and_then(|c| {
                c.project(
                    root.as_deref(),
                    &v,
                    received.timestamp_ms,
                    &mut self.compactions,
                )
            })
            .unwrap_or_else(|| {
                let mut events = wire::project(self.protocol, &v, self.cursor_children.is_some());
                if self.protocol == Protocol::Qwen {
                    self.qwen_content.observe(&v, &mut events);
                }
                events.extend(self.compactions.observe(self.protocol, &v));
                events
            });
        if matches!(self.acp_failures, wire::failure::AcpFailures::Antigravity(_)) {
            wire::antigravity_children::project(root.as_deref(), &v, &mut events);
        }
        let mut independent = None;
        if self.protocol == Protocol::Pi {
            let kind = v["type"].as_str().unwrap_or_default();
            if kind == "agent_start" && self.pi_pending_end.is_none() {
                // Native preflight responds "started" before its own agent_start.
                // Older Pi also acknowledges prompt before sending model events.
                if self.turn.is_none() || self.awaiting_pi_admission() {
                    self.pi_independent = Some(uuid::Uuid::new_v4());
                }
            }
            if kind.starts_with("agent_")
                || kind.starts_with("turn_")
                || kind.starts_with("message_")
                || kind.starts_with("tool_execution_")
                || kind.starts_with("auto_")
            {
                independent = self.pi_independent;
            }
            if independent.is_none()
                && let Some(turn) = &mut self.turn
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
        let parent = v["parent_tool_use_id"].as_str().filter(|id| !id.is_empty());
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
                if self.protocol == Protocol::Claude
                    && !path.is_empty()
                    && let Event::Model { model } = event
                {
                    if model == "<synthetic>" || self.claude_child_models.get(&path) == Some(model)
                    {
                        return false;
                    }
                    self.claude_child_models.insert(path.clone(), model.clone());
                }
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
        let mut prompt_decision = None;
        let mut codex_decision = None;
        let mut codex_request = None;
        let mut invalid_codex_admission = false;
        let mut queue_reply = None;
        let native_method = response.as_ref().map(|p| p.method.clone());
        if let Some(p) = response {
            let mut result = if p.method == crate::cursor_models::LIST
                && v["error"]["code"] == -32601
            {
                Err(Error::Unsupported(
                    "Cursor does not implement its parameterized model catalog".into(),
                ))
            } else if self.protocol == Protocol::Pi && p.method == "steer" && v["success"] == false
            {
                Err(Error::Refused(
                    v["error"]
                        .as_str()
                        .unwrap_or("Pi refused the steer")
                        .to_owned(),
                ))
            } else {
                open_error.map_or_else(|| wire::result(self.protocol, &v), Err)
            };
            if result.is_err()
                && let Some(id) = number
            {
                self.interrupt_timeout.refused(id);
            }
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
                    (_, "turn/steer") => {
                        if let Some(request) = p.control_request {
                            if let Ok(native) = &result
                                && !self.codex_steering.confirms(request, native)
                            {
                                self.codex_steering.unconfirmed(request, &mut events);
                                result = Err(Error::Protocol(
                                    "Codex did not confirm the targeted steering turn".into(),
                                ));
                            } else {
                                self.codex_steering
                                    .reply(request, result.is_ok(), &mut events);
                                codex_decision = match &result {
                                    Ok(native) => {
                                        Some((request, json!({"type":"accepted","native":native})))
                                    }
                                    Err(_) => v.get("error").map(|native| {
                                        let mut decision = codex_admission::refusal(native);
                                        let reason = format!(
                                            "not_steerable: {}",
                                            decision["reason"].as_str().unwrap()
                                        );
                                        decision["reason"] = reason.into();
                                        (request, decision)
                                    }),
                                };
                            }
                        }
                    }
                    (Ok(data), "steer") if self.protocol == Protocol::Pi => {
                        let status = match data["disposition"].as_str() {
                            Some("handled") => crate::SteerStatus::Handled,
                            Some("queued") => crate::SteerStatus::Queued,
                            Some("started") => crate::SteerStatus::Accepted,
                            _ => crate::SteerStatus::Received,
                        };
                        events.push(steer.event(status));
                    }
                    (Err(_), "steer") if self.protocol == Protocol::Pi => {
                        events.push(steer.event(crate::SteerStatus::Refused))
                    }
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
            if let (Some(request), Some(input)) = (p.prompt_request, p.codex_prompt_input) {
                codex_request = Some(request);
                if let Some(native) = v.get("error") {
                    let mut decision = codex_admission::refusal(native);
                    redact(&mut decision, &self.secrets);
                    result = Err(Error::Refused(
                        decision["reason"].as_str().unwrap().to_owned(),
                    ));
                    events.push(Event::NativeInput {
                        input_id: input,
                        receipt: crate::InputReceipt::Refused,
                    });
                    codex_decision = Some((request, decision));
                } else if let Some(native) = v.get("result").filter(|value| {
                    value
                        .pointer("/turn/id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
                }) {
                    events.push(Event::NativeInput {
                        input_id: input,
                        receipt: crate::InputReceipt::Started,
                    });
                    codex_decision = Some((request, json!({"type":"accepted","native":native})));
                } else {
                    // No evidence identifies the native turn. An uncertain write is not a native refusal.
                    result = Err(Error::Closed(
                        "Codex turn/start returned no turn id; admission is unconfirmed".into(),
                    ));
                    invalid_codex_admission = true;
                }
            } else if let Some(request) = p.prompt_request {
                let refusal = result.as_ref().err().map(|error| {
                    let mut reason = v["error"]
                        .as_str()
                        .or_else(|| v.pointer("/error/message").and_then(Value::as_str))
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string());
                    redact_text(&mut reason, &self.secrets);
                    reason
                });
                if self.protocol == Protocol::Pi
                    && let Some(turn) = self.turn.as_ref().filter(|t| t.prompt_request == request)
                    && let Some(input_id) = turn.chat_input
                {
                    let receipt = match (
                        result.is_err(),
                        v.pointer("/data/disposition").and_then(Value::as_str),
                    ) {
                        (true, _) => Some(crate::InputReceipt::Refused),
                        (false, Some("handled")) => Some(crate::InputReceipt::Handled),
                        (false, Some("started")) => Some(crate::InputReceipt::Started),
                        (false, Some("queued")) => Some(crate::InputReceipt::Queued),
                        _ => None,
                    };
                    if let Some(receipt) = receipt {
                        if let Some(reason) = &refusal {
                            events.push(Event::TurnFailure {
                                failure: crate::TurnFailure::new(
                                    crate::FailureClass::Unknown,
                                    reason.clone(),
                                ),
                            });
                            // A queued input was already accepted by the host. Its later
                            // native rejection drops it from the queue exactly once.
                            if p.prompt_ack.is_none() {
                                events.push(Event::InputDropped {
                                    input_id: input_id.to_string(),
                                });
                            }
                            outcome_error = Some(Error::Refused(reason.clone()));
                        }
                        events.push(Event::NativeInput { input_id, receipt });
                    }
                }
                prompt_decision = Some((request, refusal));
            }
            if let Some(tx) = p.prompt_ack {
                prompt_ack = Some((
                    tx,
                    result.as_ref().map(|_| ()).map_err(|e| match e {
                        Error::Protocol(reason) => Error::Protocol(reason.clone()),
                        Error::Closed(reason) => Error::Closed(reason.clone()),
                        Error::Refused(reason) => Error::Refused(reason.clone()),
                        e => Error::Refused(e.to_string()),
                    }),
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
            if p.method == "turn/start"
                && self.protocol == Protocol::Codex
                && let Ok(body) = &result
                && let Some(native) = body.pointer("/turn/id").and_then(Value::as_str)
                && let Some(turn) = self
                    .turn
                    .as_mut()
                    .filter(|t| p.control_request == Some(t.prompt_request))
            {
                turn.native = Some(native.to_owned());
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
                    self.qwen_initialized = true;
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
            } else if p.starts_turn
                && !((self.protocol == Protocol::Pi || self.protocol == Protocol::Codex)
                    && p.prompt_request.is_some())
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
            if p.method == "interrupt" && result.is_ok() && self.protocol != Protocol::Qwen {
                self.claude_notifications
                    .acknowledge_interrupt(p.claude_interrupt);
                if let Some(t) = &mut self.turn
                    && (self.protocol != Protocol::Claude
                        || t.claude_input.as_ref().map(|i| i.id) == p.claude_interrupt)
                {
                    t.aborted = true;
                }
            }
            if p.method == codex_queue::ADD
                && let Some(request) = p.control_request
            {
                if v["error"]["code"] == -32601 {
                    result = Err(Error::Unsupported(
                        "Codex does not implement thread/queue/add".into(),
                    ));
                }
                queue_reply = Some((request, result));
            } else if let Some(tx) = p.reply {
                reply = Some((
                    tx,
                    result,
                    p.control_request.filter(|_| p.method != "turn/steer"),
                ));
            }
        }
        let reported_turn = v
            .pointer("/params/turn/id")
            .or_else(|| v.pointer("/params/turnId"))
            .and_then(Value::as_str);
        if is_root && self.protocol == Protocol::Codex {
            self.codex_steering.notice(&v, &mut events);
        }
        if is_root
            && self.protocol == Protocol::Codex
            && let Some((id, request)) = self.codex_queue.notice(&v, &mut events)
        {
            if let Some(suggestions) = &mut self.suggestions {
                suggestions.start(Some(id));
            }
            received = received.with_prompt_request(id, false, request, None);
            received.response_to = Some(request.to_string());
        }
        let queued_turn = is_root
            && self.protocol == Protocol::Codex
            && self.codex_queue.active(reported_turn).is_some();
        let mut current_turn = is_root
            && !queued_turn
            && codex_request.is_none_or(|request| {
                self.turn
                    .as_ref()
                    .is_some_and(|t| t.prompt_request == request)
            })
            && independent.is_none()
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
                    Event::NativeInput { input_id, receipt }
                        if Some(*input_id) == turn.chat_input =>
                    {
                        turn.input_receipt = Some(*receipt);
                    }
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
        let terminal = events
            .iter()
            .position(|e| matches!(e, Event::TurnEnded { .. }));
        let outcome = terminal.and_then(|i| events[i].resolved_input());
        let dropped = events.iter().find_map(|e| match e {
            Event::NativeInput { receipt, .. } => receipt.settled_outcome(),
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
                Protocol::Pi if current_turn && outcome == Some(Outcome::Completed) => {
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
            if let (Some(anchor), Some(i)) = (anchor, terminal) {
                events.insert(i, Event::ForkAnchor { anchor });
            }
        }
        if current_turn
            && self.turn.is_some()
            && let Some(i) = terminal
            && let Some(grok) = &mut self.grok
            && let Some(removed) = grok.host_ended()
        {
            events.insert(i, removed);
        }
        if let Some(id) = independent {
            events.insert(0, Event::NotificationActivity { id });
            if v["type"] == "message_start"
                && v["message"]["role"] == "user"
                && let Some(mut text) = crate::observe::input_text(&v["message"]["content"])
            {
                redact_text(&mut text, &self.secrets);
                events.push(Event::UserMessage { text });
            }
            if outcome.is_some() {
                self.pi_independent = None;
            }
        }
        // A correlated wire reply is native evidence. The host's admission
        // decision is a separate response with its own sequence position.
        let mut kind = wire::classify(self.protocol, &v);
        if kind == RecordKind::Response && (received.response_to.is_some() || terminal.is_some()) {
            kind = RecordKind::Frame;
            if let Some(method) = native_method
                && crate::Record::wire_frame_type(&v).is_none()
            {
                received = received.with_frame_type(method);
            }
        }
        let sample = if is_root && self.suggestions.is_some() {
            let active = if queued_turn {
                self.codex_queue.active(reported_turn).map(|(id, _)| id)
            } else {
                self.turn.as_ref().filter(|_| current_turn).and_then(|t| t.chat_input)
            };
            self.next_prompt_sample(
                &v,
                active,
                &events,
                outcome.as_ref().filter(|_| outcome_error.is_none()),
            )
        } else {
            None
        };
        let queue_input = reported_turn
            .filter(|_| queued_turn)
            .and_then(|turn| self.codex_queue.collect(turn, &events));
        self.record(received, kind, v.clone(), events, path);
        if let Some(id) = queue_input {
            self.codex_queue.finish(id);
        }
        if let Some((id, result)) = queue_reply {
            self.codex_queue
                .reply(&self.journal, id, result, &self.secrets);
        }
        if let Some((request, decision)) = codex_decision {
            codex_admission::publish(&self.journal, request, decision, &self.secrets);
        }
        if let Some((request, refusal)) = prompt_decision {
            self.journal.control_decision(
                request,
                refusal.as_deref().map(|reason| ("runtime_refused", reason)),
            );
        }
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
        if (current_turn && (outcome.is_some() || dropped.is_some()))
            || (is_root && self.protocol == Protocol::Claude && self.idle_for_interrupt())
        {
            self.interrupt_timeout.take();
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
        // Pi can enter an asynchronous extension command before it starts its
        // model. An interrupt accepted in that window belongs to this admission.
        if self.protocol == Protocol::Pi
            && v["type"] == "agent_start"
            && self
                .turn
                .as_mut()
                .is_some_and(|t| std::mem::take(&mut t.aborted))
        {
            self.rpc("abort", json!({}), None, RpcIntent::Control)
                .await?;
        }
        if let Some(packet) = sample {
            self.write_packet(packet, RecordKind::Request, Some(Vec::new()))
                .await?;
        }
        if let Some((tx, result)) = prompt_ack {
            let _ = tx.send(result);
        }
        if invalid_codex_admission {
            return Err(Error::Protocol(
                "Codex turn/start returned no turn id".into(),
            ));
        }
        if let Some((tx, result, request)) = reply {
            if let Some(request) = request {
                self.journal.control_result(request, &result, &self.secrets);
            }
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
    // Read context before publishing the terminal frame. Qwen exposes this RPC
    // in its session journal; Pi and Factory attach their reading to the terminal event.
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
            && self.turn.as_ref().is_some_and(|turn| !turn.aborted)
            && packet["type"] == "result"
            && packet["parent_tool_use_id"].is_null();
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
        if qwen {
            self.record(
                Direction::ToRuntime,
                RecordKind::Request,
                query.clone(),
                vec![],
                vec![],
            );
            match self.readback_frame(&query, &id, lines, buffer).await {
                Some(reply) => {
                    let mut stamp = RecordStamp::from(Direction::FromRuntime);
                    stamp.response_to = Some(id);
                    let usage = wire::result(self.protocol, &reply)
                        .ok()
                        .and_then(|body| crate::context::qwen(&body));
                    self.record(
                        stamp,
                        RecordKind::Response,
                        reply,
                        usage
                            .map(|usage| Event::ContextUsage { usage })
                            .into_iter()
                            .collect(),
                        vec![],
                    );
                }
                None => {
                    let mut stamp = RecordStamp::from(Direction::Host);
                    stamp.response_to = Some(id);
                    self.record(stamp, RecordKind::Response, json!({"type":"context_readback_failed","reason":"no native context response within the readback window"}), vec![], vec![]);
                }
            }
            return None;
        }
        let body = self.private_readback(&query, &id, lines, buffer).await?;
        if factory {
            return crate::factory::context(&body);
        }
        if self.pi_independent.is_none()
            && packet["type"] == "agent_settled"
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
        let mut reply = self.readback_frame(query, id, lines, buffer).await?;
        if self.protocol == Protocol::Pi {
            return (reply["command"] == query["type"] && reply["success"] == true)
                .then(|| reply["data"].take());
        }
        wire::result(self.protocol, &reply).ok()
    }

    async fn readback_frame(
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
                if let Ok(reply) = serde_json::from_str::<Value>(&line)
                    && wire::response_id(self.protocol, &reply)
                        .as_ref()
                        .and_then(Value::as_str)
                        == Some(id)
                {
                    return Some(reply);
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
    async fn stop_child(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if self.files.pi_mcp.is_some()
            || (matches!(
                self.protocol,
                Protocol::Qwen | Protocol::Qoder | Protocol::Factory
            ) && self.turn.is_none())
        {
            self.stdin.take();
            if let Ok(Ok(status)) =
                tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await
            {
                return Ok(status);
            }
        }
        self.child.stop_with_status().await
    }
    async fn run(
        mut self,
        stdout: ChildStdout,
        mut commands: mpsc::Receiver<Command>,
        mut startup_stderr: tokio::task::JoinSet<Option<String>>,
    ) {
        let mut lines = BufReader::new(stdout).lines();
        let mut buffered = BufferedLines::new();
        let reason = loop {
            if self.turn.is_none()
                && self.pi_independent.is_none()
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
                    .dispatch(input.input.dispatch_queued(input.id), input.id, input.done)
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
            let interrupt = self.interrupt_timeout.deadline();
            tokio::select! {
                () = crate::prompt_suggestions::Native::changed(&mut self.suggestions) => {
                    if let Err(e) = self.configure_prompt_suggestions().await { break e.to_string() }
                }
                () = async { if let Some(at)=interrupt { tokio::time::sleep_until(at).await; } else { std::future::pending::<()>().await; } } => {
                    for id in self.interrupt_timeout.take() {
                        if let Some(mut pending)=self.pending.remove(&id)
                            && let (Some(request),Some(client))=(pending.control_request,pending.reply.take()) {
                            AbortReply{request,client}.finish(&self.journal,Ok(Value::Null));
                        }
                    }
                    self.record(Direction::Host, RecordKind::Frame,
                        json!({"type":"interrupt_timeout"}),vec![Event::TurnEnded{outcome:Outcome::Aborted}],vec![]);
                    self.finish(Ok(Outcome::Aborted));
                    break "native turn did not end after interrupt".into();
                }
                () = async { if let Some(at)=deadline { tokio::time::sleep_until(at).await; } else { std::future::pending::<()>().await; } } => self.expire_pi_dialogs(),
                prepared = async { self.codex_processes.as_mut().expect("enabled guard").prepared().await }, if preparing_abort => {
                    if let Some((turn, waiting)) = prepared {
                        let mut result = Ok(());
                        for tx in waiting {
                            if result.is_ok() { result = self.codex_interrupt(&turn, tx).await; }
                            else { tx.finish(&self.journal, Err(Error::Closed("could not send interrupt".into()))); }
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
            .filter(|p| {
                matches!(
                    p.method.as_str(),
                    ANTIGRAVITY_STEER | OPENCODE_V1_STEER | "steer" | "turn/steer"
                )
            })
            .filter(|p| p.method != "turn/steer" || !self.codex_steering.echoed(p.control_request))
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
        let status = self.stop_child().await;
        self.files.capture.take();
        self.files.pi.take();
        self.files.pi_mcp.take();
        self.files.qwen_defaults.take();
        self.files.dsh_patch.take();
        self.files.prompts.clear();
        let reason = if self.protocol == Protocol::Qwen
            && !self.qwen_initialized
            && self.close_ack.is_none()
        {
            let diagnostic =
                tokio::time::timeout(Duration::from_millis(200), startup_stderr.join_next())
                    .await
                    .ok()
                    .flatten()
                    .and_then(|task| task.ok())
                    .flatten();
            let detail = diagnostic.map_or_else(String::new, |text| format!(": {text}"));
            format!("Qwen exited before confirming the native session ({reason}){detail}")
        } else {
            reason
        };
        let reason = self
            .secrets
            .iter()
            .fold(reason, |msg, key| msg.replace(key, "[redacted]"));
        let closed = Error::Closed(reason.clone());
        for p in self.pending.values() {
            if matches!(p.method.as_str(), codex_queue::ADD | "turn/steer")
                || p.codex_prompt_input.is_some()
            {
                continue;
            }
            if let Some(request) = p
                .prompt_request
                .or(p.control_request.filter(|_| p.reply.is_some()))
            {
                self.journal.prompt_error(request, &closed, &self.secrets);
            }
        }
        let mut exit = crate::process::exit_body(&status, &reason);
        if let Some((id, _)) = &self.close_ack {
            exit["requestId"] = id.to_string().into();
        }
        self.record(
            Direction::Host,
            RecordKind::Response,
            exit,
            vec![Event::Exited {
                reason: reason.clone(),
            }],
            vec![],
        );
        self.finish(Err(Error::Closed(reason.clone())));
        self.codex_queue.disconnected(&reason);
        for p in self.pending.into_values() {
            if let Some(tx) = p.reply {
                let _ = tx.send(Err(Error::Closed(reason.clone())));
            }
            if let Some(tx) = p.prompt_ack {
                let _ = tx.send(Err(Error::Closed(reason.clone())));
            }
        }
        for input in self.queue {
            let _ = input.done.send(Err(Error::Closed(reason.clone())));
        }
        self.journal.close();
        crate::session::reachability::reject_pending(
            &mut commands,
            &self.journal,
            self.protocol,
            &self.secrets,
        )
        .await;
        if let Some((_, tx)) = self.close_ack {
            let _ = tx.send(status.map(|_| ()).map_err(Error::from));
        }
    }
}

pub(crate) fn redact(v: &mut Value, secrets: &[String]) {
    redact_value(v, secrets, false)
}

pub(crate) fn redact_native_error(v: &mut Value, secrets: &[String]) {
    redact_value(v, secrets, true)
}

pub(crate) fn redact_text(text: &mut String, secrets: &[String]) {
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
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
