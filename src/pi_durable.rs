//! Pure-Rust durable session engine (`kar::pi_durable`).
//!
//! [`PiDurable`] provides an in-process, restart-resumable agent session engine
//! backed by pluggable [`DurableStorage`] ([`MemoryStorage`] or atomic JSONL [`FileStorage`]),
//! executing model turns against OpenAI-compatible HTTP endpoints, Anthropic-compatible
//! HTTP endpoints, or deterministic scripted responders, with cursor-resumable
//! [`crate::ObservedEvent`] streaming and [`crate::Event::Released`] lease detachment on close.

use crate::{
    CredentialProblem, Direction, Error, Event, EventCursor, EventSubscription, Execution,
    FailureClass, NativeCause, OpenError, Outcome, PlacementRequest, Protocol, Record, RecordKind,
    ReleaseReason, Result, ResumeCwdReason, Session, SessionCapabilityDeclaration, SessionOptions,
    SessionRelease, SteerStatus, TokenReading, TokenTotals, TurnFailure, TurnReport,
    input::Input,
    stream::{Journal, RecordStamp, timestamp_ms},
    transport::{Command, redact},
};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    future::Future,
    io::{BufRead, BufReader, Write},
    ops::Deref,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

/// Persisted message entry inside a [`DurableConversationState`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableMessage {
    pub role: String,
    pub content: String,
    pub timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
}

/// Durable queued user submission persisted across controller detach/reopen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableQueuedInput {
    pub input_id: String,
    pub text: String,
    pub enqueued_ms: u64,
}

/// Complete serializable snapshot of one [`PiDurable`] conversation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurableConversationState {
    pub id: String,
    pub cwd: PathBuf,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub messages: Vec<DurableMessage>,
    #[serde(default)]
    pub queued_inputs: VecDeque<DurableQueuedInput>,
    #[serde(default)]
    pub totals: TokenTotals,
    #[serde(default)]
    pub records: Vec<Record>,
}

/// Pluggable persistence backend for [`PiDurable`] conversations.
pub trait DurableStorage: Send + Sync + 'static {
    fn load(&self, session_id: &str) -> Result<Option<DurableConversationState>>;
    fn save(&self, state: &DurableConversationState) -> Result<()>;
    fn list(&self) -> Result<Vec<String>>;
    fn delete(&self, session_id: &str) -> Result<bool>;
}

/// Thread-safe in-memory [`DurableStorage`] implementation.
#[derive(Clone, Default)]
pub struct MemoryStorage {
    states: Arc<Mutex<BTreeMap<String, DurableConversationState>>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DurableStorage for MemoryStorage {
    fn load(&self, session_id: &str) -> Result<Option<DurableConversationState>> {
        Ok(self.states.lock().unwrap().get(session_id).cloned())
    }

    fn save(&self, state: &DurableConversationState) -> Result<()> {
        self.states
            .lock()
            .unwrap()
            .insert(state.id.clone(), state.clone());
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>> {
        Ok(self.states.lock().unwrap().keys().cloned().collect())
    }

    fn delete(&self, session_id: &str) -> Result<bool> {
        Ok(self.states.lock().unwrap().remove(session_id).is_some())
    }
}

/// Crash-safe atomic JSONL [`DurableStorage`] backed by a `0700` directory and `0600` files.
///
/// Each conversation file stores a header snapshot line followed by one JSONL line per
/// recorded frame, rewritten atomically via temporary file + `sync_all` + `persist`.
#[derive(Clone, Debug)]
pub struct FileStorage {
    root: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

#[derive(Serialize, Deserialize)]
struct FileHeaderLine {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    id: String,
    cwd: PathBuf,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    append_system_prompt: Option<String>,
    created_ms: u64,
    updated_ms: u64,
    messages: Vec<DurableMessage>,
    queued_inputs: VecDeque<DurableQueuedInput>,
    totals: TokenTotals,
}

#[derive(Serialize, Deserialize)]
struct FileRecordLine {
    #[serde(rename = "type")]
    kind: String,
    record: Record,
}

impl FileStorage {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            root,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn validate_id(session_id: &str) -> Result<()> {
        if session_id.is_empty()
            || session_id.len() > 128
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Refused(format!(
                "invalid durable session identifier {session_id:?}"
            )));
        }
        Ok(())
    }

    fn path_for(&self, session_id: &str) -> Result<PathBuf> {
        Self::validate_id(session_id)?;
        Ok(self.root.join(format!("{session_id}.jsonl")))
    }
}

impl DurableStorage for FileStorage {
    fn load(&self, session_id: &str) -> Result<Option<DurableConversationState>> {
        let path = self.path_for(session_id)?;
        let _guard = self.write_lock.lock().unwrap();
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut lines = BufReader::new(file).lines();
        let Some(first) = lines.next() else {
            return Ok(None);
        };
        let header: FileHeaderLine = serde_json::from_str(&first?)?;
        if header.kind != "pi-durable/state" || header.version != 1 {
            return Err(Error::Protocol(
                "unsupported pi-durable JSONL state header".into(),
            ));
        }
        let mut records = Vec::new();
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<FileRecordLine>(&line)
                && entry.kind == "record"
            {
                records.push(entry.record);
            }
        }
        Ok(Some(DurableConversationState {
            id: header.id,
            cwd: header.cwd,
            model: header.model,
            append_system_prompt: header.append_system_prompt,
            created_ms: header.created_ms,
            updated_ms: header.updated_ms,
            messages: header.messages,
            queued_inputs: header.queued_inputs,
            totals: header.totals,
            records,
        }))
    }

    fn save(&self, state: &DurableConversationState) -> Result<()> {
        let target = self.path_for(&state.id)?;
        let _guard = self.write_lock.lock().unwrap();
        let mut builder = tempfile::Builder::new();
        builder.prefix(".pi-durable-").suffix(".tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o600));
        }
        let mut tmp = builder.tempfile_in(&self.root)?;
        let header = FileHeaderLine {
            kind: "pi-durable/state".into(),
            version: 1,
            id: state.id.clone(),
            cwd: state.cwd.clone(),
            model: state.model.clone(),
            append_system_prompt: state.append_system_prompt.clone(),
            created_ms: state.created_ms,
            updated_ms: state.updated_ms,
            messages: state.messages.clone(),
            queued_inputs: state.queued_inputs.clone(),
            totals: state.totals,
        };
        serde_json::to_writer(tmp.as_file_mut(), &header)?;
        tmp.as_file_mut().write_all(b"\n")?;
        for rec in &state.records {
            let row = FileRecordLine {
                kind: "record".into(),
                record: rec.clone(),
            };
            serde_json::to_writer(tmp.as_file_mut(), &row)?;
            tmp.as_file_mut().write_all(b"\n")?;
        }
        tmp.as_file_mut().flush()?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(&target).map_err(|e| Error::Io(e.error))?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>> {
        let _guard = self.write_lock.lock().unwrap();
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !stem.starts_with('.')
            {
                out.push(stem.to_owned());
            }
        }
        out.sort();
        Ok(out)
    }

