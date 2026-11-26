//! Deterministic, zero-token scripted runtime for SDK and host testing (`kar::testing`).
//!
//! [`ScriptedRuntime`] and [`ScriptedSession`] produce genuine [`crate::Session`]
//! handles backed by the real [`crate::stream::Journal`] and [`crate::observe::SessionReducer`]
//! pipelines without spawning external agent binaries or making network calls.

use crate::{
    ApprovalMode, ContextUsage, Direction, Error, Event, Execution, InputImage,
    ManualCompactionCapability, Outcome, OutputPart, PlacementRequest, Protocol, RecordKind,
    Result, Session, SessionCapabilityDeclaration, SessionOptions, SteerStatus, TaskChange,
    TaskDetails, TaskStatus, TaskType, TokenReading, TokenTotals, ToolResult, ToolStatus,
    TurnFailure, TurnReport, UsageSnapshot,
    input::Input,
    stream::{Journal, RecordStamp},
    transport::{Command, redact},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    ops::Deref,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

type BoxTurnFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
type CustomTurnHandler = dyn Fn(ScriptedTurnContext) -> BoxTurnFuture + Send + Sync + 'static;

/// Declarative or programmatic behavior executed for a single turn in [`ScriptedRuntime`].
#[derive(Clone)]
pub enum ScriptedStep {
    /// Emit assistant text and complete the turn.
    ReplyText(String),
    /// Stream multiple incremental text or reasoning delta frames before completing the turn.
    StreamDeltas {
        chunks: Vec<String>,
        phase: Option<String>,
        delay: Option<Duration>,
    },
    /// Emit a tool call start, structured input, tool result, and optional assistant reply.
    ToolCall {
        name: String,
        input: Value,
        output: String,
        status: ToolStatus,
        reply_text: Option<String>,
    },
    /// Emit a host user-input question request (`ApprovalRequested`), wait for
    /// [`Session::answer_question`] or [`Session::respond`], and reply with a summary.
    AskQuestion {
        id: String,
        question: String,
        options: Vec<String>,
    },
    /// Fail the turn with a classified [`TurnFailure`].
    Fail(TurnFailure),
    /// Hold the turn active until [`Session::steer`] (or [`Session::deliver`]) supplies
    /// a mid-turn steer message, then complete with the steered content.
    WaitForSteer { reply_prefix: Option<String> },
    /// Execute an arbitrary async turn script against [`ScriptedTurnContext`].
    Custom(Arc<CustomTurnHandler>),
}

impl std::fmt::Debug for ScriptedStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReplyText(text) => f.debug_tuple("ReplyText").field(text).finish(),
            Self::StreamDeltas {
                chunks,
                phase,
                delay,
            } => f
                .debug_struct("StreamDeltas")
                .field("chunks", chunks)
                .field("phase", phase)
                .field("delay", delay)
                .finish(),
            Self::ToolCall {
                name,
                input,
                output,
                status,
                reply_text,
            } => f
                .debug_struct("ToolCall")
                .field("name", name)
                .field("input", input)
                .field("output", output)
                .field("status", status)
                .field("reply_text", reply_text)
                .finish(),
            Self::AskQuestion {
                id,
                question,
                options,
            } => f
                .debug_struct("AskQuestion")
                .field("id", id)
                .field("question", question)
                .field("options", options)
                .finish(),
            Self::Fail(failure) => f.debug_tuple("Fail").field(failure).finish(),
            Self::WaitForSteer { reply_prefix } => f
                .debug_struct("WaitForSteer")
                .field("reply_prefix", reply_prefix)
                .finish(),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

impl ScriptedStep {
    pub fn reply(text: impl Into<String>) -> Self {
        Self::ReplyText(text.into())
    }

    pub fn deltas<I, S>(chunks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::StreamDeltas {
            chunks: chunks.into_iter().map(Into::into).collect(),
            phase: None,
            delay: None,
        }
    }

    pub fn deltas_with_phase<I, S>(
        chunks: I,
        phase: Option<String>,
        delay: Option<Duration>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::StreamDeltas {
            chunks: chunks.into_iter().map(Into::into).collect(),
            phase,
            delay,
        }
    }

    pub fn tool(name: impl Into<String>, input: Value, output: impl Into<String>) -> Self {
        Self::ToolCall {
            name: name.into(),
            input,
            output: output.into(),
            status: ToolStatus::Succeeded,
            reply_text: None,
        }
    }

    pub fn question<I, S>(id: impl Into<String>, question: impl Into<String>, options: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::AskQuestion {
            id: id.into(),
            question: question.into(),
            options: options.into_iter().map(Into::into).collect(),
        }
    }

    pub fn fail(class: crate::FailureClass, reason: impl Into<String>) -> Self {
        Self::Fail(TurnFailure {
            class,
            reason: reason.into(),
            status: None,
            credential: None,
            resets_at_ms: None,
            rate_limit_type: None,
        })
    }

    pub fn wait_for_steer() -> Self {
        Self::WaitForSteer { reply_prefix: None }
    }

    pub fn custom<F, Fut>(handler: F) -> Self
    where
        F: Fn(ScriptedTurnContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        Self::Custom(Arc::new(move |ctx| Box::pin(handler(ctx))))
    }
}

/// Construct a deterministic [`ScriptedRuntime`] executing `steps` in FIFO order across turns.
pub fn scripted_runtime(
    id: impl Into<String>,
    steps: impl IntoIterator<Item = ScriptedStep>,
) -> ScriptedRuntime {
    ScriptedRuntime::new(id, steps)
}

/// Deterministic in-process runtime builder for zero-token SDK tests and simulations.
#[derive(Clone)]
pub struct ScriptedRuntime {
    id: String,
    model: String,
    context_window: u64,
    placement: PlacementRequest,
    steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
    fallback: Option<ScriptedStep>,
    context_usage_enabled: bool,
}

impl ScriptedRuntime {
    pub fn new(id: impl Into<String>, steps: impl IntoIterator<Item = ScriptedStep>) -> Self {
        Self {
            id: id.into(),
            model: "scripted-1".into(),
            context_window: 200_000,
            placement: PlacementRequest::Auto,
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            fallback: None,
            context_usage_enabled: true,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_context_window(mut self, context_window: u64) -> Self {
        self.context_window = context_window;
        self
    }

    pub fn with_placement(mut self, placement: PlacementRequest) -> Self {
        self.placement = placement;
        self
    }

    /// Configure whether turns emit `Event::ContextUsage` (`true` by default).
    pub fn with_context_usage(mut self, enabled: bool) -> Self {
        self.context_usage_enabled = enabled;
        self
    }

    /// Fallback step repeated once the initial `steps` queue is exhausted.
    pub fn with_fallback_step(mut self, step: ScriptedStep) -> Self {
        self.fallback = Some(step);
        self
    }

    /// Append an additional [`ScriptedStep`] to the runtime's shared step queue.
    pub fn push_step(&self, step: ScriptedStep) {
        self.steps.lock().unwrap().push_back(step);
    }

    /// Static capability declaration matching the scripted runtime's session behavior.
    pub fn capability_declaration(&self) -> SessionCapabilityDeclaration {
        SessionCapabilityDeclaration {
            steering: Some(true),
            image_input: Some(true),
            stop_task: Some(false),
            manual_compaction: Some(ManualCompactionCapability::Unsupported {
                reason: "scripted runtime does not perform context compaction",
            }),
            context_usage: Some(crate::ContextUsageCapability {
                tokens: self.context_usage_enabled,
                context_window: self.context_usage_enabled,
            }),
            ..SessionCapabilityDeclaration::default()
        }
    }

    /// Open a new [`ScriptedSession`] with zero subprocesses or network requests.
    pub async fn open(&self, options: SessionOptions) -> Result<ScriptedSession> {
        if let Some(effort) = &options.settings.effort {
            return Err(Error::Unsupported(format!(
                "{} lists no reasoning-effort levels; cannot run at effort {effort:?}",
                self.id
            )));
        }
        if options.settings.model_requests.is_some() {
            return Err(Error::Unsupported(
                "the scripted runtime sends no provider request".into(),
            ));
        }

        let journal = Journal::for_open(
            options.retained_bytes,
            self.placement,
            Some(Execution::InProcess),
        )?;
        let session_id = options
            .resume
            .clone()
            .unwrap_or_else(|| format!("scripted-{}", Uuid::new_v4()));
        journal.bind_root(&session_id);

        journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/model"),
            RecordKind::Frame,
            Some(session_id.clone()),
            vec![],
            json!({ "type": "scripted/model", "model": self.model }),
            vec![Event::Model {
                model: self.model.clone(),
            }],
        );

        let (commands_tx, commands_rx) = mpsc::channel(64);
        let actor = ScriptedActor::new(
            session_id.clone(),
            self.model.clone(),
            self.context_window,
            self.context_usage_enabled,
            options.clone(),
            journal.clone(),
            Arc::clone(&self.steps),
            self.fallback.clone(),
        );
        tokio::spawn(actor.run(commands_rx));

        let info = json!({
            "runtime": self.id,
            "sessionId": session_id,
            "model": self.model,
        });
        let mut session = Session::remote(
            Protocol::Pi,
            options,
            commands_tx,
            journal,
            session_id,
            info,
        );
        session.image_input = true;
        Ok(ScriptedSession { session })
    }
}

/// Opened session produced by [`ScriptedRuntime::open`].
///
/// Dereferences directly to [`crate::Session`] and can be unwrapped via [`Self::into_session`].
#[derive(Clone)]
pub struct ScriptedSession {
    session: Session,
}

impl ScriptedSession {
    pub fn as_session(&self) -> &Session {
        &self.session
    }

    pub fn into_session(self) -> Session {
        self.session
    }

    /// Snapshot the session's current token ledger and cursor as a [`UsageSnapshot`].
    pub fn usage_snapshot(&self) -> UsageSnapshot {
        let snap = self.session.snapshot();
        let cursor = snap.cursor.unwrap_or(crate::Cursor {
            stream_id: self.session.stream_id(),
            after_seq: 0,
        });
        UsageSnapshot {
            cursor,
            usage: snap.usage,
            children: snap.child_usage,
        }
    }
}

impl Deref for ScriptedSession {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() {
        0
    } else {
        ((text.len() as i64) + 3) / 4
    }
}

struct PendingQuestion {
    method: String,
    body: Value,
    answer_tx: oneshot::Sender<Value>,
}

struct TurnShared {
    session_id: String,
    journal: Journal,
    approvals: ApprovalMode,
    input_text: String,
    input_id: Uuid,
    images: Vec<InputImage>,
    options: SessionOptions,
    cancel_rx: watch::Receiver<bool>,
    steered_log: Mutex<Vec<String>>,
    steer_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
    question_reg_tx: mpsc::UnboundedSender<(String, PendingQuestion)>,
    exit_tx: mpsc::UnboundedSender<i32>,
    output_messages: Mutex<Vec<String>>,
    partial_text: Mutex<String>,
    structured_output: Mutex<Option<Value>>,
    failure: Mutex<Option<TurnFailure>>,
    output_tokens: Mutex<i64>,
    reasoning_tokens: Mutex<i64>,
    next_seq: Mutex<u64>,
}

/// Interactive handle passed to [`ScriptedStep::Custom`] closures during a scripted turn.
#[derive(Clone)]
pub struct ScriptedTurnContext {
    inner: Arc<TurnShared>,
}

impl ScriptedTurnContext {
    pub fn input(&self) -> &str {
        &self.inner.input_text
    }

    pub fn input_id(&self) -> Uuid {
        self.inner.input_id
    }

    pub fn images(&self) -> &[InputImage] {
        &self.inner.images
    }

    pub fn options(&self) -> &SessionOptions {
        &self.inner.options
    }

    pub fn is_cancelled(&self) -> bool {
        *self.inner.cancel_rx.borrow()
    }

    pub async fn cancelled(&self) {
        let mut rx = self.inner.cancel_rx.clone();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }

    /// Steer inputs already delivered into this turn.
    pub fn steered(&self) -> Vec<String> {
        self.inner.steered_log.lock().unwrap().clone()
    }

    /// Wait for the next mid-turn steer input (`None` if the turn is aborted or closed).
    pub async fn wait_for_steer(&self) -> Option<String> {
        let mut cancel = self.inner.cancel_rx.clone();
        if *cancel.borrow() {
            return None;
        }
        let mut guard = self.inner.steer_rx.lock().await;
        tokio::select! {
            biased;
            _ = cancel.changed() => None,
            msg = guard.recv() => msg,
        }
    }

    fn alloc_id(&self, prefix: &str) -> String {
        let mut seq = self.inner.next_seq.lock().unwrap();
        *seq += 1;
        format!("{prefix}-{}", *seq)
    }

    /// Emit a single assistant text delta + completed message frame.
    pub fn say(&self, text: impl Into<String>) {
        if self.is_cancelled() {
            return;
        }
        let text = text.into();
        let msg_id = self.alloc_id("msg");
        *self.inner.output_tokens.lock().unwrap() += estimate_tokens(&text);
        self.inner
            .output_messages
            .lock()
            .unwrap()
            .push(text.clone());
        self.inner.partial_text.lock().unwrap().clear();
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/text"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({
                "type": "scripted/text",
                "message_id": msg_id,
                "phase": "final_answer",
                "text": text,
            }),
            vec![
                Event::TextDelta { text: text.clone() },
                Event::Message { text },
            ],
        );
    }

    /// Emit a single streaming delta chunk on a given `message_id` and optional `phase`.
    pub fn say_delta(&self, message_id: &str, text: impl Into<String>, phase: Option<&str>) {
        if self.is_cancelled() {
            return;
        }
        let text = text.into();
        *self.inner.output_tokens.lock().unwrap() += estimate_tokens(&text);
        self.inner.partial_text.lock().unwrap().push_str(&text);
        let mut native = json!({
            "type": "scripted/text_delta",
            "message_id": message_id,
            "text": text,
        });
        if let Some(ph) = phase {
            native["phase"] = ph.into();
        }
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/text_delta"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            native,
            vec![Event::TextDelta { text }],
        );
    }

    /// Flush accumulated `say_delta` text into a completed `Event::Message`.
    pub fn finish_message(&self, message_id: &str, phase: Option<&str>) {
        if self.is_cancelled() {
            return;
        }
        let full = std::mem::take(&mut *self.inner.partial_text.lock().unwrap());
        if full.is_empty() {
            return;
        }
        self.inner
            .output_messages
            .lock()
            .unwrap()
            .push(full.clone());
        let mut native = json!({
            "type": "scripted/message",
            "message_id": message_id,
            "text": full,
        });
        if let Some(ph) = phase {
            native["phase"] = ph.into();
        }
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/message"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            native,
            vec![Event::Message { text: full }],
        );
    }

    /// Emit a reasoning/thinking delta frame.
    pub fn think(&self, text: impl Into<String>) {
        if self.is_cancelled() {
            return;
        }
        let text = text.into();
        let msg_id = self.alloc_id("reason");
        *self.inner.reasoning_tokens.lock().unwrap() += estimate_tokens(&text);
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/reasoning"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({
                "type": "scripted/reasoning",
                "message_id": msg_id,
                "phase": "thinking",
                "text": text,
            }),
            vec![Event::ReasoningDelta { text }],
        );
    }

    /// Emit a structured tool call lifecycle (`ToolStarted`, `ToolInput`, `ToolResult`, `ToolEnded`).
    pub fn tool_call(
        &self,
        name: impl Into<String>,
        input: Value,
        output: impl Into<String>,
        status: ToolStatus,
    ) {
        if self.is_cancelled() {
            return;
        }
        let name = name.into();
        let output = output.into();
        let call_id = self.alloc_id("call");
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/tool_start"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({
                "type": "scripted/tool_start",
                "callId": call_id,
                "name": name,
                "input": input,
            }),
            vec![
                Event::ToolStarted {
                    id: call_id.clone(),
                    name,
                },
                Event::ToolInput {
                    id: call_id.clone(),
                    input,
                },
            ],
        );

        let content = if output.is_empty() {
            None
        } else {
            Some(vec![OutputPart::Text {
                text: output.clone(),
            }])
        };
        let result = ToolResult {
            status: Some(status.clone()),
            exit_code: Some(if status == ToolStatus::Succeeded {
                0
            } else {
                1
            }),
            content,
            file_changes: Vec::new(),
        };
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/tool_end"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({
                "type": "scripted/tool_end",
                "callId": call_id,
                "output": output,
                "status": status,
            }),
            vec![
                Event::ToolResult {
                    id: call_id.clone(),
                    result,
                },
                Event::ToolEnded { id: call_id },
            ],
        );
    }

    /// Emit a structured output payload for the turn.
    pub fn set_structured_output(&self, value: Value) {
        if self.is_cancelled() {
            return;
        }
        *self.inner.structured_output.lock().unwrap() = Some(value.clone());
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/structured_output"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({ "type": "scripted/structured_output", "value": value }),
            vec![Event::StructuredOutput { value }],
        );
    }

    /// Emit a background/subagent task start event.
    pub fn start_task(
        &self,
        id: impl Into<String>,
        description: impl Into<String>,
        task_type: TaskType,
    ) {
        if self.is_cancelled() {
            return;
        }
        let id = id.into();
        let desc = description.into();
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/task_start"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({ "type": "scripted/task_start", "id": id, "description": desc }),
            vec![Event::Task {
                id,
                change: TaskChange::Started {
                    task_type,
                    details: TaskDetails {
                        description: Some(desc),
                        background: Some(true),
                        ..TaskDetails::default()
                    },
                },
            }],
        );
    }

    /// Emit a background/subagent task terminal event.
    pub fn end_task(&self, id: impl Into<String>, status: TaskStatus, summary: Option<String>) {
        if self.is_cancelled() {
            return;
        }
        let id = id.into();
        self.inner.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/task_end"),
            RecordKind::Frame,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({ "type": "scripted/task_end", "id": id, "status": status }),
            vec![Event::Task {
                id,
                change: TaskChange::Ended {
                    status,
                    summary,
                    output: None,
                    output_file: None,
                },
            }],
        );
    }

    /// Ask the host a structured user-input question (`item/tool/requestUserInput`) and await reply.
    pub async fn ask_question(
        &self,
        field_id: impl Into<String>,
        question: impl Into<String>,
        options: Vec<String>,
    ) -> Result<Value> {
        if self.is_cancelled() {
            return Err(Error::Closed("turn cancelled".into()));
        }
        let field_id = field_id.into();
        let question = question.into();
        let req_id = self.alloc_id("ask");
        let opts_json: Value = if options.is_empty() {
            Value::Null
        } else {
            Value::Array(options.iter().map(|opt| json!({ "label": opt })).collect())
        };
        let params = json!({
            "questions": [{
                "id": field_id,
                "question": question,
                "isOther": true,
                "options": opts_json,
            }]
        });
        let method = "item/tool/requestUserInput";
        let (answer_tx, answer_rx) = oneshot::channel();
        let _ = self.inner.question_reg_tx.send((
            req_id.clone(),
            PendingQuestion {
                method: method.into(),
                body: params.clone(),
                answer_tx,
            },
        ));
        self.inner.journal.push(
            Direction::FromRuntime,
            RecordKind::Request,
            Some(self.inner.session_id.clone()),
            vec![],
            json!({
                "method": method,
                "id": req_id,
                "params": params,
            }),
            vec![Event::ApprovalRequested {
                id: Value::String(req_id.clone()),
                method: method.into(),
            }],
        );
        if self.inner.approvals == ApprovalMode::Deny {
            self.inner
                .journal
                .host_event(Event::RequestResolved { id: req_id.into() });
            return Ok(json!({ "cancelled": true }));
        }
        let mut cancel = self.inner.cancel_rx.clone();
        tokio::select! {
            biased;
            _ = cancel.changed() => Err(Error::Closed("turn cancelled".into())),
            res = answer_rx => res.map_err(|_| Error::Closed("question cancelled".into())),
        }
    }

    /// Mark this turn failed with a classified [`TurnFailure`].
    pub fn fail(&self, failure: TurnFailure) {
        if self.is_cancelled() {
            return;
        }
        *self.inner.failure.lock().unwrap() = Some(failure);
    }

    /// Simulate an unrequested process crash (`exited` without `TurnEnded`).
    pub fn exit_unexpectedly(&self, code: i32) {
        let _ = self.inner.exit_tx.send(code);
    }
}

