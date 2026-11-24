#[cfg(not(target_arch = "wasm32"))]
mod queue;
#[cfg(not(target_arch = "wasm32"))]
mod reachability;
pub(crate) mod retention;
mod text;
pub use text::MessageBoundary;
#[cfg(not(target_arch = "wasm32"))]
pub use text::CoalescedEvents;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub stream_id: Uuid,
    pub after_seq: u64,
}
/// An event position includes its index because one native frame can produce several events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub stream_id: Uuid,
    pub seq: u64,
    pub index: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservedEvent {
    pub cursor: EventCursor,
    pub timestamp_ms: u64,
    pub session_id: Option<String>,
    pub agent_path: Vec<String>,
    pub span_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub elided: bool,
    #[serde(default, flatten)]
    pub message: MessageBoundary,
    pub event: Event,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    ToRuntime,
    FromRuntime,
    Host,
}
/// Captured before projecting a packet so relative deadlines and the
/// journal record share the same clock reading.
#[derive(Clone)]
pub(crate) struct RecordStamp {
    pub direction: Direction,
    pub timestamp_ms: u64,
    prompt: Option<PromptIdentity>,
    pub(crate) response_to: Option<String>,
    opening: Option<crate::Opening>,
    frame_type: Option<String>,
    pub(crate) carried: Vec<Uuid>,
    pub(crate) origin: Option<InputOrigin>,
}

impl From<Direction> for RecordStamp {
    fn from(direction: Direction) -> Self {
        Self {
            direction,
            timestamp_ms: timestamp_ms(),
            response_to: None,
            opening: None,
            frame_type: None,
            carried: Vec::new(),
            origin: None,
            prompt: None,
        }
    }
}
/// Origin declared by the host, kept out of the native message payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputOrigin {
    pub kind: InputOriginKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputOriginKind {
    User,
    Notification,
    Automation,
}

/// Host identity of an ordinary prompt. Native admission is reported by Pi RPC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptIdentity {
    /// Unique identity of this control attempt, independent of the caller's input ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub input_id: String,
    pub native_admission: bool,
    /// Original text and image references of an input with attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission: Option<ImageSubmission>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSubmission {
    pub input: String,
    pub images: Vec<ImageReference>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageReference {
    /// Absent for a caller-provided in-memory image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl RecordStamp {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn with_input(mut self, input: &crate::input::Input) -> Self {
        self.origin = input.origin.clone();
        self.carried = input.carried.clone();
        self
    }

    pub(crate) fn frame(direction: Direction, name: impl Into<String>) -> Self {
        Self::from(direction).with_frame_type(name)
    }

    pub(crate) fn with_frame_type(mut self, name: impl Into<String>) -> Self {
        self.frame_type = Some(name.into());
        self
    }
    pub(crate) fn with_prompt_request(
        mut self,
        input_id: impl ToString,
        native_admission: bool,
        request: Uuid,
        submission: Option<ImageSubmission>,
    ) -> Self {
        self.prompt = Some(PromptIdentity {
            request_id: Some(request.to_string()),
            input_id: input_id.to_string(),
            native_admission,
            submission,
        });
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Frame,
    Request,
    Response,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpStartupStatus {
    Pending,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerStatus {
    Submitted,
    /// The native input is waiting for a model step.
    Queued,
    /// The runtime acknowledged this input without confirming model consumption.
    Received,
    /// A native extension consumed the steer without creating a model input.
    Handled,
    Accepted,
    /// The original loop ended before the runtime consumed this input.
    FollowUp,
    Cancelled,
    Refused,
    /// The controller lost the native connection before it could confirm delivery.
    Unconfirmed,
}
#[cfg(not(target_arch = "wasm32"))]
impl SteerStatus {
    pub(crate) fn is_waiting(self) -> bool {
        matches!(self, Self::Submitted | Self::Queued)
    }
}

/// A native receipt for a host input identified by its stable UUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputReceipt {
    Queued,
    Started,
    /// The runtime handled this input without starting a model turn.
    Handled,
    /// The runtime explicitly refused this input before starting a model turn.
    Refused,
    Cancelled,
    Duplicate,
}
impl InputReceipt {
    pub(crate) fn settled_outcome(self) -> Option<Outcome> {
        match self {
            Self::Handled => Some(Outcome::Completed),
            Self::Refused | Self::Duplicate => Some(Outcome::Failed),
            Self::Cancelled => Some(Outcome::Aborted),
            _ => None,
        }
    }
}

/// Outcome of process cleanup requested by a particular native interrupt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnProcessCleanup {
    pub turn_id: String,
    pub request_id: u64,
    pub result: ProcessCleanupResult,
}