    fn delete(&self, session_id: &str) -> Result<bool> {
        let path = self.path_for(session_id)?;
        let _guard = self.write_lock.lock().unwrap();
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// Output produced by a deterministic [`DurableProvider::Scripted`] responder.
#[derive(Clone, Debug)]
pub struct DurableResponderOutput {
    pub chunks: Vec<String>,
    pub reasoning: Option<String>,
    pub usage: Option<TokenTotals>,
    pub failure: Option<TurnFailure>,
}

impl DurableResponderOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            chunks: vec![text.into()],
            reasoning: None,
            usage: None,
            failure: None,
        }
    }

    pub fn chunks<I, S>(chunks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            chunks: chunks.into_iter().map(Into::into).collect(),
            reasoning: None,
            usage: None,
            failure: None,
        }
    }

    pub fn failed(failure: TurnFailure) -> Self {
        Self {
            chunks: Vec::new(),
            reasoning: None,
            usage: None,
            failure: Some(failure),
        }
    }
}

type ScriptedResponder =
    dyn Fn(&[DurableMessage], &str) -> Result<DurableResponderOutput> + Send + Sync + 'static;

/// Model execution backend for [`PiDurable`].
#[derive(Clone)]
pub enum DurableProvider {
    /// OpenAI Chat Completions compatible endpoint (`POST /v1/chat/completions` or custom URL).
    OpenAiCompatible {
        base_url: String,
        api_key: Option<String>,
        model: String,
        headers: BTreeMap<String, String>,
        stream: bool,
    },
    /// Anthropic Messages API compatible endpoint (`POST /v1/messages` or custom URL).
    AnthropicCompatible {
        base_url: String,
        api_key: Option<String>,
        model: String,
        max_tokens: u32,
        headers: BTreeMap<String, String>,
        stream: bool,
    },
    /// Deterministic in-process responder closure for tests and offline workflows.
    Scripted {
        model: String,
        responder: Arc<ScriptedResponder>,
    },
}

impl DurableProvider {
    pub fn openai(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::OpenAiCompatible {
            base_url: base_url.into(),
            api_key,
            model: model.into(),
            headers: BTreeMap::new(),
            stream: true,
        }
    }

    pub fn anthropic(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::AnthropicCompatible {
            base_url: base_url.into(),
            api_key,
            model: model.into(),
            max_tokens: 4096,
            headers: BTreeMap::new(),
            stream: true,
        }
    }

    pub fn scripted<F>(model: impl Into<String>, responder: F) -> Self
    where
        F: Fn(&[DurableMessage], &str) -> Result<DurableResponderOutput> + Send + Sync + 'static,
    {
        Self::Scripted {
            model: model.into(),
            responder: Arc::new(responder),
        }
    }

    pub fn default_model(&self) -> &str {
        match self {
            Self::OpenAiCompatible { model, .. }
            | Self::AnthropicCompatible { model, .. }
            | Self::Scripted { model, .. } => model,
        }
    }
}

/// Pure-Rust durable agent engine managing conversations over [`DurableStorage`].
#[derive(Clone)]
pub struct PiDurable {
    storage: Arc<dyn DurableStorage>,
    provider: DurableProvider,
    placement: PlacementRequest,
}

impl PiDurable {
    pub fn new(storage: impl DurableStorage, provider: DurableProvider) -> Self {
        Self {
            storage: Arc::new(storage),
            provider,
            placement: PlacementRequest::Auto,
        }
    }

    pub fn with_shared_storage(
        storage: Arc<dyn DurableStorage>,
        provider: DurableProvider,
    ) -> Self {
        Self {
            storage,
            provider,
            placement: PlacementRequest::Auto,
        }
    }

    pub fn with_placement(mut self, placement: PlacementRequest) -> Self {
        self.placement = placement;
        self
    }

    pub fn storage(&self) -> &Arc<dyn DurableStorage> {
        &self.storage
    }

    pub fn capability_declaration(&self) -> SessionCapabilityDeclaration {
        SessionCapabilityDeclaration::pi_durable()
    }

    fn validate_options(&self, options: &SessionOptions) -> Result<()> {
        let decl = SessionCapabilityDeclaration::pi_durable();
        let reject = |key: &str| -> Result<()> {
            let reason = decl
                .refused_session_options
                .get(key)
                .copied()
                .unwrap_or("unsupported pi-durable session option");
            Err(Error::Unsupported(format!("{key}: {reason}")))
        };
        if !options.settings.additional_directories.is_empty() {
            return reject("additional_directories");
        }
        if !options.settings.disallowed_tools.is_empty() {
            return reject("disallowed_tools");
        }
        if options.settings.end_turn_processes_on_abort {
            return reject("end_turn_processes_on_abort");
        }
        if !options.env.is_empty() || !options.unset_env.is_empty() {
            return reject("env");
        }
        if !options.settings.exclude_user_configuration.is_empty() {
            return reject("exclude_user_configuration");
        }
        if options.settings.goals {
            return reject("goals");
        }
        if !options.launch_args.is_empty() {
            return reject("launch_args");
        }
        if !options.mcp_servers.is_empty() {
            return reject("mcp_servers");
        }
        if options.settings.model_requests.is_some() {
            return reject("model_requests");
        }
        if options.settings.name.is_some() {
            return reject("name");
        }
        if options.settings.native_memory_default.is_some() {
            return reject("native_memory_default");
        }
        if options.settings.output_schema.is_some() {
            return reject("output_schema");
        }
        if options.settings.pi_project_trusted.is_some()
            || options.settings.pi_allow_project_package_install
        {
            return reject("pi_project_trusted");
        }
        if options.settings.service_tier.is_some() {
            return reject("service_tier");
        }
        if options.settings.session_log_upload.is_some() {
            return reject("session_log_upload");
        }
        if options.settings.system_prompt.is_some() {
            return reject("system_prompt");
        }
        Ok(())
    }