struct ActiveTurn {
    input_id: Uuid,
    request_id: Uuid,
    shared: Arc<TurnShared>,
    cancel_tx: watch::Sender<bool>,
    steer_tx: mpsc::UnboundedSender<String>,
    done: oneshot::Sender<Result<TurnReport>>,
    future: Pin<Box<dyn Future<Output = Result<()>> + Send>>,
}

struct ScriptedActor {
    session_id: String,
    _model: String,
    context_window: u64,
    context_usage_enabled: bool,
    options: SessionOptions,
    secrets: Vec<String>,
    journal: Journal,
    steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
    fallback: Option<ScriptedStep>,
    totals: TokenTotals,
    queued: VecDeque<(Uuid, Input, oneshot::Sender<Result<TurnReport>>)>,
    questions: HashMap<String, PendingQuestion>,
    question_reg_rx: Option<mpsc::UnboundedReceiver<(String, PendingQuestion)>>,
    question_reg_tx: mpsc::UnboundedSender<(String, PendingQuestion)>,
    exit_rx: Option<mpsc::UnboundedReceiver<i32>>,
    exit_tx: mpsc::UnboundedSender<i32>,
    active: Option<ActiveTurn>,
}

impl ScriptedActor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: String,
        model: String,
        context_window: u64,
        context_usage_enabled: bool,
        options: SessionOptions,
        journal: Journal,
        steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
        fallback: Option<ScriptedStep>,
    ) -> Self {
        let secrets = crate::options::session_secrets(&options);
        let (question_reg_tx, question_reg_rx) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = mpsc::unbounded_channel();
        Self {
            session_id,
            _model: model,
            context_window,
            context_usage_enabled,
            options,
            secrets,
            journal,
            steps,
            fallback,
            totals: TokenTotals::default(),
            queued: VecDeque::new(),
            questions: HashMap::new(),
            question_reg_rx: Some(question_reg_rx),
            question_reg_tx,
            exit_rx: Some(exit_rx),
            exit_tx,
            active: None,
        }
    }

    fn next_step(&self, input_text: &str) -> ScriptedStep {
        self.steps
            .lock()
            .unwrap()
            .pop_front()
            .or_else(|| self.fallback.clone())
            .unwrap_or_else(|| ScriptedStep::ReplyText(format!("echo: {input_text}")))
    }

    fn record_prompt_dispatch(&self, id: Uuid, input: &Input, from_queue: bool) {
        let mut body = json!({
            "type": "prompt",
            "id": input.request_id,
            "inputId": id,
            "message": input.text,
            "imageCount": input.images.len(),
        });
        redact(&mut body, &self.secrets);
        let mut user_text = input.text.clone();
        crate::transport::redact_text(&mut user_text, &self.secrets);

        let stamp = RecordStamp::from(Direction::ToRuntime)
            .with_input(input)
            .with_prompt_request(
                id,
                false,
                input.request_id,
                crate::ImageSubmission::loaded(&input.text, &input.images, &self.secrets),
            );
        self.journal.push(
            stamp,
            RecordKind::Request,
            Some(self.session_id.clone()),
            vec![],
            body,
            vec![
                Event::PromptDispatched,
                Event::UserMessage { text: user_text },
            ],
        );
        let _ = from_queue;
        self.journal.control_decision(input.request_id, None);
    }

    fn launch_turn(
        &mut self,
        input_id: Uuid,
        input: Input,
        done: oneshot::Sender<Result<TurnReport>>,
        from_queue: bool,
    ) {
        self.record_prompt_dispatch(input_id, &input, from_queue);
        self.totals.input = self
            .totals
            .input
            .saturating_add(estimate_tokens(&input.text));

        let step = self.next_step(&input.text);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();

        let shared = Arc::new(TurnShared {
            session_id: self.session_id.clone(),
            journal: self.journal.clone(),
            approvals: self.options.approvals,
            input_text: input.text.clone(),
            input_id,
            images: input.images.clone(),
            options: self.options.clone(),
            cancel_rx,
            steered_log: Mutex::new(Vec::new()),
            steer_rx: tokio::sync::Mutex::new(steer_rx),
            question_reg_tx: self.question_reg_tx.clone(),
            exit_tx: self.exit_tx.clone(),
            output_messages: Mutex::new(Vec::new()),
            partial_text: Mutex::new(String::new()),
            structured_output: Mutex::new(None),
            failure: Mutex::new(None),
            output_tokens: Mutex::new(0),
            reasoning_tokens: Mutex::new(0),
            next_seq: Mutex::new(0),
        });

        self.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/turn_started"),
            RecordKind::Frame,
            Some(self.session_id.clone()),
            vec![],
            json!({ "type": "scripted/turn_started", "inputId": input_id }),
            vec![Event::TurnStarted],
        );

        let ctx = ScriptedTurnContext {
            inner: Arc::clone(&shared),
        };
        let future = Box::pin(execute_step(step, ctx));

        self.active = Some(ActiveTurn {
            input_id,
            request_id: input.request_id,
            shared,
            cancel_tx,
            steer_tx,
            done,
            future,
        });
    }

    fn settle_active_turn(
        &mut self,
        forced_outcome: Option<Outcome>,
        step_result: Option<Result<()>>,
    ) {
        let Some(active) = self.active.take() else {
            return;
        };
        let _ = active.cancel_tx.send(true);
        self.questions.clear();

        let out_tokens = *active.shared.output_tokens.lock().unwrap();
        let reason_tokens = *active.shared.reasoning_tokens.lock().unwrap();
        self.totals.output = self.totals.output.saturating_add(out_tokens);
        if reason_tokens > 0 {
            self.totals.reasoning = Some(
                self.totals
                    .reasoning
                    .unwrap_or(0)
                    .saturating_add(reason_tokens),
            );
        }

        let mut failure = active.shared.failure.lock().unwrap().take();
        let outcome = if let Some(forced) = forced_outcome {
            forced
        } else if let Some(Err(err)) = step_result {
            if failure.is_none() {
                failure = Some(TurnFailure {
                    class: crate::FailureClass::Unknown,
                    reason: err.to_string(),
                    status: None,
                    credential: None,
                    resets_at_ms: None,
                    rate_limit_type: None,
                });
            }
            Outcome::Failed
        } else if failure.is_some() {
            Outcome::Failed
        } else {
            Outcome::Completed
        };

        let partial = std::mem::take(&mut *active.shared.partial_text.lock().unwrap());
        let mut messages = active.shared.output_messages.lock().unwrap().clone();
        if !partial.is_empty() {
            messages.push(partial);
        }
        let text = messages.join("\n");
        let structured_output = active.shared.structured_output.lock().unwrap().clone();

        let mut end_events = vec![Event::TokenUsage {
            reading: TokenReading {
                cache_write_by_ttl: None,
                tokens: Some(self.totals),
                total: Some(self.totals),
            },
        }];
        if self.context_usage_enabled {
            let used = u64::try_from(self.totals.input.saturating_add(self.totals.output).max(0))
                .unwrap_or(0);
            let percent = (self.context_window > 0)
                .then(|| (used as f64 / self.context_window as f64) * 100.0);
            end_events.push(Event::ContextUsage {
                usage: ContextUsage {
                    tokens: Some(used),
                    context_window: Some(self.context_window),
                    percent,
                },
            });
        }
        if let Some(f) = &failure {
            end_events.push(Event::TurnFailure { failure: f.clone() });
        }
        end_events.push(Event::TurnEnded {
            outcome: outcome.clone(),
        });

        self.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "scripted/end"),
            RecordKind::Frame,
            Some(self.session_id.clone()),
            vec![],
            json!({
                "type": "scripted/end",
                "outcome": outcome,
            }),
            end_events,
        );

        let _ = active.done.send(Ok(TurnReport {
            native_input_receipt: None,
            outcome,
            text,
            structured_output,
            failure,
        }));

        if let Some((next_id, next_input, next_done)) = self.queued.pop_front() {
            let carried_input = next_input.dispatch_queued(next_id);
            self.launch_turn(next_id, carried_input, next_done, true);
        }
    }

    fn drain_question_registrations(
        questions: &mut HashMap<String, PendingQuestion>,
        question_reg_rx: &mut mpsc::UnboundedReceiver<(String, PendingQuestion)>,
    ) {
        while let Ok((id, q)) = question_reg_rx.try_recv() {
            questions.insert(id, q);
        }
    }

    fn handle_command(
        &mut self,
        cmd: Command,
        question_reg_rx: &mut mpsc::UnboundedReceiver<(String, PendingQuestion)>,
    ) -> Option<oneshot::Sender<Result<()>>> {
        if let Some(err) = cmd.reject_unreachable(&self.journal, Protocol::Pi, &self.secrets) {
            match cmd {
                Command::Prompt(_, _, _, ack, _)
                | Command::Queue(_, _, ack, _)
                | Command::Withdraw(_, ack)
                | Command::Send(_, ack)
                | Command::Respond(_, _, ack) => {
                    let _ = ack.send(Err(err));
                }
                Command::Steer(_, _, _, ack)
                | Command::StopTask(_, _, ack)
                | Command::Rpc(_, _, _, ack) => {
                    let _ = ack.send(Err(err));
                }
                Command::Abort(req) => {
                    let _ = req.reply.send(Err(err));
                }
                Command::Compact(_, _, ack) => {
                    let _ = ack.send(Err(err));
                }
                Command::Close(ack) => return Some(ack),
                Command::WorkbenchMcpReply(_) => {}
            }
            return None;
        }

        match cmd {
            Command::Prompt(id, input, _admission, ack, done) => {
                if self.active.is_some() {
                    let err = self.journal.busy_prompt(id, &input, false, &self.secrets);
                    let _ = ack.send(Err(err));
                } else {
                    self.launch_turn(id, input, done, false);
                    let _ = ack.send(Ok(()));
                }
            }
            Command::Queue(id, input, ack, done) => {
                let req = input.request_id;
                self.journal.queue_request(id, &input, &self.secrets);
                self.journal.queue_accepted(req, id, true, None);
                let _ = ack.send(Ok(()));
                if self.active.is_none() {
                    let carried = input.dispatch_queued(id);
                    self.launch_turn(id, carried, done, true);
                } else {
                    self.queued.push_back((id, input, done));
                }
            }
            Command::Withdraw(id, ack) => {
                let res = self.journal.withdraw_held(&mut self.queued, id);
                let _ = ack.send(res);
            }
            Command::Steer(id, input, target, ack) => {
                let native_owner = self.active.as_ref().is_some_and(|act| {
                    target.is_none_or(|t| {
                        t.input_id == act.input_id && t.request_id == Some(act.request_id)
                    })
                });
                let (request, ack) = self.journal.admit_steer(
                    id,
                    &input,
                    target,
                    native_owner,
                    &self.secrets,
                    ack,
                )?;
                if let Some(active) = &self.active {
                    let mut clean_text = input.text.clone();
                    crate::transport::redact_text(&mut clean_text, &self.secrets);
                    self.totals.input = self
                        .totals
                        .input
                        .saturating_add(estimate_tokens(&input.text));
                    active
                        .shared
                        .steered_log
                        .lock()
                        .unwrap()
                        .push(input.text.clone());
                    let _ = active.steer_tx.send(input.text);
                    self.journal.push(
                        RecordStamp::frame(Direction::FromRuntime, "scripted/steer"),
                        RecordKind::Frame,
                        Some(self.session_id.clone()),
                        vec![],
                        json!({
                            "type": "scripted/steer",
                            "inputId": id,
                            "text": clean_text,
                        }),
                        vec![Event::SteeringInput {
                            input_id: id,
                            chat_turn: Some(active.input_id),
                            text: clean_text,
                            status: SteerStatus::Accepted,
                        }],
                    );
                    let ok = Ok(Value::Null);
                    self.journal.control_result(request, &ok, &self.secrets);
                    let _ = ack.send(ok);
                } else {
                    let err = Err(Error::Refused("no active turn".into()));
                    self.journal.control_result(request, &err, &self.secrets);
                    let _ = ack.send(err);
                }
            }
            Command::Abort(req) => {
                let ack = req.for_current(&self.journal)?;
                let abort_id = self.journal.request_abort();
                if self.active.is_some() {
                    let ok = Ok(Value::Null);
                    self.journal.control_result(abort_id, &ok, &self.secrets);
                    self.settle_active_turn(Some(Outcome::Aborted), None);
                    let _ = ack.send(ok);
                } else {
                    let err: Result<Value> = Err(Error::Refused("no active turn".into()));
                    self.journal.control_result(abort_id, &err, &self.secrets);
                    let _ = ack.send(err);
                }
            }
            Command::Respond(id, host_answer, ack) => {
                Self::drain_question_registrations(&mut self.questions, question_reg_rx);
                let pending_req = self.journal.pending_request(&id, host_answer.cursor());
                let key = id
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| id.to_string());
                let result = if let Some(q) = self.questions.remove(&key) {
                    match host_answer.payload(&self.journal, &id, &q.method, &q.body) {
                        Ok(payload) => {
                            self.journal
                                .host_event(Event::RequestResolved { id: id.clone() });
                            if let Some(req) = pending_req {
                                self.journal.host_event(Event::RequestAnswerDispatched {
                                    cursor: req.cursor,
                                    id: req.id,
                                    method: req.method,
                                    kind: req.kind,
                                    confirmed: true,
                                });
                            }
                            let _ = q.answer_tx.send(payload);
                            Ok(())
                        }
                        Err(err) => {
                            self.questions.insert(key, q);
                            Err(err)
                        }
                    }
                } else {
                    Err(Error::Refused(
                        "unknown scripted interaction request".into(),
                    ))
                };
                let _ = ack.send(result);
            }
            Command::Compact(req_id, _, ack) => {
                let err = Err(Error::Unsupported(
                    "scripted runtime does not support manual compaction".into(),
                ));
                self.journal.control_result(req_id, &err, &self.secrets);
                let _ = ack.send(err);
            }
            Command::StopTask(_, _, ack) => {
                let _ = ack.send(Err(Error::Unsupported(
                    "scripted runtime does not support stop_task".into(),
                )));
            }
            Command::Rpc(method, _, _, ack) => {
                let res = match method.as_str() {
                    "get_available_models" => Ok(json!({
                        "models": [{ "id": self._model, "name": self._model }]
                    })),
                    _ => Err(Error::Unsupported(format!(
                        "unsupported scripted RPC {method:?}"
                    ))),
                };
                let _ = ack.send(res);
            }
            Command::Send(_, ack) => {
                let _ = ack.send(Err(Error::Unsupported("raw send unsupported".into())));
            }
            Command::WorkbenchMcpReply(_) => {}
            Command::Close(ack) => return Some(ack),
        }
        None
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        enum ExitMode {
            Dispose(Option<oneshot::Sender<Result<()>>>),
            Unexpected(i32),
        }

        let mut question_reg_rx = self
            .question_reg_rx
            .take()
            .expect("question_reg_rx initialized");
        let mut exit_rx = self.exit_rx.take().expect("exit_rx initialized");

        let exit_mode = loop {
            let active_fut = futures_util::future::OptionFuture::from(
                self.active.as_mut().map(|a| a.future.as_mut()),
            );
            tokio::select! {
                biased;
                Some((qid, q)) = question_reg_rx.recv() => {
                    self.questions.insert(qid, q);
                }
                Some(code) = exit_rx.recv() => {
                    break ExitMode::Unexpected(code);
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            if let Some(close_ack) = self.handle_command(cmd, &mut question_reg_rx) {
                                break ExitMode::Dispose(Some(close_ack));
                            }
                        }
                        None => break ExitMode::Dispose(None),
                    }
                }
                Some(step_res) = active_fut => {
                    Self::drain_question_registrations(&mut self.questions, &mut question_reg_rx);
                    self.settle_active_turn(None, Some(step_res));
                }
            }
        };

        match exit_mode {
            ExitMode::Dispose(close_ack) => {
                let dispose_id = close_ack.as_ref().map(|_| self.journal.request_dispose());
                let reason = if close_ack.is_some() {
                    "closed".to_owned()
                } else {
                    "controller closed".to_owned()
                };
                if let Some(active) = self.active.take() {
                    let _ = active.cancel_tx.send(true);
                    let _ = active.done.send(Err(Error::Closed(reason.clone())));
                }
                for (_, _, done) in self.queued.drain(..) {
                    let _ = done.send(Err(Error::Closed(reason.clone())));
                }
                if let Some(id) = dispose_id {
                    self.journal.control_outcome(
                        id,
                        json!({ "type": "exited", "code": 0, "reason": reason }),
                        Event::Exited { reason },
                    );
                } else {
                    self.journal.push(
                        Direction::Host,
                        RecordKind::Response,
                        None,
                        vec![],
                        json!({ "type": "exited", "code": 0, "reason": reason }),
                        vec![Event::Exited { reason }],
                    );
                }
                self.journal.close();
                crate::session::reachability::reject_pending(
                    &mut rx,
                    &self.journal,
                    Protocol::Pi,
                    &self.secrets,
                )
                .await;
                if let Some(ack) = close_ack {
                    let _ = ack.send(Ok(()));
                }
            }
            ExitMode::Unexpected(code) => {
                let reason = format!("exit status {code}");
                if let Some(active) = self.active.take() {
                    let _ = active.cancel_tx.send(true);
                    let _ = active.done.send(Err(Error::Closed(reason.clone())));
                }
                for (_, _, done) in self.queued.drain(..) {
                    let _ = done.send(Err(Error::Closed(reason.clone())));
                }
                self.journal.push(
                    Direction::Host,
                    RecordKind::Response,
                    None,
                    vec![],
                    json!({ "type": "exited", "code": code, "reason": reason }),
                    vec![Event::Exited { reason }],
                );
                self.journal.close();
                crate::session::reachability::reject_pending(
                    &mut rx,
                    &self.journal,
                    Protocol::Pi,
                    &self.secrets,
                )
                .await;
            }
        }
    }
}