#[cfg(not(target_arch = "wasm32"))]
impl TurnProcessCleanup {
    pub(crate) fn publish(self, journal: &Journal, session: Option<String>) {
        let response = serde_json::json!({"type":"cleanup", "requestId":self.request_id, "result":self.result});
        journal.append(
            Direction::Host,
            RecordKind::Response,
            session,
            vec![],
            Some(self.turn_id.clone()),
            response,
            vec![Event::TurnProcessCleanup { cleanup: self }],
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessCleanupResult {
    Completed {
        signalled: usize,
        #[serde(default, skip_serializing_if = "is_zero")]
        foreign: usize,
    },
    Failed {
        reason: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        signalled: usize,
        #[serde(default, skip_serializing_if = "is_zero")]
        foreign: usize,
    },
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    McpServerStartup {
        server: String,
        status: McpStartupStatus,
    },
    TurnAbortRequested {
        turn_id: String,
        request_id: u64,
    },
    TurnProcessCleanup {
        cleanup: TurnProcessCleanup,
    },
    PromptDispatched,
    /// A separate execution started by the native runtime itself.
    NativeTurnStarted {
        id: Uuid,
        native_turn: String,
    },
    /// Independent model activity observed while a native host input is waiting.
    NotificationActivity {
        id: Uuid,
    },
    /// Claude consumed a steering input as the first input of a separate model loop.
    SteeringTurnStarted {
        input_id: Uuid,
        from: Uuid,
    },
    /// A previously submitted input joined a different native activity.
    SteeringMoved {
        input_id: Uuid,
        from: Uuid,
        to: Uuid,
    },
    /// The native model loop incorporated the host input at a tool boundary.
    NotificationJoined {
        id: Uuid,
        input_id: Uuid,
    },
    NativeInput {
        input_id: Uuid,
        receipt: InputReceipt,
    },
    InputQueued {
        input_id: String,
    },
    InputRemoved {
        input_id: String,
    },
    ServiceTier {
        tier: String,
    },
    SessionLinked {
        parent: String,
        child: String,
    },
    Released,
    InputDropped {
        input_id: String,
    },
    Effort {
        effort: String,
    },
    Model {
        model: String,
    },
    ManualCompactionRequested,
    ManualCompaction {
        report: crate::ManualCompactionReport,
    },
    Compaction {
        state: String,
    },
    Retry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_attempts: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    UserMessage {
        text: String,
    },
    /// One steering input across its native submission and acknowledgement.
    SteeringInput {
        input_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_turn: Option<Uuid>,
        text: String,
        status: SteerStatus,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    Message {
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
    },
    ToolUpdated {
        id: String,
    },
    ToolIdentity {
        id: String,
        native_type: String,
        server: Option<String>,
        tool_name: Option<String>,
    },
    ToolInputDelta {
        id: String,
        delta: String,
    },
    ToolInput {
        id: String,
        input: Value,
    },
    ToolProgress {
        id: String,
        output: Option<String>,
        delta: Option<String>,
    },
    ToolResult {
        id: String,
        result: crate::ToolResult,
    },
    /// Latest cumulative patch reported by the runtime for this turn.
    TurnDiff {
        turn_id: String,
        diff: String,
    },
    ToolEnded {
        id: String,
    },
    TokenUsage {
        #[serde(flatten)]
        reading: crate::TokenReading,
    },
    Usage {
        usage: Value,
    },
    TurnStarted,
    ForkAnchor {
        anchor: String,
    },
    Task {
        id: String,
        change: crate::TaskChange,
    },
    ContextUsage {
        usage: crate::ContextUsage,
    },
    GoalChanged {
        goal: Option<crate::GoalSnapshot>,
    },
    /// Native structured value on a completed turn, including an explicit JSON null.
    StructuredOutput {
        value: Value,
    },
    TurnFailure {
        failure: crate::TurnFailure,
    },
    TurnEnded {
        outcome: Outcome,
    },
    ApprovalRequested {
        id: Value,
        method: String,
    },
    RequestResolved {
        id: Value,
    },
    Exited {
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Aborted,
    Failed,
}
impl Event {
    pub(crate) fn codex_mcp_startup(params: &Value) -> Option<Self> {
        params["threadId"].as_str().filter(|id| !id.is_empty())?;
        let name = params["name"].as_str().filter(|name| !name.is_empty())?;
        let status = match params["status"].as_str()? {
            "starting" => McpStartupStatus::Pending,
            "ready" => McpStartupStatus::Ready,
            "failed" => McpStartupStatus::Failed,
            "cancelled" => McpStartupStatus::Cancelled,
            _ => return None,
        };
        Some(Self::McpServerStartup {
            server: name.into(),
            status,
        })
    }

    pub(crate) fn resolved_input(&self) -> Option<Outcome> {
        match self {
            Self::TurnEnded { outcome } => Some(outcome.clone()),
            Self::NativeInput { receipt, .. } => receipt.settled_outcome(),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub stream_id: Uuid,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub direction: Direction,
    pub kind: RecordKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptIdentity>,
    /// Opening metadata belongs to the Kar lifecycle, independently of native wire packets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opening: Option<crate::Opening>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<InputOrigin>,
    /// Inputs previously admitted to an adapter queue and carried by this dispatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carries: Vec<String>,
    /// The wire discriminator or protocol operation that produced a frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_type: Option<String>,
    /// Public control request answered by this record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(default)]
    pub agent_path: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub elided: bool,
    /// Input identities preserved when the in-memory replay budget elides a native frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elided_input_echoes: Vec<crate::observe::InputEcho>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub message_boundaries: std::collections::BTreeMap<usize, MessageBoundary>,
    pub native: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<Value>,
    pub events: Vec<Event>,
}
// Called only for a present field. Serde's regular Option maps explicit null to absence.
pub(crate) fn present_json<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}
/// Both public records of one control attempt, in the original session stream.
#[derive(Clone, Debug)]
pub struct ControlReceipt {
    pub request: Arc<Record>,
    pub response: Arc<Record>,
}

impl ControlReceipt {
    /// Finds the decision paired with this exact request, including in replayed records.
    pub fn from_records(records: &[Arc<Record>], request_id: &str) -> Option<Self> {
        if request_id.is_empty() {
            return None;
        }
        let mut response = None;
        for record in records.iter().rev() {
            if record.control_response_to() == Some(request_id) {
                response = Some(record);
            } else if record.control_request_id() == Some(request_id) {
                let response = response?;
                if response.stream_id == record.stream_id && response.seq > record.seq {
                    return Some(Self {
                        request: record.clone(),
                        response: response.clone(),
                    });
                }
                return None;
            }
        }
        None
    }
}

impl Record {
    pub(crate) fn wire_frame_type(value: &Value) -> Option<&str> {
        ["method", "type", "kind", "event"]
            .iter()
            .find_map(|key| value[*key].as_str().filter(|s| !s.is_empty()))
    }

    /// Legacy records may already carry the discriminator in their native envelope.
    pub fn frame_type(&self) -> Option<&str> {
        if self.kind != RecordKind::Frame {
            return None;
        }
        self.frame_type
            .as_deref()
            .or_else(|| self.upstream.as_ref()?.pointer("/body/type")?.as_str())
            .or_else(|| Self::wire_frame_type(&self.native))
    }

    /// Request named by a public decision, including decisions in older recordings.
    pub fn control_response_to(&self) -> Option<&str> {
        if self.kind != RecordKind::Response || self.direction != Direction::Host {
            return None;
        }
        self.response_to
            .as_deref()
            .or_else(|| self.native["requestId"].as_str())
    }

    /// Identifier used by the paired response to a Kar control request.
    pub fn control_request_id(&self) -> Option<&str> {
        if self.kind != RecordKind::Request || self.direction == Direction::FromRuntime {
            return None;
        }
        if let Some(crate::Opening::Open { request_id, .. }) = &self.opening {
            return Some(request_id);
        }
        self.prompt
            .as_ref()
            .and_then(|p| p.request_id.as_deref())
            .or_else(|| self.upstream.as_ref()?.get("id")?.as_str())
            .or_else(|| self.native.get("id")?.as_str())
    }

    pub fn structured_output(&self) -> Option<&Value> {
        self.events.iter().find_map(|event| match event {
            Event::StructuredOutput { value } => Some(value),
            _ => None,
        })
    }
    /// The native reason for dropping this exact input, when the recorded evidence identifies it.
    pub fn input_drop_reason(&self, id: &str) -> Option<&str> {
        if let Some(events) = self
            .upstream
            .as_ref()
            .and_then(|r| r.pointer("/body/events"))
            .and_then(Value::as_array)
        {
            return events
                .iter()
                .find(|e| e["kind"] == "input_dropped" && e["inputId"] == id)?
                .get("reason")?
                .as_str();
        }
        if !self
            .events
            .iter()
            .any(|e| matches!(e, Event::InputDropped { input_id } if input_id == id))
        {
            return None;
        }
        let interrupted = self
            .native
            .pointer("/params/turn/status")
            .and_then(Value::as_str)
            == Some("interrupted");
        let late_reply = self.frame_type() == Some("turn/steer") && self.response_to.is_some();
        ((interrupted || late_reply)
            && self.events.iter().any(|e| {
                matches!(e, Event::SteeringInput {
            status: SteerStatus::Cancelled, input_id, ..
        } if input_id.to_string() == id)
            }))
        .then_some("turn_interrupted")
    }

    /// Classified evidence attached to a terminal record by the runtime adapter.
    pub fn turn_failure(&self) -> Option<&crate::TurnFailure> {
        self.events.iter().find_map(|event| {
            if let Event::TurnFailure { failure } = event {
                Some(failure)
            } else {
                None
            }
        })
    }

    pub fn cursor(&self) -> Cursor {
        Cursor {
            stream_id: self.stream_id,
            after_seq: self.seq,
        }
    }
}

struct State {
    messages: text::MessageTracker,
    #[cfg(not(target_arch = "wasm32"))]
    chat: Option<crate::chat::Sink>,
    #[cfg(not(target_arch = "wasm32"))]
    claude_results: std::collections::HashSet<Uuid>,
    root: Option<String>,
    queries: Option<crate::observe::SessionReducer>,
    records: Vec<Arc<Record>>,
    closed: bool,
    #[cfg(not(target_arch = "wasm32"))]
    reachability: reachability::Reachability,
    observers: Vec<mpsc::UnboundedSender<Arc<Record>>>,
    sizes: VecDeque<(usize, usize)>,
    bytes: usize,
    measured: usize,
    retained_bytes: Option<usize>,
}
#[derive(Clone)]
pub(crate) struct Journal {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) delivery: Arc<crate::session::delivery::Coordinator>,
    id: Uuid,
    state: Arc<Mutex<State>>,
}
impl Journal {
    pub(crate) fn for_open(
        retained_bytes: Option<usize>,
        requested: crate::PlacementRequest,
        execution: Option<crate::Execution>,
    ) -> Result<Self> {
        let placement = requested.resolve(execution)?;
        let journal = Self::new(retained_bytes)?;
        if let Some(placement) = placement {
            let id = Uuid::new_v4().to_string();
            let mut request = RecordStamp::from(Direction::ToRuntime);
            request.opening = Some(crate::Opening::Open {
                request_id: id.clone(),
                placement: requested,
            });
            journal.push(
                request,
                RecordKind::Request,
                None,
                vec![],
                Value::Null,
                vec![],
            );
            let mut reply = RecordStamp::from(Direction::Host);
            reply.opening = Some(crate::Opening::Opened { placement });
            reply.response_to = Some(id);
            journal.push(
                reply,
                RecordKind::Response,
                None,
                vec![],
                Value::Null,
                vec![],
            );
        }
        Ok(journal)
    }

    pub(crate) fn placement(&self) -> Option<crate::SessionPlacement> {
        let state = self.state.lock().unwrap();
        crate::SessionPlacement::from_pair(state.records.first()?, state.records.get(1)?)
    }
    pub fn new(retained_bytes: Option<usize>) -> Result<Self> {
        retention::check_limit(retained_bytes)?;
        Ok(Self {
            #[cfg(not(target_arch = "wasm32"))]
            delivery: Default::default(),
            id: Uuid::new_v4(),
            state: Arc::new(Mutex::new(State {
                messages: Default::default(),
                #[cfg(not(target_arch = "wasm32"))]
                chat: None,
                #[cfg(not(target_arch = "wasm32"))]
                claude_results: Default::default(),
                root: None,
                queries: None,
                records: vec![],
                closed: false,
                #[cfg(not(target_arch = "wasm32"))]
                reachability: reachability::Reachability::default(),
                observers: Vec::new(),
                sizes: VecDeque::new(),
                bytes: 0,
                measured: 0,
                retained_bytes,
            })),
        })
    }
    pub fn id(&self) -> Uuid {
        self.id
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn attempted_prompt(
        &self,
        input_id: Uuid,
        input: &crate::input::Input,
        native: bool,
        secrets: &[String],
    ) -> Uuid {
        let request = input.request_id;
        let mut body = serde_json::json!({"control":"prompt", "input":input.text, "imageCount":input.images.len()});
        crate::transport::redact(&mut body, secrets);
        let stamp = RecordStamp::from(Direction::ToRuntime).with_input(input);
        self.push(
            stamp.with_prompt_request(
                input_id,
                native,
                request,
                ImageSubmission::loaded(&input.text, &input.images, secrets),
            ),
            RecordKind::Request,
            None,
            vec![],
            body,
            vec![],
        );
        request
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn busy_prompt(
        &self,
        id: Uuid,
        input: &crate::input::Input,
        native: bool,
        secrets: &[String],
    ) -> Error {
        let request = self.attempted_prompt(id, input, native, secrets);
        self.control_decision(request, Some(("busy", "busy")));
        Error::Busy
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn prompt_error(&self, request: Uuid, error: &Error, secrets: &[String]) {
        let (code, mut reason) = match error {
            Error::Busy => ("busy", "busy".to_owned()),
            Error::Refused(reason) => ("runtime_refused", reason.clone()),
            Error::Unsupported(reason) => ("unsupported", reason.clone()),
            _ => (
                "error",
                "native admission could not be confirmed".to_owned(),
            ),
        };
        crate::transport::redact_text(&mut reason, secrets);
        self.control_decision(request, Some((code, &reason)));
    }
    pub(crate) fn control_decision(&self, request: Uuid, rejection: Option<(&str, &str)>) {
        let body = match rejection {
            Some((code, reason)) => {
                serde_json::json!({"type":"rejected", "requestId":request, "code":code, "reason":reason})
            }
            None => serde_json::json!({"type":"accepted", "requestId":request}),
        };
        self.push(
            Direction::Host,
            RecordKind::Response,
            None,
            vec![],
            body,
            vec![],
        );
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn steer_request(
        &self,
        input_id: Uuid,
        input: &crate::input::Input,
        target: Option<crate::transport::TurnTarget>,
        native_owner: bool,
        secrets: &[String],
    ) -> Result<Uuid> {
        let request = input.request_id;
        let mut body = serde_json::json!({"type":"steer","id":request,"inputId":input_id,"input":input.text,"imageCount":input.images.len()});
        if let Some(images) = ImageSubmission::loaded(&input.text, &input.images, secrets) {
            body["images"] = serde_json::to_value(images.images)?;
        }
        if let Some(target) = target {
            body["targetInputId"] = target.input_id.to_string().into();
            if let Some(id) = target.request_id {
                body["targetRequestId"] = id.to_string().into();
            }
        }
        crate::transport::redact(&mut body, secrets);
        if target.is_some_and(|t| !native_owner || t.request_id.is_none() || t.pin(self).is_none())
        {
            let reason = "prompt no longer owns the active turn";
            self.refused_input(
                RecordStamp::from(Direction::ToRuntime).with_input(input),
                body,
                request,
                "no_active_turn",
                reason,
            )?;
            return Err(Error::Refused(reason.into()));
        }
        self.push(
            RecordStamp::from(Direction::ToRuntime).with_input(input),
            RecordKind::Request,
            None,
            vec![],
            body,
            vec![],
        );
        Ok(request)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn admit_steer(
        &self,
        id: Uuid,
        input: &crate::input::Input,
        target: Option<crate::transport::TurnTarget>,
        native_owner: bool,
        secrets: &[String],
        reply: tokio::sync::oneshot::Sender<Result<Value>>,
    ) -> Option<(Uuid, tokio::sync::oneshot::Sender<Result<Value>>)> {
        match self.steer_request(id, input, target, native_owner, secrets) {
            Ok(request) => Some((request, reply)),
            Err(err) => {
                let _ = reply.send(Err(err));
                None
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn control_result<T>(&self, request: Uuid, result: &Result<T>, secrets: &[String]) {
        match result {
            Ok(_) => self.control_decision(request, None),
            Err(Error::Refused(reason))
                if matches!(
                    reason.as_str(),
                    "no active turn"
                        | "no active DSH turn"
                        | "no running DSH compaction to cancel"
                        | "turn ended before the interrupt was sent"
                        | "runtime has not acknowledged this turn"
                ) =>
            {
                self.control_decision(request, Some(("no_active_turn", reason)))
            }
            Err(Error::Unsupported(_)) => self.control_decision(
                request,
                Some(("unsupported", "runtime does not support this operation")),
            ),
            Err(error) => self.prompt_error(request, error, secrets),
        }
    }
    pub(crate) fn request_abort(&self) -> Uuid {
        self.control_request(serde_json::json!({"type":"abort"}))
    }
    pub(crate) fn request_dispose(&self) -> Uuid {
        self.control_request(serde_json::json!({"type":"dispose"}))
    }
    pub(crate) fn control_request(&self, mut body: Value) -> Uuid {
        let id = Uuid::new_v4();
        body["id"] = id.to_string().into();
        self.push(
            Direction::ToRuntime,
            RecordKind::Request,
            None,
            vec![],
            body,
            vec![],
        );
        id
    }
    pub(crate) fn control_outcome(&self, id: Uuid, mut outcome: Value, event: Event) {
        outcome["requestId"] = id.to_string().into();
        self.push(
            Direction::Host,
            RecordKind::Response,
            None,
            vec![],
            outcome,
            vec![event],
        );
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn process_exited(
        &self,
        id: Option<Uuid>,
        reason: String,
        status: &std::io::Result<std::process::ExitStatus>,
    ) {
        if let Some(id) = id {
            self.control_outcome(
                id,
                crate::process::exit_body(status, &reason),
                Event::Exited { reason },
            );
        } else {
            self.push(
                Direction::Host,
                RecordKind::Response,
                None,
                vec![],
                crate::process::exit_body(status, &reason),
                vec![Event::Exited { reason }],
            );
        }
    }
    pub(crate) fn host_event(&self, event: Event) {
        self.push(
            RecordStamp::frame(Direction::Host, "kar/notification"),
            RecordKind::Frame,
            None,
            vec![],
            Value::Null,
            vec![event],
        );
    }
    pub fn push(
        &self,
        direction: impl Into<RecordStamp>,
        kind: RecordKind,
        session_id: Option<String>,
        agent_path: Vec<String>,
        native: Value,
        events: Vec<Event>,
    ) {
        let span_id = native
            .pointer("/params/turnId")
            .or_else(|| native.pointer("/params/turn/id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.append(
            direction, kind, session_id, agent_path, span_id, native, events,
        );
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append(
        &self,
        direction: impl Into<RecordStamp>,
        kind: RecordKind,
        session_id: Option<String>,
        agent_path: Vec<String>,
        span_id: Option<String>,
        native: Value,
        events: Vec<Event>,
    ) -> Option<Arc<Record>> {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return None;
        }
        Some(self.append_locked(
            &mut s,
            direction.into(),
            kind,
            session_id,
            agent_path,
            span_id,
            native,
            events,
        ))
    }
    #[allow(clippy::too_many_arguments)]
    fn append_locked(
        &self,
        s: &mut State,
        stamp: RecordStamp,
        kind: RecordKind,
        session_id: Option<String>,
        agent_path: Vec<String>,
        span_id: Option<String>,
        native: Value,
        events: Vec<Event>,
    ) -> Arc<Record> {
        let seq = s.records.len() as u64;
        #[cfg(not(target_arch = "wasm32"))]
        let native = {
            let mut native = native;
            if stamp
                .prompt
                .as_ref()
                .is_some_and(|p| p.submission.is_some())
            {
                crate::input::omit_image_bytes(&mut native);
            }
            native
        };
        let session_id = session_id.or_else(|| s.root.clone());
        #[cfg(not(target_arch = "wasm32"))]
        if agent_path.is_empty() && s.root == session_id {
            s.reachability
                .observe(kind, stamp.direction, &native, &events);
        }
        let message_boundaries = s.messages.observe(stamp.direction, session_id.as_deref(), &agent_path, &native, &events);
        let record = Arc::new(Record {
            message_boundaries,
            elided: false,
            elided_input_echoes: vec![],
            upstream: None,
            span_id,
            prompt: stamp.prompt,
            response_to: stamp.response_to.or_else(|| {
                if stamp.direction == Direction::Host && kind == RecordKind::Response {
                    native["requestId"].as_str().map(str::to_owned)
                } else {
                    None
                }
            }),
            opening: stamp.opening,
            origin: stamp.origin,
            carries: stamp.carried.iter().map(ToString::to_string).collect(),
            frame_type: if kind == RecordKind::Frame {
                stamp
                    .frame_type
                    .or_else(|| Record::wire_frame_type(&native).map(str::to_owned))
                    .or_else(|| {
                        (stamp.direction == Direction::Host).then(|| "kar/notification".to_owned())
                    })
            } else {
                None
            },
            stream_id: self.id,
            seq,
            timestamp_ms: stamp.timestamp_ms,
            direction: stamp.direction,
            kind,
            session_id,
            agent_path,
            native,
            events,
        });
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(chat) = &s.chat {
            chat.capture(&record, false);
        }
        if let Some(queries) = &mut s.queries {
            queries.fold(&record);
        }
        s.observers
            .retain(|observer| observer.send(record.clone()).is_ok());
        s.records.push(record.clone());
        s.trim_history();
        record
    }
    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn first_claude_result(&self, id: Uuid) -> bool {
        self.state.lock().unwrap().claude_results.insert(id)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn attach_chat(
        &self,
        chat: crate::chat::Sink,
        retained_bytes: Option<usize>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(Error::Closed("agent exited before attaching chat".into()));
        }
        // Handshake and resume observations may precede binding. Hold the same lock
        // as append so no native notification can fall between replay and live delivery.
        let previous = chat.claude_result_ids();
        for record in &state.records {
            let duplicate = !previous.is_empty()
                && record.agent_path.is_empty()
                && record.session_id == state.root
                && crate::wire::claude_input::result_id(&record.native)
                    .is_some_and(|id| previous.contains(&id));
            if !duplicate {
                chat.capture(record, true);
            }
        }
        state.claude_results.extend(previous);
        state.chat = Some(chat);
        state.retained_bytes = retained_bytes;
        state.trim_history();
        Ok(())
    }
    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        #[cfg(not(target_arch = "wasm32"))]
        {
            s.chat.take();
        }
        s.observers.clear();
    }
    pub fn subscribe(&self, cursor: Option<&Cursor>) -> Result<Subscription> {
        let (tx, live) = mpsc::unbounded_channel();
        let mut s = self.state.lock().unwrap();
        let next = match cursor {
            Some(c) if c.stream_id == self.id && c.after_seq < (s.records.len() as u64) => {
                c.after_seq as usize + 1
            }
            Some(_) => return Err(Error::InvalidCursor),
            None => 0,
        };
        Ok(self.subscribe_from(&mut s, next, tx, live))
    }
    fn subscribe_from(
        &self,
        s: &mut State,
        next: usize,
        tx: mpsc::UnboundedSender<Arc<Record>>,
        live: mpsc::UnboundedReceiver<Arc<Record>>,
    ) -> Subscription {
        let history = s.records[next..].iter().cloned().collect();
        if !s.closed {
            s.observers.push(tx);
        }
        Subscription {
            history,
            live,
            journal: Arc::downgrade(&self.state),
            next,
        }
    }
    pub fn events(&self, cursor: Option<&EventCursor>) -> Result<EventSubscription> {
        let mut s = self.state.lock().unwrap();
        let (seq, index) = match cursor {
            None => (0, 0),
            Some(c) if c.stream_id == self.id => {
                let seq = usize::try_from(c.seq).map_err(|_| Error::InvalidCursor)?;
                if s.records.get(seq).is_none_or(|r| c.index >= r.events.len()) {
                    return Err(Error::InvalidCursor);
                }
                (seq, c.index + 1)
            }
            _ => return Err(Error::InvalidCursor),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(EventSubscription {
            records: self.subscribe_from(&mut s, seq, tx, rx),
            current: None,
            index,
        })
    }
    pub fn bind_root(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        state.root = Some(id.to_owned());
        for rec in &mut state.records {
            if rec.session_id.is_none() {
                Arc::make_mut(rec).session_id = Some(id.to_owned());
            }
        }
        let mut queries = crate::observe::SessionReducer::new(id);
        for rec in &state.records {
            queries.fold(rec);
        }
        state.queries = Some(queries);
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn usage_for(&self, session: &str) -> crate::SessionUsage {
        self.state
            .lock()
            .unwrap()
            .queries
            .as_ref()
            .expect("session bound before publication")
            .usage_for(session)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn has_mcp_startup(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .queries
            .as_ref()
            .is_some_and(|queries| !queries.snapshot().mcp_startup.is_empty())
    }
    pub fn snapshot(&self) -> crate::observe::SessionSnapshot {
        self.state
            .lock()
            .unwrap()
            .queries
            .as_ref()
            .expect("session is bound before it is published")
            .snapshot()
            .clone()
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn has_request(&self, id: &Value, cursor: &Cursor) -> bool {
        self.state
            .lock()
            .unwrap()
            .queries
            .as_ref()
            .is_some_and(|q| {
                q.snapshot()
                    .pending_requests
                    .iter()
                    .any(|p| &p.id == id && &p.cursor == cursor)
            })
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn latest_record(&self) -> Option<Arc<Record>> {
        self.state.lock().unwrap().records.last().cloned()
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn control_receipt(&self, request_id: &str) -> Option<ControlReceipt> {
        ControlReceipt::from_records(&self.state.lock().unwrap().records, request_id)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn active_prompt(&self) -> Option<(String, String)> {
        let state = self.state.lock().unwrap();
        match &state.queries.as_ref()?.snapshot().status {
            crate::observe::AgentStatus::Running {
                input_id: Some(input),
                request_id: Some(request),
                ..
            } => Some((input.clone(), request.clone())),
            _ => None,
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn handled_prompt(&self, request: &str, input: Uuid) -> Option<Arc<Record>> {
        self.state.lock().unwrap().records.iter().rev()
            .take_while(|r| r.control_request_id() != Some(request))
            .find(|r| r.kind == RecordKind::Frame && r.agent_path.is_empty()
                && r.response_to.as_deref() == Some(request)
                && r.events.iter().any(|e| matches!(e, Event::NativeInput { input_id, receipt: InputReceipt::Handled } if *input_id == input)))
            .cloned()
    }
    pub fn records(&self) -> Vec<Arc<Record>> {
        self.state.lock().unwrap().records.clone()
    }
}
pub struct Subscription {
    journal: std::sync::Weak<Mutex<State>>,
    next: usize,
    history: VecDeque<Arc<Record>>,
    live: mpsc::UnboundedReceiver<Arc<Record>>,
}
pub struct EventSubscription {
    records: Subscription,
    current: Option<Arc<Record>>,
    index: usize,
}
impl EventSubscription {
    /// Merge consecutive text in each native message. Set a quiet interval to flush a paused stream.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn coalesce_text(self, max_hold: Option<std::time::Duration>) -> CoalescedEvents {
        CoalescedEvents::new(self, max_hold)
    }
    pub async fn next(&mut self) -> Option<ObservedEvent> {
        loop {
            if self.current.is_none() {
                self.current = Some(self.records.next().await?);
            }
            let r = self.current.as_ref().unwrap();
            if let Some(event) = r.events.get(self.index) {
                let cursor = EventCursor {
                    stream_id: r.stream_id,
                    seq: r.seq,
                    index: self.index,
                };
                self.index += 1;
                return Some(ObservedEvent {
                    cursor,
                    timestamp_ms: r.timestamp_ms,
                    session_id: r.session_id.clone(),
                    agent_path: r.agent_path.clone(),
                    span_id: r.span_id.clone(),
                    elided: r.elided,
                    message: r.message_boundaries.get(&(self.index - 1)).cloned().unwrap_or_default(),
                    event: event.clone(),
                });
            }
            self.index = 0;
            self.current = None;
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
#[cfg(target_arch = "wasm32")]
pub(crate) fn timestamp_ms() -> u64 {
    js_sys::Date::now() as u64
}
impl Subscription {
    // Existing readers reach EOF when the runtime closes. A later control
    // decision can still be read at its exact cursor from the retained journal.
    fn finish_read(&mut self, record: Option<Arc<Record>>) -> Option<Arc<Record>> {
        let r = record.or_else(|| {
            let journal = self.journal.upgrade()?;
            let state = journal.lock().unwrap();
            if !state.closed {
                return None;
            }
            state.records.get(self.next).cloned()
        })?;
        self.next = r.seq as usize + 1;
        Some(r)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn try_next(&mut self) -> Option<Arc<Record>> {
        let record = match self.history.pop_front() {
            Some(record) => Some(record),
            None => match self.live.try_recv() {
                Ok(record) => Some(record),
                Err(mpsc::error::TryRecvError::Empty) => return None,
                Err(mpsc::error::TryRecvError::Disconnected) => None,
            },
        };
        self.finish_read(record)
    }
    pub async fn next(&mut self) -> Option<Arc<Record>> {
        let record = match self.history.pop_front() {
            Some(record) => Some(record),
            None => self.live.recv().await,
        };
        self.finish_read(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn old_codex_mcp_frames_only_update_the_matching_root() {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("root");
        let push = |id: Option<&str>, thread: Option<&str>, path: Vec<String>, status: &str| {
            journal.push(Direction::FromRuntime, RecordKind::Frame, id.map(str::to_owned), path,
                json!({"method":"mcpServer/startupStatus/updated","params":{"threadId":thread,"name":"app","status":status}}), vec![]);
        };
        push(Some("root"), Some("root"), vec![], "starting");
        assert_eq!(
            journal.snapshot().mcp_startup.get("app"),
            Some(&McpStartupStatus::Pending)
        );
        push(Some("root"), Some("root"), vec![], "ready");
        push(Some("child"), Some("child"), vec![], "failed");
        push(None, None, vec![], "cancelled");
        push(Some("root"), Some("root"), vec!["child".into()], "failed");
        assert_eq!(
            journal.snapshot().mcp_startup.get("app"),
            Some(&McpStartupStatus::Ready)
        );
    }

    #[tokio::test]
    async fn event_cursor_resumes_inside_an_elided_frame_across_live_delivery() {
        let journal = Journal::new(Some(1)).unwrap();
        journal.bind_root("root");
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            json!({"seq":1}),
            vec![
                Event::TurnStarted,
                Event::Model {
                    model: "native".into(),
                },
            ],
        );
        journal.host_event(Event::Released);
        let mut all = journal.events(None).unwrap();
        let first = all.next().await.unwrap();
        assert!(first.elided);
        let mut resumed = journal.events(Some(&first.cursor)).unwrap();
        assert_eq!(resumed.next().await, all.next().await);
        let invalid = EventCursor {
            index: 2,
            ..first.cursor.clone()
        };
        assert!(matches!(
            journal.events(Some(&invalid)),
            Err(Error::InvalidCursor)
        ));
        let foreign = EventCursor {
            stream_id: Uuid::new_v4(),
            ..first.cursor
        };
        assert!(matches!(
            journal.events(Some(&foreign)),
            Err(Error::InvalidCursor)
        ));
        journal.host_event(Event::Released);
        journal.close();
        for _ in 0..2 {
            assert_eq!(resumed.next().await, all.next().await);
        }
        assert!(resumed.next().await.is_none());
        assert_eq!(journal.snapshot().model.as_deref(), Some("native"));
    }
    #[test]
    fn legacy_admissions_and_overlapping_root_spans() {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("parent");
        journal.push(
            Direction::ToRuntime,
            RecordKind::Request,
            None,
            vec![],
            json!({"type":"user"}),
            vec![],
        );
        assert!(journal.snapshot().status.is_running());
        let emit = |span: &str, events| {
            journal.push(
                Direction::FromRuntime,
                RecordKind::Frame,
                None,
                vec![],
                json!({"params":{"turnId":span}}),
                events,
            )
        };
        emit("one", vec![Event::TurnStarted]);
        emit("two", vec![Event::TurnStarted]);
        emit(
            "one",
            vec![Event::ToolStarted {
                id: "abc".into(),
                name: "echo".into(),
            }],
        );
        assert!(
            matches!(journal.snapshot().status, crate::observe::AgentStatus::Running {
            phase:crate::observe::Phase::Tool { id, .. }, ..} if id == "abc")
        );
        emit(
            "one",
            vec![Event::TurnEnded {
                outcome: Outcome::Completed,
            }],
        );
        assert!(journal.snapshot().status.is_running());
        emit(
            "two",
            vec![Event::TurnEnded {
                outcome: Outcome::Completed,
            }],
        );
        assert!(!journal.snapshot().status.is_running());
        for (kind, running) in [("queue", true), ("withdrawn", false)] {
            journal.push(
                Direction::Host,
                RecordKind::Response,
                None,
                vec![],
                json!({"type":kind,"inputId":"id"}),
                vec![],
            );
            assert_eq!(journal.snapshot().status.is_running(), running);
        }
    }
    #[tokio::test]
    async fn bounded_history_preserves_live_frames_and_cursor() {
        let journal = Journal::new(Some(1)).unwrap();
        let mut live = journal.subscribe(None).unwrap();
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            json!({"text":"full"}),
            vec![Event::TextDelta {
                text: "full".into(),
            }],
        );
        assert!(!journal.records()[0].elided);
        journal.host_event(Event::Released);
        journal.close();
        let raw = live.next().await.unwrap();
        assert!(!raw.elided);
        assert_eq!(raw.native["text"], "full");
        assert!(!live.next().await.unwrap().elided);
        assert!(live.next().await.is_none());
        let retained = journal.records();
        assert!(retained[0].elided);
        assert!(retained[0].native.is_null());
        assert_eq!(raw.events, retained[0].events);
        assert_eq!(raw.cursor(), retained[0].cursor());
    }
}