    /// Create a new durable session or reopen/resume an existing durable conversation.
    pub async fn open(&self, options: SessionOptions) -> Result<PiDurableSession> {
        self.validate_options(&options)?;
        let cwd = fs::canonicalize(&options.cwd)?;
        if !cwd.is_dir() {
            return Err(Error::Protocol("cwd must be a directory".into()));
        }

        let journal = Journal::for_open(
            options.retained_bytes,
            self.placement,
            Some(Execution::InProcess),
        )?;

        let mut state = if let Some(resume_id) = &options.resume {
            let Some(saved) = self.storage.load(resume_id)? else {
                return Err(OpenError::SessionNotFound {
                    session_id: resume_id.clone(),
                    message: format!("pi-durable session {resume_id:?} was not found"),
                    cause: NativeCause {
                        method: "pi-durable/open".into(),
                        native: json!({ "sessionId": resume_id }),
                    },
                }
                .into());
            };
            let saved_cwd_str = saved.cwd.to_string_lossy();
            if !crate::opening::same_directory(&saved_cwd_str, &cwd) {
                return Err(OpenError::cwd(
                    resume_id,
                    &cwd,
                    ResumeCwdReason::OtherCwd {
                        saved_cwd: saved_cwd_str.into_owned(),
                    },
                )
                .into());
            }
            let mut updated = saved;
            if let Some(model) = &options.model {
                updated.model = model.clone();
            }
            if let Some(append) = &options.settings.append_system_prompt {
                updated.append_system_prompt = Some(append.clone());
            }
            updated
        } else {
            let id = format!("durable-{}", Uuid::new_v4().simple());
            let now = timestamp_ms();
            DurableConversationState {
                id,
                cwd: cwd.clone(),
                model: options
                    .model
                    .clone()
                    .unwrap_or_else(|| self.provider.default_model().to_owned()),
                append_system_prompt: options.settings.append_system_prompt.clone(),
                created_ms: now,
                updated_ms: now,
                messages: Vec::new(),
                queued_inputs: VecDeque::new(),
                totals: TokenTotals::default(),
                records: Vec::new(),
            }
        };

        let session_id = state.id.clone();
        journal.bind_root(&session_id);

        // Replay persisted history frames into the new controller's journal so cursors and
        // snapshots reflect full durable history upon reopen, skipping prior opening/release pairs.
        for rec in &state.records {
            if rec.opening.is_some() || rec.events.contains(&Event::Released) {
                continue;
            }
            if rec.kind == RecordKind::Request
                && rec.native.get("type").and_then(Value::as_str) == Some("dispose")
            {
                continue;
            }
            journal.push(
                rec.direction,
                rec.kind,
                rec.session_id.clone(),
                rec.agent_path.clone(),
                rec.native.clone(),
                rec.events.clone(),
            );
        }

        journal.push(
            RecordStamp::frame(Direction::FromRuntime, "pi-durable/model"),
            RecordKind::Frame,
            Some(session_id.clone()),
            vec![],
            json!({ "type": "pi-durable/model", "model": state.model }),
            vec![Event::Model {
                model: state.model.clone(),
            }],
        );
        if state.totals.input > 0 || state.totals.output > 0 {
            journal.push(
                RecordStamp::frame(Direction::FromRuntime, "pi-durable/usage"),
                RecordKind::Frame,
                Some(session_id.clone()),
                vec![],
                json!({ "type": "pi-durable/usage", "totals": state.totals }),
                vec![Event::TokenUsage {
                    reading: TokenReading {
                        cache_write_by_ttl: None,
                        tokens: Some(state.totals),
                        total: Some(state.totals),
                    },
                }],
            );
        }

        state.updated_ms = timestamp_ms();
        state.records = journal
            .records()
            .into_iter()
            .map(|r| (*r).clone())
            .collect();
        self.storage.save(&state)?;

        let (commands_tx, commands_rx) = mpsc::channel(64);
        let actor = DurableActor::new(
            Arc::clone(&self.storage),
            self.provider.clone(),
            state,
            journal.clone(),
            crate::options::session_secrets(&options),
        )?;
        tokio::spawn(actor.run(commands_rx));

        let mut opts_for_session = options;
        opts_for_session.cwd = cwd;
        let info = json!({
            "sessionId": session_id,
            "durable": true,
        });
        let mut session = Session::remote(
            Protocol::Pi,
            opts_for_session,
            commands_tx,
            journal,
            session_id,
            info,
        );
        session.image_input = false;
        Ok(PiDurableSession {
            session,
            storage: Arc::clone(&self.storage),
        })
    }
}

/// Active controller handle over a [`PiDurable`] conversation.
///
/// Dereferences to [`crate::Session`]. Calling [`PiDurableSession::close`] (or
/// [`Session::close`]) emits [`Event::Released`] (`ReleaseReason::Dispose`) while
/// preserving durable conversation state in [`DurableStorage`].
#[derive(Clone)]
pub struct PiDurableSession {
    session: Session,
    storage: Arc<dyn DurableStorage>,
}

impl PiDurableSession {
    pub fn as_session(&self) -> &Session {
        &self.session
    }

    pub fn into_session(self) -> Session {
        self.session
    }

    /// Subscribe to the cursor-resumable [`crate::ObservedEvent`] stream.
    pub fn events(&self, cursor: Option<&EventCursor>) -> Result<EventSubscription> {
        self.session.events(cursor)
    }

    /// Read the latest persisted [`DurableConversationState`] from storage.
    pub fn durable_state(&self) -> Result<Option<DurableConversationState>> {
        self.storage.load(self.session.id())
    }

    /// Release this controller lease (`Event::Released` with `ReleaseReason::Dispose`).
    pub async fn close(&self) -> Result<()> {
        self.session.close().await
    }
}

impl Deref for PiDurableSession {
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

struct TurnExecutionOutput {
    reply_text: String,
    usage_delta: TokenTotals,
    failure: Option<TurnFailure>,
}

struct ActiveDurableTurn {
    input_id: Uuid,
    request_id: Uuid,
    input_text: String,
    steered_texts: Vec<String>,
    cancel_tx: watch::Sender<bool>,
    done: Option<oneshot::Sender<Result<TurnReport>>>,
    future: Pin<Box<dyn Future<Output = TurnExecutionOutput> + Send>>,
}

struct DurableActor {
    storage: Arc<dyn DurableStorage>,
    provider: DurableProvider,
    http: Client,
    state: DurableConversationState,
    journal: Journal,
    secrets: Vec<String>,
    waiters: BTreeMap<Uuid, oneshot::Sender<Result<TurnReport>>>,
    active: Option<ActiveDurableTurn>,
}

impl DurableActor {
    fn new(
        storage: Arc<dyn DurableStorage>,
        provider: DurableProvider,
        state: DurableConversationState,
        journal: Journal,
        secrets: Vec<String>,
    ) -> Result<Self> {
        let http = Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| Error::Protocol(format!("pi-durable HTTP client init failed: {e}")))?;
        let mut actor = Self {
            storage,
            provider,
            http,
            state,
            journal,
            secrets,
            waiters: BTreeMap::new(),
            active: None,
        };
        actor.drain_durable_queue_if_idle();
        Ok(actor)
    }