async fn execute_step(step: ScriptedStep, ctx: ScriptedTurnContext) -> Result<()> {
    // Yield once so prompt admission records always precede streaming turn frames.
    tokio::task::yield_now().await;
    match step {
        ScriptedStep::ReplyText(text) => {
            ctx.say(text);
            Ok(())
        }
        ScriptedStep::StreamDeltas {
            chunks,
            phase,
            delay,
        } => {
            let msg_id = ctx.alloc_id("stream-msg");
            let is_reasoning = phase.as_deref() == Some("thinking");
            for chunk in chunks {
                if ctx.is_cancelled() {
                    return Ok(());
                }
                if let Some(d) = delay {
                    tokio::select! {
                        biased;
                        _ = ctx.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(d) => {}
                    }
                }
                if is_reasoning {
                    ctx.think(chunk);
                } else {
                    ctx.say_delta(&msg_id, chunk, phase.as_deref());
                }
            }
            if !is_reasoning {
                ctx.finish_message(&msg_id, phase.as_deref());
            }
            Ok(())
        }
        ScriptedStep::ToolCall {
            name,
            input,
            output,
            status,
            reply_text,
        } => {
            ctx.tool_call(name, input, output, status);
            if let Some(reply) = reply_text {
                ctx.say(reply);
            }
            Ok(())
        }
        ScriptedStep::AskQuestion {
            id,
            question,
            options,
        } => {
            let answer = ctx.ask_question(&id, question, options).await?;
            let rendered = answer
                .pointer(&format!("/answers/{id}/answers/0"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| answer.to_string());
            ctx.say(format!("answer:{rendered}"));
            Ok(())
        }
        ScriptedStep::Fail(failure) => {
            ctx.fail(failure);
            Ok(())
        }
        ScriptedStep::WaitForSteer { reply_prefix } => {
            if let Some(steered) = ctx.wait_for_steer().await {
                let prefix = reply_prefix.unwrap_or_else(|| "steered:".into());
                ctx.say(format!("{prefix}{steered}"));
            }
            Ok(())
        }
        ScriptedStep::Custom(handler) => handler(ctx).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalMode, FailureClass, QuestionReply, Record, SessionPlacement};

    #[tokio::test]
    async fn scripted_runtime_drives_reply_deltas_tools_failures_and_usage_snapshots() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let rt = scripted_runtime(
            "mock-agent",
            vec![
                ScriptedStep::reply("Hello from scripted turn 1!"),
                ScriptedStep::deltas_with_phase(
                    ["part-A ", "part-B ", "part-C"],
                    Some("final_answer".into()),
                    None,
                ),
                ScriptedStep::ToolCall {
                    name: "read_file".into(),
                    input: json!({ "path": "Cargo.toml" }),
                    output: "[package]\nname = \"kar\"".into(),
                    status: ToolStatus::Succeeded,
                    reply_text: Some("Read Cargo.toml.".into()),
                },
                ScriptedStep::fail(FailureClass::RateLimited, "429 burst limit"),
            ],
        );

        let session = rt.open(SessionOptions::new(dir.path())).await?;
        assert_eq!(session.placement(), Some(SessionPlacement::InProcess));
        assert_eq!(session.model().as_deref(), Some("scripted-1"));

        // 1. ReplyText
        let report1 = session.prompt("first prompt").await?.report().await?;
        assert_eq!(report1.outcome, Outcome::Completed);
        assert_eq!(report1.text, "Hello from scripted turn 1!");

        // 2. StreamDeltas + live coalescing
        let cursor_before_2 = session.snapshot().cursor.unwrap();
        let report2 = session.prompt("stream it").await?.report().await?;
        assert_eq!(report2.outcome, Outcome::Completed);
        assert_eq!(report2.text, "part-A part-B part-C");

        let records: Vec<Record> = session
            .records()
            .into_iter()
            .filter(|r| r.seq > cursor_before_2.after_seq)
            .map(|r| (*r).clone())
            .collect();
        let coalesced = crate::observe::coalesce_events(&records, true);
        let merged_deltas: Vec<&str> = coalesced
            .iter()
            .filter_map(|ev| match &ev.event {
                Event::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(merged_deltas, vec!["part-A part-B part-C"]);

        // 3. ToolCall
        let report3 = session.prompt("inspect Cargo.toml").await?.report().await?;
        assert_eq!(report3.outcome, Outcome::Completed);
        assert_eq!(report3.text, "Read Cargo.toml.");

        // 4. Fail
        let report4 = session.prompt("trigger rate limit").await?.report().await?;
        assert_eq!(report4.outcome, Outcome::Failed);
        assert_eq!(
            report4.failure.as_ref().map(|f| f.class),
            Some(FailureClass::RateLimited)
        );

        // Deterministic cumulative UsageSnapshot + ContextUsage
        let usage = session.usage_snapshot();
        let totals = usage.usage.total.expect("tokens tracked");
        assert!(totals.input > 0);
        assert!(totals.output > 0);
        assert!(session.snapshot().context_usage.is_some());

        session.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn scripted_runtime_supports_steer_queue_withdraw_abort_and_questions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let rt = scripted_runtime(
            "interactive-agent",
            vec![
                ScriptedStep::wait_for_steer(),
                ScriptedStep::question("db", "Choose database", ["sqlite", "postgres"]),
                ScriptedStep::custom(|ctx| async move {
                    ctx.cancelled().await;
                    Ok(())
                }),
                ScriptedStep::reply("queued survivor"),
            ],
        );

        let mut opts = SessionOptions::new(dir.path());
        opts.approvals = ApprovalMode::Host;
        let session = rt.open(opts).await?;

        // 1. WaitForSteer + Session::steer
        let steer_turn = session.prompt("start working").await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        session.steer("focus on tests").await?;
        let steer_report = steer_turn.report().await?;
        assert_eq!(steer_report.outcome, Outcome::Completed);
        assert_eq!(steer_report.text, "steered:focus on tests");

        // 2. AskQuestion + Session::answer_question
        let q_turn = session.prompt("configure storage").await?;
        let pending = loop {
            let snap = session.snapshot();
            if let Some(req) = snap.pending_requests.first() {
                break req.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let mut values = serde_json::Map::new();
        values.insert("db".into(), Value::String("sqlite".into()));
        session
            .answer_question(&pending, QuestionReply::Submit { values })
            .await?;
        let q_report = q_turn.report().await?;
        assert_eq!(q_report.outcome, Outcome::Completed);
        assert_eq!(q_report.text, "answer:sqlite");

        // 3. Active turn + Queue + Withdraw + Abort -> remaining queued item runs!
        let blocking_turn = session.prompt("hold open").await?;
        let (withdrawn_id, withdrawn_turn) = session.queue("cancel me").await?;
        let (_kept_id, kept_turn) = session.queue("run me next").await?;
        session.withdraw(withdrawn_id).await?;
        assert!(matches!(
            withdrawn_turn.report().await,
            Err(Error::Refused(_))
        ));

        session.abort().await?;
        let aborted_report = blocking_turn.report().await?;
        assert_eq!(aborted_report.outcome, Outcome::Aborted);

        let kept_report = kept_turn.report().await?;
        assert_eq!(kept_report.outcome, Outcome::Completed);
        assert_eq!(kept_report.text, "queued survivor");

        session.close().await?;
        Ok(())
    }
}
