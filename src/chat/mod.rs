//! Persistent native chats. A per-chat file lock gives one process ownership of the provider session.
pub(crate) mod checkpoint;
pub(crate) mod feed;
mod handoff;
mod http_feed;
pub use feed::{ChatCursor, ChatEvent, ChatFeed, ChatFeedSnapshot, ChatReplica, event_router};
pub use handoff::{Handoff, HandoffRequest, HandoffStatus, ProviderSession};
pub use http_feed::HttpChatFeed;
pub use log::ChatUpdate;
pub use timeline::{Edit as ChatEdit, Frame as ChatFrame, MorphProgress};
mod fork;
pub use fork::{ForkOrigin, ForkWorktreeBase};
mod import;
pub use checkpoint::{ChangeKind, Checkpoint, CheckpointPoint, FileChange, RollbackReport};
mod log;
pub(crate) use log::pi::recover as recover_pi;
mod queue;
pub(crate) use queue::Admission;
pub use queue::{InputStatus, QueuedInput};
mod timeline;
pub(crate) use log::Sink;
pub use timeline::{ChatTurn, Item, Role, TimelineItem, TurnStatus};

use crate::{
    ApprovalMode, Error, PendingRequest, PermissionDecision, Protocol, Result, Session,
    SessionOptions, runtime,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{DirBuilder, File, OpenOptions},
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatInfo {
    pub id: Uuid,
    pub workspace: PathBuf,
    /// Stable checkout identity for Kar-managed worktrees, including chats in a subdirectory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<Uuid>,
    pub runtime: String,
    pub protocol: Protocol,
    pub native_id: String,
    pub created_ms: u64,
    pub program: PathBuf,
    pub model: Option<String>,
    #[serde(default)]
    pub settings: crate::SessionSettings,
    /// Only removal names are persisted. Environment values belong to the launch.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub unset_env: std::collections::BTreeSet<String>,
    pub approvals: ApprovalMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ForkOrigin>,
}
impl ChatInfo {
    fn pin_worktree(&self) -> Result<Option<File>> {
        crate::workbench_git::pin_saved_checkout(&self.workspace, self.worktree_id)
    }
    fn tracks_steering_receipts(&self) -> bool {
        matches!(self.protocol, Protocol::Claude | Protocol::Pi)
            || (self.protocol == Protocol::Acp
                && matches!(self.runtime.as_str(), "antigravity" | "opencode" | "grok"))
    }
    fn awaits_steering_receipt(&self, status: crate::SteerStatus) -> bool {
        // Pi's queued disposition is already a native receipt. Only a submitted
        // call can lose its acknowledgement when the controller exits.
        if self.protocol == Protocol::Pi {
            status == crate::SteerStatus::Submitted
        } else {
            status.is_waiting()
        }
    }
    fn checkpoint_store(&self) -> Uuid {
        self.forked_from
            .as_ref()
            .map_or(self.id, |f| f.checkpoint_store)
    }
    fn options(&self) -> SessionOptions {
        let mut opts = SessionOptions::new(&self.workspace);
        opts.model = self.model.clone();
        opts.settings = self.settings.clone();
        if self.protocol == Protocol::KimiKap
            || (self.protocol == Protocol::Acp && self.runtime == "kimi")
        {
            // Kimi accepts additional roots only at creation on both native transports.
            opts.settings.additional_directories.clear();
        }
        opts.unset_env.clone_from(&self.unset_env);
        opts.approvals = self.approvals;
        opts
    }
    pub(crate) async fn launch_runtime(&self, opts: &SessionOptions) -> Result<crate::Runtime> {
        let rt = self.runtime()?;
        if rt.program.is_absolute() {
            Ok(rt)
        } else {
            rt.selected(opts).await
        }
    }
    fn runtime(&self) -> Result<crate::Runtime> {
        let mut rt = runtime(&self.runtime)?;
        // Chats created through the former ACP adapter keep their native transport.
        if self.runtime == "dsh" && self.protocol == Protocol::Acp {
            rt.protocol = Protocol::Acp;
            rt.args = vec!["--profile".into(), "acp".into()];
        }
        if self.runtime == "factory" && self.protocol == Protocol::Acp {
            rt.protocol = Protocol::Acp;
            rt.args = ["exec", "--output-format", "acp"].map(Into::into).to_vec();
        }
        if self.runtime == "kimi" && self.protocol == Protocol::Acp {
            rt.protocol = Protocol::Acp;
            rt.args = vec!["acp".into()];
        }
        if self.runtime == "qoder" && self.protocol == Protocol::Acp {
            rt.protocol = Protocol::Acp;
            rt.args = vec!["--acp".into()];
        }
        if rt.protocol != self.protocol {
            return Err(Error::Unsupported(
                "saved provider protocol has changed".into(),
            ));
        }
        rt.program = self.program.clone();
        Ok(rt)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatNotification {
    pub activity: ChatTurn,
    /// Set when the same native model loop later incorporates this host input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_input: Option<Uuid>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatHistory {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoffs: Vec<Handoff>,
    pub chat: ChatInfo,
    pub cursor: Option<crate::Cursor>,
    pub turns: Vec<ChatTurn>,
    /// Independent native activity, including Claude notifications and Pi extension runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notifications: Vec<ChatNotification>,
    #[serde(
        default,
        deserialize_with = "crate::goal::observed",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal: Option<Option<crate::GoalSnapshot>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<crate::NativeTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<crate::ContextUsage>,
    /// Latest reported token usage per controller stream, in open order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<crate::UsageSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_requests: Vec<PendingRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction: Option<crate::CompactionReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_compaction: Option<crate::ManualCompactionReport>,
    #[serde(default)]
    pub workspace_position: Option<CheckpointPoint>,
    #[serde(default)]
    pub pending_rollback: Option<CheckpointPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_fork: Option<ForkOrigin>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
impl SessionOptions {
    // Durable history and restart reconciliation consume the complete native handshake.
    fn before_chat_attach(&self) -> Result<Self> {
        crate::stream::retention::check_limit(self.retained_bytes)?;
        Ok(Self {
            retained_bytes: None,
            ..self.clone()
        })
    }
}
#[derive(Clone)]
pub struct ChatStore {
    dir: PathBuf,
}
impl ChatStore {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let mut create = DirBuilder::new();
        create.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            create.mode(0o700);
        }
        create.create(dir.as_ref())?;
        Ok(Self {
            dir: dir.as_ref().canonicalize()?,
        })
    }
    pub fn local() -> Result<Self> {
        if let Some(dir) = std::env::var_os("KAR_CHAT_DIR").filter(|v| !v.is_empty()) {
            return Self::open(dir);
        }
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|p| p.join(".local/state")))
            .ok_or_else(|| Error::Protocol("set KAR_CHAT_DIR to a writable directory".into()))?;
        Self::open(state.join("kar/chats"))
    }
    pub(crate) fn directory(&self) -> &Path {
        &self.dir
    }
    pub(crate) fn initial_info(&self, id: Uuid) -> Result<ChatInfo> {
        Ok(log::parse_header(
            &mut std::io::BufReader::new(File::open(self.path(id))?),
            Some(id),
        )?
        .0
        .info)
    }
    pub(crate) fn saved_info(&self, id: Uuid) -> Result<ChatInfo> {
        log::info(File::open(self.path(id))?, id)
    }
    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.karchat"))
    }
    /// Ask in a disposable native fork of a saved conversation.
    pub async fn side_question(&self, id: Uuid, question: &str) -> Result<String> {
        let info = log::info(File::open(self.path(id))?, id)?;
        let _pin = info.pin_worktree()?;
        if info.workspace.canonicalize()? != info.workspace {
            return Err(Error::Refused(
                "saved workspace resolves to a different path".into(),
            ));
        }
        let mut options = crate::SideQuestionOptions::new(&info.workspace);
        options.unset_env = info.unset_env.clone();
        options.model = info.model.clone();
        options.system_prompt = info.settings.system_prompt.clone();
        options.append_system_prompt = info.settings.append_system_prompt.clone();
        options.exclude_user_configuration = info.settings.exclude_user_configuration.clone();
        info.runtime()?
            .side_question(&info.native_id, question, &options)
            .await
    }
    /// Most recently used chats first. Reading history never changes this order.
    pub fn list(&self) -> Result<Vec<ChatInfo>> {
        let mut list = Vec::new();
        for item in std::fs::read_dir(&self.dir)? {
            let item = item?;
            let path = item.path();
            if path.extension().is_none_or(|s| s != "karchat") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
            else {
                continue;
            };
            let file = File::open(&path)?;
            list.push((file.metadata()?.modified()?, log::info(file, id)?));
        }
        list.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| y.id.cmp(&x.id)));
        Ok(list.into_iter().map(|(_, info)| info).collect())
    }
    pub fn latest(&self) -> Result<Uuid> {
        self.list()?
            .first()
            .map(|c| c.id)
            .ok_or_else(|| Error::Refused("no saved chats in this store".into()))
    }
    fn lease(&self, id: Uuid, create: bool) -> Result<File> {
        let mut open = OpenOptions::new();
        open.read(true).write(true).create(create).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        Ok(open.open(self.dir.join(format!("{id}.lock")))?)
    }
    fn read_state(&self, mut file: File, id: Uuid) -> Result<log::State> {
        // A controller takes the journal exclusively when claiming its lease. Hold
        // the shared journal lock across the snapshot and the short ownership probe.
        log::with_lock(&mut file, false, |file| {
            let (mut state, _) = log::read_locked(file, id)?;
            if state.needs_controller() {
                let lease = File::open(self.dir.join(format!("{id}.lock")))?;
                match lease.try_lock_shared() {
                    Ok(()) => {
                        lease.unlock()?;
                        state.pending_requests.clear();
                        state.interrupt_notifications();
                        state.unconfirm_steering();
                        if let Some(report) = state.abandoned_compaction() {
                            state.manual_compaction = Some(report);
                        }
                        if let Some(t) = state
                            .turns
                            .last_mut()
                            .filter(|t| t.status == TurnStatus::Running)
                        {
                            t.finish(TurnStatus::Interrupted, &state.info.native_id);
                        }
                    }
                    Err(std::fs::TryLockError::WouldBlock) => {}
                    Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
                }
            }
            Ok(state)
        })
    }
    pub fn history(&self, id: Uuid) -> Result<ChatHistory> {
        let state = self.read_state(File::open(self.path(id))?, id)?;
        self.history_from_state(id, state)
    }
    fn history_from_state(&self, id: Uuid, state: log::State) -> Result<ChatHistory> {
        Ok(ChatHistory {
            cursor: state.cursor(),
            workspace_position: state.position,
            pending_rollback: state.restoring,
            pending_fork: self.pending_fork(id)?,
            handoffs: state.handoffs,
            chat: state.info,
            turns: state.turns,
            goal: state.goal,
            tasks: state.tasks,
            notifications: state.notifications,
            context_usage: state.context_usage,
            usage: state.usage,
            pending_requests: state.pending_requests,
            last_compaction: state.last_compaction,
            manual_compaction: state.manual_compaction,
        })
    }
    pub async fn create(
        &self,
        runtime_id: &str,
        program: Option<&Path>,
        opts: SessionOptions,
    ) -> Result<ChatSession> {
        self.create_with_id(Uuid::new_v4(), runtime_id, program, opts)
            .await
    }

    pub(crate) async fn create_with_id(
        &self,
        id: Uuid,
        runtime_id: &str,
        program: Option<&Path>,
        mut opts: SessionOptions,
    ) -> Result<ChatSession> {
        let lease = self.lease(id, true)?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        if self.fork_claimed(id)? {
            return Err(Error::Refused(
                "chat UUID belongs to a native fork request".into(),
            ));
        }
        if self.path(id).try_exists()? {
            return Err(Error::Refused("chat UUID already exists".into()));
        }
        opts.cwd = opts.cwd.canonicalize()?;
        let worktree = crate::workbench_git::pin_checkout(&opts.cwd)?;
        let mut rt = runtime(runtime_id)?;
        if let Some(bin) = program {
            rt.program = bin.to_owned();
        }
        if rt.program.is_relative() && rt.program.components().count() > 1 {
            rt.program = rt.program.canonicalize()?;
        }
        let (rt, version) = if program.is_some() && rt.program.is_absolute() {
            (rt, None)
        } else {
            rt.selected_with_version(&opts).await?
        };
        let session = rt
            .open_selected_with_version(opts.before_chat_attach()?, version.as_deref())
            .await?;
        if !session.capabilities().resume {
            session.close().await?;
            return Err(Error::Unsupported(
                "this agent did not advertise session recovery".into(),
            ));
        }
        let info = ChatInfo {
            id,
            workspace: opts.cwd.clone(),
            worktree_id: worktree.as_ref().map(|(id, _)| *id),
            runtime: rt.id,
            protocol: session.protocol(),
            program: rt.program,
            native_id: session.id().into(),
            model: opts.model.clone(),
            settings: opts.settings.clone(),
            unset_env: opts.unset_env.clone(),
            approvals: opts.approvals,
            created_ms: now(),
            forked_from: None,
        };
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        log::header(tmp.as_file_mut(), &info)?;
        let chat = ChatSession::bind(
            session,
            log::Log::new(tmp.as_file().try_clone()?, lease, info),
            self.clone(),
            opts,
        )?;
        tmp.persist_noclobber(self.path(chat.info().id))
            .map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(chat)
    }
    pub async fn resume(&self, id: Uuid) -> Result<ChatSession> {
        self.resume_with(id, |_| {}).await
    }
    /// Supply per-process environment, MCP servers and other launch settings; the stored session and cwd are authoritative.
    pub async fn resume_with(
        &self,
        id: Uuid,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<ChatSession> {
        let mut log = self.open_controller(id).await?;
        if log.state.pending_handoff().is_some() {
            let (result, session) = self.finish_handoff(log, configure).await?;
            return session.ok_or_else(|| {
                Error::Refused(result.error.unwrap_or_else(|| "handoff failed".into()))
            });
        }
        self.require_no_pending_fork(id)?;
        if log.state.restoring.is_some() {
            return Err(Error::Refused(
                "finish the pending workspace rollback first".into(),
            ));
        }
        let info = &log.state.info;
        let _pin = info.pin_worktree()?;
        if info.workspace.canonicalize()? != info.workspace {
            return Err(Error::Refused(
                "saved workspace now resolves to a different path".into(),
            ));
        }
        let mut opts = info.options();
        configure(&mut opts);
        opts.cwd = info.workspace.clone();
        let session = match Self::open_saved(&log.state, opts.before_chat_attach()?).await {
            Ok(session) => session,
            Err(error) => self.renew_empty_target(&mut log, &opts, error).await?,
        };
        log.recover(&session, &opts, self).await?;
        ChatSession::bind(session, log, self.clone(), opts)
    }
    async fn open_controller(&self, id: Uuid) -> Result<log::Log> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.locked_log(id))
            .await
            .map_err(|e| Error::Closed(e.to_string()))?
    }
    fn locked_log(&self, id: Uuid) -> Result<log::Log> {
        let lease = self.lease(id, false)?;
        // An orphan can be settled by an observer while the controller is offline.
        let mut journal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.path(id))?;
        log::with_lock(&mut journal, true, |_| match lease.try_lock() {
            Ok(()) => Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::Refused(format!(
                "chat {id} is open in another controller"
            ))),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        })?;
        Self::load_log(&self.path(id), id, lease)
    }
    fn load_log(path: &Path, id: Uuid, lease: File) -> Result<log::Log> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let state = log::with_lock(&mut file, true, |file| {
            let (state, valid) = log::read_locked(file, id)?;
            if valid != file.metadata()?.len() {
                file.set_len(valid)?;
                file.sync_data()?;
            }
            file.seek(SeekFrom::End(0))?;
            Ok(state)
        })?;
        let mut log = log::Log::loaded(file, lease, state);
        log.settle_abandoned_controls()?;
        Ok(log)
    }
}
pub struct ChatSession {
    launch: SessionOptions,
    sender: Arc<Sender>,
    worker: queue::QueueWorker,
}
struct Sender {
    _worktree: Option<File>,
    store: ChatStore,
    session: Arc<Session>,
    sink: Sink,
    info: ChatInfo,
}
impl ChatSession {
    fn bind(
        session: Session,
        mut log: log::Log,
        store: ChatStore,
        launch: SessionOptions,
    ) -> Result<Self> {
        if log.state.stream != Some(session.stream_id()) {
            log.attach(session.stream_id())?;
        }
        if session.capabilities().goals
            || session.protocol() == Protocol::Claude
            || log.state.info.runtime == "grok"
        {
            log.native_store = Some(store.clone());
        }
        let info = log.state.info.clone();
        let worktree = info.pin_worktree()?;
        let sink = Sink(Arc::new(Mutex::new(log)));
        session.attach_chat(sink.clone(), launch.retained_bytes)?;
        let sender = Arc::new(Sender {
            _worktree: worktree,
            store,
            session: Arc::new(session),
            sink,
            info,
        });
        let worker = queue::QueueWorker::start(&sender);
        Ok(Self {
            launch,
            sender,
            worker,
        })
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.sender.session.is_closed()
    }
    pub(crate) fn live_session(&self) -> std::sync::Weak<Session> {
        Arc::downgrade(&self.sender.session)
    }
    pub(crate) fn needs_controller(&self) -> Result<bool> {
        let log = self.sender.sink.0.lock().unwrap();
        log.healthy()?;
        Ok(log.state.needs_controller())
    }
    pub fn info(&self) -> &ChatInfo {
        &self.sender.info
    }
    pub fn history(&self) -> Result<ChatHistory> {
        let log = self.sender.sink.0.lock().unwrap();
        log.healthy()?;
        Ok(ChatHistory {
            handoffs: log.state.handoffs.clone(),
            chat: self.sender.info.clone(),
            workspace_position: log.state.position,
            pending_rollback: log.state.restoring,
            pending_fork: self.sender.store.pending_fork(self.sender.info.id)?,
            cursor: log.state.cursor(),
            turns: log.state.turns.clone(),
            goal: log.state.goal.clone(),
            tasks: log.state.tasks.clone(),
            notifications: log.state.notifications.clone(),
            context_usage: log.state.context_usage.clone(),
            usage: log.state.usage.clone(),
            pending_requests: log.state.pending_requests.clone(),
            last_compaction: log.state.last_compaction.clone(),
            manual_compaction: log.state.manual_compaction.clone(),
        })
    }
    pub fn subscribe(&self, cursor: Option<&crate::Cursor>) -> Result<crate::Subscription> {
        self.sender.session.subscribe(cursor)
    }
    pub fn observe_stalls(
        &self,
        threshold: std::time::Duration,
    ) -> Result<crate::observe::StallSubscription> {
        self.sender.session.observe_stalls(threshold)
    }
    /// Capabilities of this open, including settings resolved by its native adapter.
    pub fn capabilities(&self) -> crate::Capabilities {
        self.sender.session.capabilities()
    }
    pub fn snapshot(&self) -> crate::observe::SessionSnapshot {
        self.sender.session.snapshot()
    }
    pub async fn start_compaction(&self) -> Result<crate::ManualCompaction> {
        if self
            .sender
            .store
            .queued_inputs(self.info().id)?
            .iter()
            .any(|input| matches!(input.status, InputStatus::Pending | InputStatus::Sending))
        {
            return Err(Error::Busy);
        }
        self.sender.session.start_compaction().await
    }
    pub async fn context_breakdown(&self) -> Result<crate::ContextBreakdown> {
        self.sender.session.context_breakdown().await
    }
    pub async fn last_model_request(&self) -> Result<Option<crate::ModelRequest>> {
        self.sender.session.last_model_request().await
    }
    pub async fn resources(&self) -> Option<crate::SessionResources> {
        self.sender.session.resources().await
    }
    pub fn usage(&self) -> Result<Vec<crate::UsageSnapshot>> {
        let log = self.sender.sink.0.lock().unwrap();
        log.healthy()?;
        Ok(log.state.usage.clone())
    }
    pub fn tasks(&self) -> Result<Vec<crate::NativeTask>> {
        let log = self.sender.sink.0.lock().unwrap();
        log.healthy()?;
        Ok(log.state.tasks.clone())
    }
    pub async fn stop_task(&self, id: &str) -> Result<()> {
        self.sender.session.stop_task(id).await
    }
    pub async fn read_goal(&self) -> Result<Option<crate::GoalSnapshot>> {
        let result = self.sender.session.read_goal().await?;
        self.sender.sink.0.lock().unwrap().healthy()?;
        Ok(result)
    }
    pub async fn set_goal(&self, options: crate::GoalSetOptions) -> Result<crate::GoalSnapshot> {
        let result = self.sender.session.set_goal(options).await?;
        self.sender.sink.0.lock().unwrap().healthy()?;
        Ok(result)
    }
    pub async fn clear_goal(&self) -> Result<()> {
        self.sender.session.clear_goal().await?;
        self.sender.sink.0.lock().unwrap().healthy()
    }
    pub async fn select_policy(
        &self,
        p: &PendingRequest,
        choice: crate::PolicyChoice,
    ) -> Result<()> {
        self.sender.session.select_policy(p, choice).await
    }
    pub async fn grant_resources(
        &self,
        p: &PendingRequest,
        scope: PermissionDecision,
        resources: Vec<crate::ResourceId>,
    ) -> Result<()> {
        self.sender
            .session
            .grant_resources(p, scope, resources)
            .await
    }
    pub async fn answer_question(
        &self,
        p: &PendingRequest,
        reply: crate::QuestionReply,
    ) -> Result<()> {
        self.sender.session.answer_question(p, reply).await
    }
    pub async fn decide(&self, p: &PendingRequest, d: PermissionDecision) -> Result<()> {
        self.sender.session.decide(p, d).await
    }
    pub async fn answer_request(&self, p: &PendingRequest, v: serde_json::Value) -> Result<()> {
        self.sender.session.answer_request(p, v).await
    }
    pub async fn abort(&self) -> Result<()> {
        self.sender.session.abort().await
    }
    /// Send a follow-up into the running native turn.
    pub async fn steer(&self, text: impl Into<String>) -> Result<()> {
        self.sender.session.steer(text).await
    }
    pub async fn steer_with_id(&self, id: Uuid, text: impl Into<String>) -> Result<()> {
        self.sender.session.steer_with_id(id, text).await
    }
    pub async fn steer_images_with_id(
        &self,
        id: Uuid,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<()> {
        self.sender
            .session
            .steer_images_with_id(id, text, images)
            .await
    }
    pub async fn steer_images(
        &self,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<()> {
        self.sender.session.steer_images(text, images).await
    }
    /// Restore the workspace while keeping an idle provider session attached.
    pub async fn rollback(&self, target: CheckpointPoint, dry_run: bool) -> Result<RollbackReport> {
        let store = self.sender.store.clone();
        let sink = self.sender.sink.clone();
        tokio::task::spawn_blocking(move || {
            let mut log = sink.0.lock().unwrap();
            log.healthy()?;
            store.rollback_locked(&mut log, target, dry_run)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
    /// A reused id returns the recorded turn. Reusing an id for different text is an error.
    pub async fn send(&self, id: Uuid, text: impl Into<String>) -> Result<ChatTurn> {
        let sender = self.sender.clone();
        let text = text.into();
        // Dispatch and completion outlive a disconnected caller.
        tokio::spawn(async move { sender.run_turn(id, text).await })
            .await
            .map_err(|e| Error::Closed(e.to_string()))?
    }
    pub async fn close(self) -> Result<()> {
        self.worker.request_stop();
        let closed = self.sender.session.close().await;
        self.worker.join().await;
        let released = self.sender.sink.0.lock().unwrap().finish_on_release();
        drop(self.sender);
        released
            .await
            .map_err(|_| Error::Closed("chat journal stopped during close".into()))??;
        closed
    }
}
impl Sender {
    async fn run_turn(&self, id: Uuid, text: String) -> Result<ChatTurn> {
        if let Some(turn) = self.sink.0.lock().unwrap().existing(id, &text)? {
            return Ok(turn);
        }
        crate::input::check_text(&text, crate::input::text_limit(&self.info.runtime))?;
        let store = self.store.clone();
        let info = self.info.clone();
        let (store, info, lease) = tokio::task::spawn_blocking(move || -> Result<_> {
            let lease = store.workspace_lease(&info.workspace)?;
            store.require_no_pending_fork(info.id)?;
            Ok((store, info, lease))
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        let expected = {
            let log = self.sink.0.lock().unwrap();
            if let Some(turn) = log.existing(id, &text)? {
                return Ok(turn);
            }
            info.forked_from.as_ref().and_then(|f| {
                log.state
                    .turns
                    .last()
                    .filter(|t| {
                        t.id == f.turn
                            && log.state.position
                                == Some(CheckpointPoint {
                                    turn: f.turn,
                                    before: false,
                                })
                    })
                    .and_then(|t| t.checkpoint.as_ref()?.after_tree.clone())
            })
        };
        let (store, before) = tokio::task::spawn_blocking(move || -> Result<_> {
            let before = store.capture(
                &info,
                CheckpointPoint {
                    turn: id,
                    before: true,
                },
                expected.as_deref(),
            )?;
            Ok((store, before))
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        let wire_text = self.sink.0.lock().unwrap().state.dispatch_text(&text);
        crate::input::check_text(&wire_text, crate::input::text_limit(&self.info.runtime))?;
        let admission = Admission {
            store: store.clone(),
            sink: self.sink.clone(),
            id,
            text: text.clone(),
            checkpoint: log::TurnCapture {
                store,
                id,
                before,
                _lease: lease,
            },
        };
        let result = match self
            .session
            .start_prompt(id, crate::input::Input::text(wire_text), Some(admission))
            .await
        {
            Ok(turn) => {
                let receipt = self.sink.0.lock().unwrap().input_sent(id);
                let outcome = turn.wait().await;
                receipt?;
                outcome
            }
            Err(e) if self.sink.0.lock().unwrap().state.find(id).is_none() => return Err(e),
            Err(e) => Err(e),
        };
        let status = match &result {
            Ok(outcome) => outcome.into(),
            Err(Error::Closed(_) | Error::Timeout(_)) => TurnStatus::Interrupted,
            Err(_) => TurnStatus::Failed,
        };
        let sink = self.sink.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut log = sink.0.lock().unwrap();
            log.healthy()?;
            log.end(id, status)?;
            log.finish_checkpoint(id)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        let log = self.sink.0.lock().unwrap();
        result?;
        Ok(log.state.find(id).expect("admitted turn").clone())
    }
}