    fn persist_state(&mut self) {
        self.state.updated_ms = timestamp_ms();
        self.state.records = self
            .journal
            .records()
            .into_iter()
            .map(|r| (*r).clone())
            .collect();
        let _ = self.storage.save(&self.state);
    }

    fn drain_durable_queue_if_idle(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(queued) = self.state.queued_inputs.pop_front() else {
            return;
        };
        let input_id = Uuid::parse_str(&queued.input_id).unwrap_or_else(|_| Uuid::new_v4());
        let done = self.waiters.remove(&input_id);
        let input = Input::new(queued.text, Vec::new()).dispatch_queued(input_id);
        self.start_turn(input_id, input, done);
    }

    fn start_turn(
        &mut self,
        input_id: Uuid,
        input: Input,
        done: Option<oneshot::Sender<Result<TurnReport>>>,
    ) {
        let mut body = json!({
            "type": "prompt",
            "id": input.request_id,
            "inputId": input_id,
            "message": input.text,
        });
        redact(&mut body, &self.secrets);
        let mut user_text = input.text.clone();
        crate::transport::redact_text(&mut user_text, &self.secrets);

        let stamp = RecordStamp::from(Direction::ToRuntime)
            .with_input(&input)
            .with_prompt_request(id_str(input_id), false, input.request_id, None);
        self.journal.push(
            stamp,
            RecordKind::Request,
            Some(self.state.id.clone()),
            vec![],
            body,
            vec![
                Event::PromptDispatched,
                Event::UserMessage {
                    text: user_text.clone(),
                },
            ],
        );
        self.journal.control_decision(input.request_id, None);

        self.state.messages.push(DurableMessage {
            role: "user".into(),
            content: input.text.clone(),
            timestamp_ms: timestamp_ms(),
            input_id: Some(input_id.to_string()),
        });

        self.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "pi-durable/turn_started"),
            RecordKind::Frame,
            Some(self.state.id.clone()),
            vec![],
            json!({ "type": "pi-durable/turn_started", "inputId": input_id }),
            vec![Event::TurnStarted],
        );
        self.persist_state();

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let history = self.state.messages.clone();
        let system_prompt = self.state.append_system_prompt.clone();
        let model = self.state.model.clone();
        let provider = self.provider.clone();
        let http = self.http.clone();
        let journal = self.journal.clone();
        let session_id = self.state.id.clone();

        let future = Box::pin(run_provider_turn(
            provider,
            http,
            model,
            system_prompt,
            history,
            journal,
            session_id,
            cancel_rx,
        ));

        self.active = Some(ActiveDurableTurn {
            input_id,
            request_id: input.request_id,
            input_text: input.text,
            steered_texts: Vec::new(),
            cancel_tx,
            done,
            future,
        });
    }

    fn finish_turn(
        &mut self,
        forced_outcome: Option<Outcome>,
        output: Option<TurnExecutionOutput>,
    ) {
        let Some(active) = self.active.take() else {
            return;
        };
        let _ = active.cancel_tx.send(true);

        let (reply_text, delta_tokens, failure, outcome) = match (forced_outcome, output) {
            (Some(Outcome::Aborted), _) => (
                String::new(),
                TokenTotals {
                    input: estimate_tokens(&active.input_text),
                    output: 0,
                    ..TokenTotals::default()
                },
                None,
                Outcome::Aborted,
            ),
            (_, Some(out)) => {
                let outcome = if out.failure.is_some() {
                    Outcome::Failed
                } else {
                    Outcome::Completed
                };
                (out.reply_text, out.usage_delta, out.failure, outcome)
            }
            (Some(other), None) => (String::new(), TokenTotals::default(), None, other),
            (None, None) => (
                String::new(),
                TokenTotals::default(),
                None,
                Outcome::Completed,
            ),
        };

        self.state.totals = self.state.totals.plus(delta_tokens);
        if outcome == Outcome::Completed && !reply_text.is_empty() {
            self.state.messages.push(DurableMessage {
                role: "assistant".into(),
                content: reply_text.clone(),
                timestamp_ms: timestamp_ms(),
                input_id: None,
            });
        }
        for steered in active.steered_texts {
            self.state.messages.push(DurableMessage {
                role: "user".into(),
                content: steered,
                timestamp_ms: timestamp_ms(),
                input_id: None,
            });
        }

        let mut end_events = vec![Event::TokenUsage {
            reading: TokenReading {
                cache_write_by_ttl: None,
                tokens: Some(self.state.totals),
                total: Some(self.state.totals),
            },
        }];
        if let Some(f) = &failure {
            end_events.push(Event::TurnFailure { failure: f.clone() });
        }
        end_events.push(Event::TurnEnded {
            outcome: outcome.clone(),
        });

        self.journal.push(
            RecordStamp::frame(Direction::FromRuntime, "pi-durable/turn_ended"),
            RecordKind::Frame,
            Some(self.state.id.clone()),
            vec![],
            json!({
                "type": "pi-durable/turn_ended",
                "outcome": outcome,
            }),
            end_events,
        );
        self.persist_state();

        if let Some(done) = active.done {
            let _ = done.send(Ok(TurnReport {
                native_input_receipt: None,
                outcome,
                text: reply_text,
                structured_output: None,
                failure,
            }));
        }

        self.drain_durable_queue_if_idle();
    }

    fn reject_images(&self, id: Uuid, input: &Input, control: &str) -> Error {
        let req = input.request_id;
        let reason = "pi-durable sessions do not accept local image attachments";
        let _ = self.journal.refused_input(
            RecordStamp::from(Direction::ToRuntime).with_input(input),
            json!({ "type": control, "id": req, "inputId": id }),
            req,
            "unsupported",
            reason,
        );
        Error::Unsupported(reason.into())
    }

    fn handle_command(&mut self, cmd: Command) -> Option<oneshot::Sender<Result<()>>> {
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
                if !input.images.is_empty() {
                    let _ = ack.send(Err(self.reject_images(id, &input, "prompt")));
                    return None;
                }
                if self.active.is_some() {
                    let err = self.journal.busy_prompt(id, &input, false, &self.secrets);
                    let _ = ack.send(Err(err));
                } else {
                    self.start_turn(id, input, Some(done));
                    let _ = ack.send(Ok(()));
                }
            }
            Command::Queue(id, input, ack, done) => {
                if !input.images.is_empty() {
                    let _ = ack.send(Err(self.reject_images(id, &input, "queue")));
                    return None;
                }
                let req = input.request_id;
                self.journal.queue_request(id, &input, &self.secrets);
                self.journal.queue_accepted(req, id, false, None);
                if self.active.is_none() {
                    let carried = input.dispatch_queued(id);
                    self.start_turn(id, carried, Some(done));
                } else {
                    self.state.queued_inputs.push_back(DurableQueuedInput {
                        input_id: id.to_string(),
                        text: input.text,
                        enqueued_ms: timestamp_ms(),
                    });
                    self.waiters.insert(id, done);
                    self.persist_state();
                }
                let _ = ack.send(Ok(()));
            }
            Command::Withdraw(id, ack) => {
                let req = self.journal.withdraw_request(id);
                let id_s = id.to_string();
                let mut removed = false;
                while let Some(pos) = self
                    .state
                    .queued_inputs
                    .iter()
                    .position(|q| q.input_id == id_s)
                {
                    self.state.queued_inputs.remove(pos);
                    removed = true;
                }
                if let Some(done) = self.waiters.remove(&id) {
                    let _ = done.send(Err(Error::Refused("input withdrawn".into())));
                    removed = true;
                }
                if !removed {
                    self.journal
                        .control_decision(req, Some(("not_queued", "input is not queued")));
                    let _ = ack.send(Err(Error::Refused("input is not queued".into())));
                } else {
                    self.journal.control_outcome(
                        req,
                        json!({ "type": "accepted" }),
                        Event::InputRemoved { input_id: id_s },
                    );
                    self.persist_state();
                    let _ = ack.send(Ok(()));
                }
            }
            Command::Steer(id, input, target, ack) => {
                if !input.images.is_empty() {
                    let _ = ack.send(Err(self.reject_images(id, &input, "steer")));
                    return None;
                }
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
                if let Some(active) = &mut self.active {
                    let mut clean = input.text.clone();
                    crate::transport::redact_text(&mut clean, &self.secrets);
                    active.steered_texts.push(input.text);
                    self.journal.push(
                        RecordStamp::frame(Direction::FromRuntime, "pi-durable/steer"),
                        RecordKind::Frame,
                        Some(self.state.id.clone()),
                        vec![],
                        json!({ "type": "pi-durable/steer", "inputId": id, "text": clean }),
                        vec![Event::SteeringInput {
                            input_id: id,
                            chat_turn: Some(active.input_id),
                            text: clean,
                            status: SteerStatus::Accepted,
                        }],
                    );
                    let ok = Ok(Value::Null);
                    self.journal.control_result(request, &ok, &self.secrets);
                    self.persist_state();
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
                    self.finish_turn(Some(Outcome::Aborted), None);
                    let _ = ack.send(ok);
                } else {
                    let err: Result<Value> = Err(Error::Refused("no active turn".into()));
                    self.journal.control_result(abort_id, &err, &self.secrets);
                    let _ = ack.send(err);
                }
            }
            Command::Compact(req_id, _, ack) => {
                let err = Err(Error::Unsupported(
                    "the harness exposes no verified manual compaction control".into(),
                ));
                self.journal.control_result(req_id, &err, &self.secrets);
                let _ = ack.send(err);
            }
            Command::StopTask(_, _, ack) => {
                let _ = ack.send(Err(Error::Unsupported(
                    "pi-durable exposes no background task stop".into(),
                )));
            }
            Command::Respond(_, _, ack) => {
                let _ = ack.send(Err(Error::Refused(
                    "unknown pi-durable interaction request".into(),
                )));
            }
            Command::Rpc(method, _, _, ack) => {
                let res = match method.as_str() {
                    "get_available_models" => Ok(json!({
                        "models": [{ "id": self.state.model, "name": self.state.model }]
                    })),
                    _ => Err(Error::Unsupported(format!(
                        "unsupported pi-durable RPC {method:?}"
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
        let close_ack = loop {
            let active_fut = futures_util::future::OptionFuture::from(
                self.active.as_mut().map(|a| a.future.as_mut()),
            );
            tokio::select! {
                biased;
                cmd = rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            if let Some(ack) = self.handle_command(cmd) {
                                break Some(ack);
                            }
                        }
                        None => break None,
                    }
                }
                Some(out) = active_fut => {
                    self.finish_turn(None, Some(out));
                }
            }
        };

        let release = SessionRelease::new(ReleaseReason::Dispose);
        if let Some(active) = self.active.take() {
            let _ = active.cancel_tx.send(true);
            if let Some(done) = active.done {
                let _ = done.send(Err(Error::Closed("session released".into())));
            }
        }
        for (_, done) in std::mem::take(&mut self.waiters) {
            let _ = done.send(Err(Error::Closed("session released".into())));
        }

        if close_ack.is_some() {
            let dispose_id = self.journal.request_dispose();
            self.journal.control_outcome(
                dispose_id,
                json!({
                    "type": "released",
                    "release": release,
                }),
                Event::Released,
            );
        } else {
            self.journal.push(
                Direction::Host,
                RecordKind::Response,
                Some(self.state.id.clone()),
                vec![],
                json!({
                    "type": "released",
                    "release": release,
                }),
                vec![Event::Released],
            );
        }
        self.persist_state();
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
}

fn id_str(id: Uuid) -> String {
    id.to_string()
}

fn classify_http_failure(status: u16, body_text: &str) -> TurnFailure {
    let lower = body_text.to_ascii_lowercase();
    let (class, credential) = match status {
        401 | 403 => (FailureClass::Auth, Some(CredentialProblem::Rejected)),
        402 => (FailureClass::Billing, None),
        404 => (FailureClass::ModelUnavailable, None),
        413 => (FailureClass::InputTooLarge, None),
        429 => {
            if lower.contains("quota") || lower.contains("insufficient_quota") {
                (FailureClass::Quota, None)
            } else {
                (FailureClass::RateLimited, None)
            }
        }
        400 if lower.contains("context_length") || lower.contains("too many tokens") => {
            (FailureClass::InputTooLarge, None)
        }
        400 | 422 => (FailureClass::InvalidRequest, None),
        503 | 529 => (FailureClass::Overloaded, None),
        500..=599 => (FailureClass::Provider, None),
        _ => (FailureClass::Unknown, None),
    };
    let reason = serde_json::from_str::<Value>(body_text)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if body_text.trim().is_empty() {
                format!("HTTP {status}")
            } else {
                body_text.trim().chars().take(300).collect()
            }
        });
    TurnFailure {
        class,
        reason,
        status: Some(status),
        credential,
        resets_at_ms: None,
        rate_limit_type: None,
    }
}

fn resolve_openai_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_owned()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/chat/completions")
    } else {
        format!("{trimmed}/v1/chat/completions")
    }
}

fn resolve_anthropic_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/messages") {
        trimmed.to_owned()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/messages")
    } else {
        format!("{trimmed}/v1/messages")
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_provider_turn(
    provider: DurableProvider,
    http: Client,
    model: String,
    append_system_prompt: Option<String>,
    history: Vec<DurableMessage>,
    journal: Journal,
    session_id: String,
    mut cancel_rx: watch::Receiver<bool>,
) -> TurnExecutionOutput {
    tokio::task::yield_now().await;
    let prompt_tokens_est: i64 = history
        .iter()
        .map(|m| estimate_tokens(&m.content))
        .sum::<i64>()
        .saturating_add(append_system_prompt.as_deref().map_or(0, estimate_tokens));

    let msg_id = format!("durable-msg-{}", Uuid::new_v4().simple());
    let emit_delta = |chunk: &str| {
        if chunk.is_empty() {
            return;
        }
        journal.push(
            RecordStamp::frame(Direction::FromRuntime, "pi-durable/text_delta"),
            RecordKind::Frame,
            Some(session_id.clone()),
            vec![],
            json!({
                "type": "pi-durable/text_delta",
                "message_id": msg_id,
                "phase": "final_answer",
                "text": chunk,
            }),
            vec![Event::TextDelta {
                text: chunk.to_owned(),
            }],
        );
    };
    let emit_reasoning = |chunk: &str| {
        if chunk.is_empty() {
            return;
        }
        journal.push(
            RecordStamp::frame(Direction::FromRuntime, "pi-durable/reasoning_delta"),
            RecordKind::Frame,
            Some(session_id.clone()),
            vec![],
            json!({
                "type": "pi-durable/reasoning_delta",
                "message_id": format!("{msg_id}-think"),
                "phase": "thinking",
                "text": chunk,
            }),
            vec![Event::ReasoningDelta {
                text: chunk.to_owned(),
            }],
        );
    };
    let emit_message = |full: &str| {
        if full.is_empty() {
            return;
        }
        journal.push(
            RecordStamp::frame(Direction::FromRuntime, "pi-durable/message"),
            RecordKind::Frame,
            Some(session_id.clone()),
            vec![],
            json!({
                "type": "pi-durable/message",
                "message_id": msg_id,
                "phase": "final_answer",
                "text": full,
            }),
            vec![Event::Message {
                text: full.to_owned(),
            }],
        );
    };

    let run = async {
        match provider {
            DurableProvider::Scripted { responder, .. } => match responder(&history, &model) {
                Ok(out) => {
                    if let Some(reason) = &out.reasoning {
                        emit_reasoning(reason);
                    }
                    let mut full = String::new();
                    for chunk in &out.chunks {
                        emit_delta(chunk);
                        full.push_str(chunk);
                    }
                    if out.failure.is_none() {
                        emit_message(&full);
                    }
                    let usage_delta = out.usage.unwrap_or_else(|| TokenTotals {
                        input: prompt_tokens_est,
                        output: estimate_tokens(&full),
                        reasoning: out.reasoning.as_deref().map(estimate_tokens),
                        ..TokenTotals::default()
                    });
                    TurnExecutionOutput {
                        reply_text: full,
                        usage_delta,
                        failure: out.failure,
                    }
                }
                Err(err) => TurnExecutionOutput {
                    reply_text: String::new(),
                    usage_delta: TokenTotals {
                        input: prompt_tokens_est,
                        ..TokenTotals::default()
                    },
                    failure: Some(TurnFailure {
                        class: FailureClass::Unknown,
                        reason: err.to_string(),
                        status: None,
                        credential: None,
                        resets_at_ms: None,
                        rate_limit_type: None,
                    }),
                },
            },
            DurableProvider::OpenAiCompatible {
                base_url,
                api_key,
                headers,
                stream,
                ..
            } => {
                let url = resolve_openai_url(&base_url);
                let mut messages_json = Vec::new();
                if let Some(sys) = &append_system_prompt {
                    messages_json.push(json!({ "role": "system", "content": sys }));
                }
                for m in &history {
                    messages_json.push(json!({ "role": m.role, "content": m.content }));
                }
                let mut body = json!({
                    "model": model,
                    "messages": messages_json,
                    "stream": stream,
                });
                if stream {
                    body["stream_options"] = json!({ "include_usage": true });
                }
                let mut req = http.post(&url).header("content-type", "application/json");
                if let Some(key) = &api_key {
                    req = req.bearer_auth(key);
                }
                for (k, v) in &headers {
                    req = req.header(k, v);
                }
                let resp = match req.json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        return TurnExecutionOutput {
                            reply_text: String::new(),
                            usage_delta: TokenTotals::default(),
                            failure: Some(TurnFailure {
                                class: FailureClass::Provider,
                                reason: format!("OpenAI HTTP request failed: {e}"),
                                status: None,
                                credential: None,
                                resets_at_ms: None,
                                rate_limit_type: None,
                            }),
                        };
                    }
                };
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    return TurnExecutionOutput {
                        reply_text: String::new(),
                        usage_delta: TokenTotals::default(),
                        failure: Some(classify_http_failure(status, &text)),
                    };
                }
                let is_sse = stream
                    && resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));
                if is_sse {
                    let mut full = String::new();
                    let mut reported: Option<TokenTotals> = None;
                    let mut byte_stream = resp.bytes_stream();
                    let mut buf = String::new();
                    while let Some(Ok(bytes)) = byte_stream.next().await {
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(nl) = buf.find('\n') {
                            let line = buf[..nl].trim_end_matches('\r').to_owned();
                            buf.drain(..=nl);
                            let Some(data) = line.strip_prefix("data:") else {
                                continue;
                            };
                            let data = data.trim();
                            if data.is_empty() || data == "[DONE]" {
                                continue;
                            }
                            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                                continue;
                            };
                            if let Some(reason) = ev
                                .pointer("/choices/0/delta/reasoning_content")
                                .and_then(Value::as_str)
                            {
                                emit_reasoning(reason);
                            }
                            if let Some(delta) = ev
                                .pointer("/choices/0/delta/content")
                                .and_then(Value::as_str)
                            {
                                emit_delta(delta);
                                full.push_str(delta);
                            }
                            if let Some(u) = ev.get("usage").filter(|u| u.is_object()) {
                                reported = Some(TokenTotals {
                                    input: u["prompt_tokens"].as_i64().unwrap_or(prompt_tokens_est),
                                    output: u["completion_tokens"]
                                        .as_i64()
                                        .unwrap_or_else(|| estimate_tokens(&full)),
                                    reasoning: u
                                        .pointer("/completion_tokens_details/reasoning_tokens")
                                        .and_then(Value::as_i64),
                                    cache_read: u
                                        .pointer("/prompt_tokens_details/cached_tokens")
                                        .and_then(Value::as_i64),
                                    cache_write: None,
                                });
                            }
                        }
                    }
                    emit_message(&full);
                    let usage_delta = reported.unwrap_or_else(|| TokenTotals {
                        input: prompt_tokens_est,
                        output: estimate_tokens(&full),
                        ..TokenTotals::default()
                    });
                    TurnExecutionOutput {
                        reply_text: full,
                        usage_delta,
                        failure: None,
                    }
                } else {
                    match resp.json::<Value>().await {
                        Ok(v) => {
                            let content = v
                                .pointer("/choices/0/message/content")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            emit_delta(&content);
                            emit_message(&content);
                            let u = &v["usage"];
                            let usage_delta = TokenTotals {
                                input: u["prompt_tokens"].as_i64().unwrap_or(prompt_tokens_est),
                                output: u["completion_tokens"]
                                    .as_i64()
                                    .unwrap_or_else(|| estimate_tokens(&content)),
                                reasoning: u
                                    .pointer("/completion_tokens_details/reasoning_tokens")
                                    .and_then(Value::as_i64),
                                cache_read: u
                                    .pointer("/prompt_tokens_details/cached_tokens")
                                    .and_then(Value::as_i64),
                                cache_write: None,
                            };
                            TurnExecutionOutput {
                                reply_text: content,
                                usage_delta,
                                failure: None,
                            }
                        }
                        Err(e) => TurnExecutionOutput {
                            reply_text: String::new(),
                            usage_delta: TokenTotals::default(),
                            failure: Some(TurnFailure {
                                class: FailureClass::Provider,
                                reason: format!("invalid OpenAI JSON body: {e}"),
                                status: None,
                                credential: None,
                                resets_at_ms: None,
                                rate_limit_type: None,
                            }),
                        },
                    }
                }
            }
            DurableProvider::AnthropicCompatible {
                base_url,
                api_key,
                max_tokens,
                headers,
                stream,
                ..
            } => {
                let url = resolve_anthropic_url(&base_url);
                let messages_json: Vec<Value> = history
                    .iter()
                    .map(|m| json!({ "role": m.role, "content": m.content }))
                    .collect();
                let mut body = json!({
                    "model": model,
                    "max_tokens": max_tokens,
                    "messages": messages_json,
                    "stream": stream,
                });
                if let Some(sys) = &append_system_prompt {
                    body["system"] = sys.clone().into();
                }
                let mut req = http
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("anthropic-version", "2023-06-01");
                if let Some(key) = &api_key {
                    req = req.header("x-api-key", key);
                }
                for (k, v) in &headers {
                    req = req.header(k, v);
                }
                let resp = match req.json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        return TurnExecutionOutput {
                            reply_text: String::new(),
                            usage_delta: TokenTotals::default(),
                            failure: Some(TurnFailure {
                                class: FailureClass::Provider,
                                reason: format!("Anthropic HTTP request failed: {e}"),
                                status: None,
                                credential: None,
                                resets_at_ms: None,
                                rate_limit_type: None,
                            }),
                        };
                    }
                };
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    return TurnExecutionOutput {
                        reply_text: String::new(),
                        usage_delta: TokenTotals::default(),
                        failure: Some(classify_http_failure(status, &text)),
                    };
                }
                let is_sse = stream
                    && resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));
                if is_sse {
                    let mut full = String::new();
                    let mut in_tokens = prompt_tokens_est;
                    let mut out_tokens: Option<i64> = None;
                    let mut cache_read: Option<i64> = None;
                    let mut cache_write: Option<i64> = None;
                    let mut byte_stream = resp.bytes_stream();
                    let mut buf = String::new();
                    while let Some(Ok(bytes)) = byte_stream.next().await {
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(nl) = buf.find('\n') {
                            let line = buf[..nl].trim_end_matches('\r').to_owned();
                            buf.drain(..=nl);
                            let Some(data) = line.strip_prefix("data:") else {
                                continue;
                            };
                            let data = data.trim();
                            if data.is_empty() {
                                continue;
                            }
                            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                                continue;
                            };
                            match ev["type"].as_str() {
                                Some("message_start") => {
                                    let u = &ev["message"]["usage"];
                                    if let Some(n) = u["input_tokens"].as_i64() {
                                        in_tokens = n;
                                    }
                                    cache_read = u["cache_read_input_tokens"].as_i64();
                                    cache_write = u["cache_creation_input_tokens"].as_i64();
                                }
                                Some("content_block_delta") => match ev["delta"]["type"].as_str() {
                                    Some("text_delta") => {
                                        if let Some(t) = ev["delta"]["text"].as_str() {
                                            emit_delta(t);
                                            full.push_str(t);
                                        }
                                    }
                                    Some("thinking_delta") => {
                                        if let Some(t) = ev["delta"]["thinking"].as_str() {
                                            emit_reasoning(t);
                                        }
                                    }
                                    _ => {}
                                },
                                Some("message_delta") => {
                                    if let Some(n) = ev["usage"]["output_tokens"].as_i64() {
                                        out_tokens = Some(n);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    emit_message(&full);
                    TurnExecutionOutput {
                        usage_delta: TokenTotals {
                            input: in_tokens,
                            output: out_tokens.unwrap_or_else(|| estimate_tokens(&full)),
                            cache_read,
                            cache_write,
                            reasoning: None,
                        },
                        reply_text: full,
                        failure: None,
                    }
                } else {
                    match resp.json::<Value>().await {
                        Ok(v) => {
                            let mut full = String::new();
                            for block in v["content"].as_array().into_iter().flatten() {
                                if block["type"] == "text"
                                    && let Some(t) = block["text"].as_str()
                                {
                                    emit_delta(t);
                                    full.push_str(t);
                                } else if block["type"] == "thinking"
                                    && let Some(t) = block["thinking"].as_str()
                                {
                                    emit_reasoning(t);
                                }
                            }
                            emit_message(&full);
                            let u = &v["usage"];
                            TurnExecutionOutput {
                                usage_delta: TokenTotals {
                                    input: u["input_tokens"].as_i64().unwrap_or(prompt_tokens_est),
                                    output: u["output_tokens"]
                                        .as_i64()
                                        .unwrap_or_else(|| estimate_tokens(&full)),
                                    cache_read: u["cache_read_input_tokens"].as_i64(),
                                    cache_write: u["cache_creation_input_tokens"].as_i64(),
                                    reasoning: None,
                                },
                                reply_text: full,
                                failure: None,
                            }
                        }
                        Err(e) => TurnExecutionOutput {
                            reply_text: String::new(),
                            usage_delta: TokenTotals::default(),
                            failure: Some(TurnFailure {
                                class: FailureClass::Provider,
                                reason: format!("invalid Anthropic JSON body: {e}"),
                                status: None,
                                credential: None,
                                resets_at_ms: None,
                                rate_limit_type: None,
                            }),
                        },
                    }
                }
            }
        }
    };

    tokio::select! {
        biased;
        _ = cancel_rx.changed() => TurnExecutionOutput {
            reply_text: String::new(),
            usage_delta: TokenTotals::default(),
            failure: None,
        },
        out = run => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router, http::StatusCode as AxumStatus, response::IntoResponse, routing::post,
    };

    #[tokio::test]
    async fn file_storage_persists_resumes_validates_cwd_and_emits_released_on_close() -> Result<()>
    {
        let store_dir = tempfile::tempdir()?;
        let ws_dir = tempfile::tempdir()?;
        let other_dir = tempfile::tempdir()?;

        let storage = FileStorage::new(store_dir.path().join("durable-db"))?;
        let provider = DurableProvider::scripted("durable-script-1", |history, model| {
            let last = history.last().map(|m| m.content.as_str()).unwrap_or("");
            Ok(DurableResponderOutput::chunks([
                format!("[{model}:{} msgs] ", history.len()),
                format!("ack:{last}"),
            ]))
        });
        let engine = PiDurable::new(storage.clone(), provider.clone());

        let session1 = engine.open(SessionOptions::new(ws_dir.path())).await?;
        let sid = session1.id().to_owned();

        let mut events1 = session1.events(None)?;
        let turn1 = session1
            .prompt("hello durable world")
            .await?
            .report()
            .await?;
        assert_eq!(turn1.outcome, Outcome::Completed);
        assert_eq!(
            turn1.text,
            "[durable-script-1:1 msgs] ack:hello durable world"
        );

        let first_ev = events1.next().await.unwrap();
        let saved_cursor = first_ev.cursor.clone();

        // Close detaches controller and emits Event::Released (ReleaseReason::Dispose).
        session1.close().await?;
        let mut saw_released = false;
        while let Some(ev) = events1.next().await {
            if ev.event == Event::Released {
                saw_released = true;
            }
        }
        assert!(saw_released);
        let release_meta = session1
            .records()
            .iter()
            .find_map(|r| r.session_release())
            .expect("SessionRelease metadata attached");
        assert_eq!(release_meta.reason, ReleaseReason::Dispose);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_path = store_dir
                .path()
                .join("durable-db")
                .join(format!("{sid}.jsonl"));
            let mode = fs::metadata(file_path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Resuming an unknown session fails with OpenError::SessionNotFound.
        let mut missing_opts = SessionOptions::new(ws_dir.path());
        missing_opts.resume = Some("nonexistent-session".into());
        assert!(matches!(
            engine.open(missing_opts).await,
            Err(Error::Open(err)) if matches!(&*err, OpenError::SessionNotFound { .. })
        ));

        // Resuming from a different workspace fails with OpenError::ResumeCwd.
        let mut wrong_cwd = SessionOptions::new(other_dir.path());
        wrong_cwd.resume = Some(sid.clone());
        assert!(matches!(
            engine.open(wrong_cwd).await,
            Err(Error::Open(err)) if matches!(&*err, OpenError::ResumeCwd { .. })
        ));

        // Reopen with fresh PiDurable instance over the same FileStorage!
        let engine2 = PiDurable::new(
            FileStorage::new(store_dir.path().join("durable-db"))?,
            provider,
        );
        let mut resume_opts = SessionOptions::new(ws_dir.path());
        resume_opts.resume = Some(sid.clone());
        let session2 = engine2.open(resume_opts).await?;
        assert_eq!(session2.id(), sid);

        // Prior conversation history (user + assistant = 2 messages + new user = 3 messages) is preserved!
        let turn2 = session2.prompt("second turn").await?.report().await?;
        assert_eq!(turn2.outcome, Outcome::Completed);
        assert_eq!(turn2.text, "[durable-script-1:3 msgs] ack:second turn");

        // Cursor-resumable ObservedEvent stream works cleanly on the reopened session!
        let mut all_ev = session2.events(None)?;
        let e0 = all_ev.next().await.unwrap();
        let e1 = all_ev.next().await.unwrap();
        let mut resumed_ev = session2.events(Some(&e0.cursor))?;
        assert_eq!(resumed_ev.next().await.unwrap(), e1);
        let _ = saved_cursor;

        session2.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn openai_and_anthropic_sse_providers_stream_deltas_and_token_usage() -> Result<()> {
        async fn mock_openai(Json(body): Json<Value>) -> impl IntoResponse {
            assert_eq!(body["model"], "gpt-test");
            let sse = concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think...\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"OpenAI \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"stream!\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":6,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
                "data: [DONE]\n\n"
            );
            (
                AxumStatus::OK,
                [(reqwest::header::CONTENT_TYPE, "text/event-stream")],
                sse,
            )
        }

        async fn mock_anthropic(Json(body): Json<Value>) -> impl IntoResponse {
            assert_eq!(body["model"], "claude-test");
            assert_eq!(body["system"], "Be concise.");
            let sse = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":15,\"cache_read_input_tokens\":4}}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Anthropic \"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"OK!\"}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n"
            );
            (
                AxumStatus::OK,
                [(reqwest::header::CONTENT_TYPE, "text/event-stream")],
                sse,
            )
        }

        let app = Router::new()
            .route("/v1/chat/completions", post(mock_openai))
            .route("/v1/messages", post(mock_anthropic));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let ws = tempfile::tempdir()?;

        // 1. OpenAI-compatible SSE
        let oai_engine = PiDurable::new(
            MemoryStorage::new(),
            DurableProvider::openai(format!("http://{addr}"), Some("sk-test".into()), "gpt-test"),
        );
        let oai_sess = oai_engine.open(SessionOptions::new(ws.path())).await?;
        let oai_rep = oai_sess.prompt("ping openai").await?.report().await?;
        assert_eq!(oai_rep.outcome, Outcome::Completed);
        assert_eq!(oai_rep.text, "OpenAI stream!");
        let oai_totals = oai_sess.usage().total.unwrap();
        assert_eq!(oai_totals.input, 12);
        assert_eq!(oai_totals.output, 6);
        assert_eq!(oai_totals.reasoning, Some(2));
        oai_sess.close().await?;

        // 2. Anthropic-compatible SSE
        let ant_engine = PiDurable::new(
            MemoryStorage::new(),
            DurableProvider::anthropic(
                format!("http://{addr}"),
                Some("sk-ant".into()),
                "claude-test",
            ),
        );
        let mut ant_opts = SessionOptions::new(ws.path());
        ant_opts.settings.append_system_prompt = Some("Be concise.".into());
        let ant_sess = ant_engine.open(ant_opts).await?;
        let ant_rep = ant_sess.prompt("ping anthropic").await?.report().await?;
        assert_eq!(ant_rep.outcome, Outcome::Completed);
        assert_eq!(ant_rep.text, "Anthropic OK!");
        let ant_totals = ant_sess.usage().total.unwrap();
        assert_eq!(ant_totals.input, 15);
        assert_eq!(ant_totals.output, 5);
        assert_eq!(ant_totals.cache_read, Some(4));
        ant_sess.close().await?;

        srv.abort();
        Ok(())
    }
}
