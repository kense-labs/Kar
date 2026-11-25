//! Replay and follow a durable chat without acquiring the provider session.
use super::{ChatHistory, ChatInfo, ChatStore, ChatTurn, ChatUpdate, log};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
    routing::get,
};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fmt,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

/// Opaque exclusive continuation for this chat, including across native process restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ChatCursor {
    chat: Uuid,
    offset: u64,
}
impl fmt::Display for ChatCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.chat, self.offset)
    }
}
impl From<ChatCursor> for String {
    fn from(cursor: ChatCursor) -> Self {
        cursor.to_string()
    }
}
impl FromStr for ChatCursor {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        let (chat, offset) = value.split_once(':').ok_or(Error::InvalidCursor)?;
        Ok(Self {
            chat: chat.parse().map_err(|_| Error::InvalidCursor)?,
            offset: offset.parse().map_err(|_| Error::InvalidCursor)?,
        })
    }
}
impl TryFrom<String> for ChatCursor {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatEvent {
    pub previous: ChatCursor,
    pub cursor: ChatCursor,
    pub update: ChatUpdate,
}
impl ChatEvent {
    pub(super) fn follows(&self, cursor: ChatCursor) -> Result<bool> {
        if self.cursor.chat != cursor.chat
            || self.previous.chat != cursor.chat
            || self.previous.offset >= self.cursor.offset
        {
            return Err(Error::InvalidCursor);
        }
        if self.cursor.offset <= cursor.offset {
            return Ok(false);
        }
        if self.previous != cursor {
            return Err(Error::InvalidCursor);
        }
        Ok(true)
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatFeedSnapshot {
    pub cursor: ChatCursor,
    /// Latest native stream, even when it has not emitted a frame yet.
    pub stream: Option<Uuid>,
    pub history: ChatHistory,
}

/// A client-side view using the same message identity and tool projection as chat history.
pub struct ChatReplica {
    pub(super) state: log::State,
    cursor: ChatCursor,
}
impl ChatReplica {
    pub fn from_snapshot(snapshot: ChatFeedSnapshot) -> Self {
        Self {
            cursor: snapshot.cursor,
            state: log::State::from_snapshot(snapshot),
        }
    }
    pub fn cursor(&self) -> ChatCursor {
        self.cursor
    }
    /// Advance a directory projection to the prefix available at the start of this scan.
    pub(crate) fn refresh(&mut self, store: &ChatStore) -> Result<()> {
        if self.state.needs_controller() {
            store.settle_abandoned(self.cursor.chat)?;
        }
        let mut file = File::open(store.path(self.cursor.chat))?;
        let length = file.metadata()?.len();
        while self.cursor.offset < length {
            let batch = log::with_lock(&mut file, false, |f| read_batch(f, self.cursor))?;
            if batch.is_empty() {
                break;
            }
            for event in batch {
                self.apply(event)?;
            }
        }
        Ok(())
    }
    pub(crate) fn needs_controller(&self) -> bool {
        self.state.needs_controller()
    }
    pub fn pending_requests(&self) -> &[crate::PendingRequest] {
        &self.state.pending_requests
    }
    pub fn orphaned_questions(&self) -> &[crate::OrphanedQuestion] {
        &self.state.orphaned_questions
    }
    pub fn manual_compaction(&self) -> Option<&crate::ManualCompactionReport> {
        self.state.manual_compaction.as_ref()
    }
    pub fn last_compaction(&self) -> Option<&crate::CompactionReport> {
        self.state.last_compaction.as_ref()
    }
    pub fn usage(&self) -> &[crate::UsageSnapshot] {
        &self.state.usage
    }
    pub fn context_usage(&self) -> Option<&crate::ContextUsage> {
        self.state.context_usage.as_ref()
    }
    pub fn notifications(&self) -> &[super::ChatNotification] {
        &self.state.notifications
    }
    pub fn background_working(&self) -> bool {
        self.state.tasks.iter().any(crate::NativeTask::blocking_background)
    }
    pub fn tasks(&self) -> &[crate::NativeTask] {
        &self.state.tasks
    }
    pub fn goal(&self) -> Option<Option<&crate::GoalSnapshot>> {
        self.state.goal.as_ref().map(Option::as_ref)
    }
    pub fn chat(&self) -> &ChatInfo {
        &self.state.info
    }
    pub fn rewinds(&self) -> &[super::ContextRewind] {
        &self.state.rewinds
    }
    pub fn handoffs(&self) -> &[super::Handoff] {
        &self.state.handoffs
    }
    pub fn turns(&self) -> &[ChatTurn] {
        &self.state.turns
    }
    /// Returns false for an already applied event. An out-of-order gap is an error.
    pub fn apply(&mut self, event: ChatEvent) -> Result<bool> {
        if !event.follows(self.cursor)? {
            return Ok(false);
        }
        self.state.apply(event.update)?;
        self.cursor = event.cursor;
        Ok(true)
    }
}

pub struct ChatFeed {
    store: ChatStore,
    may_be_running: bool,
    may_have_background: bool,
    may_have_requests: bool,
    may_have_compaction: bool,
    may_have_notification: bool,
    may_have_steering: bool,
    file: Arc<Mutex<File>>,
    info: ChatInfo,
    cursor: ChatCursor,
    ready: VecDeque<ChatEvent>,
}

impl ChatStore {
    /// Snapshot and cursor describe the same complete prefix of the durable journal.
    pub fn feed_snapshot(&self, chat: Uuid) -> Result<ChatFeedSnapshot> {
        let mut file = File::open(self.path(chat))?;
        let (mut state, mut offset) = log::read(&mut file, chat)?;
        if state.needs_controller() && !self.settle_abandoned(chat)? {
            (state, offset) = log::read(&mut file, chat)?;
        }
        Ok(ChatFeedSnapshot {
            cursor: ChatCursor { chat, offset },
            stream: state.stream,
            history: self.history_from_state(chat, state)?,
        })
    }
    /// No cursor replays the chat from creation. Observers never launch the provider.
    pub fn follow(&self, chat: Uuid, after: Option<ChatCursor>) -> Result<ChatFeed> {
        let mut file = File::open(self.path(chat))?;
        let length = log::with_lock(&mut file, false, |f| Ok(f.metadata()?.len()))?;
        let (mut state, start) = log::parse_header(&mut BufReader::new(&mut file), Some(chat))?;
        let cursor = after.unwrap_or(ChatCursor {
            chat,
            offset: start,
        });
        if cursor.chat != chat || cursor.offset < start || cursor.offset > length {
            return Err(Error::InvalidCursor);
        }
        file.seek(SeekFrom::Start(cursor.offset - 1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            return Err(Error::InvalidCursor);
        }
        if cursor.offset > start {
            file.seek(SeekFrom::Start(start))?;
            let prefix = BufReader::new((&mut file).take(cursor.offset - start));
            log::read_info_changes(prefix, &mut state.info)?;
        }
        Ok(ChatFeed {
            store: self.clone(),
            may_be_running: true,
            may_have_background: true,
            may_have_requests: true,
            may_have_compaction: true,
            may_have_notification: true,
            may_have_steering: state.info.tracks_steering_receipts(),
            file: Arc::new(Mutex::new(file)),
            info: state.info,
            cursor,
            ready: VecDeque::new(),
        })
    }
    // Return true while a real controller still owns the session. Publish one durable
    // interruption even when several readers notice a killed process at the same time.
    pub(super) fn settle_abandoned(&self, chat: Uuid) -> Result<bool> {
        let lease = File::open(self.dir.join(format!("{chat}.lock")))?;
        let has_controller = || -> Result<bool> {
            match lease.try_lock_shared() {
                Ok(()) => {
                    lease.unlock()?;
                    Ok(false)
                }
                Err(std::fs::TryLockError::WouldBlock) => Ok(true),
                Err(std::fs::TryLockError::Error(err)) => Err(err.into()),
            }
        };
        // A live snapshot only reads committed records. Other readers may
        // still hold a shared journal lock while the controller is active.
        if has_controller()? {
            return Ok(true);
        }
        let mut file = match File::options().read(true).write(true).open(self.path(chat)) {
            Ok(file) => file,
            // Read-only mounts can follow all durable events. A writable observer or
            // the next controller will publish the interrupted turn in that store.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        };
        log::with_lock(&mut file, true, |file| {
            // A new controller may have opened while we waited for the journal.
            if has_controller()? {
                return Ok(true);
            }
            let (mut state, valid) = log::read_locked(file, chat)?;
            if state.needs_controller() {
                file.set_len(valid)?;
                file.seek(SeekFrom::Start(valid))?;
            }
            let tasks = crate::tasks::background::controller_lost(&state.tasks);
            if !tasks.is_empty() {
                log::append_locked(file, &ChatUpdate::TasksChanged { tasks })?;
            }
            if let Some(report) = state.abandoned_compaction() {
                log::append_locked(file, &ChatUpdate::ManualCompaction { report })?;
            }
            if state.waiting_steering() {
                log::append_locked(file, &ChatUpdate::SteeringUnconfirmed)?;
            }
            if state.observing_notifications() {
                log::append_locked(file, &ChatUpdate::NotificationsInterrupted)?;
            }
            for (turn, error) in state.unconfirmed_delivery_failures(super::now()) {
                log::milestones::orphan(
                    file,
                    &mut state,
                    ChatUpdate::RequestDeliveryFailed { turn, error },
                )?;
            }
            if matches!(self.stop_fence(chat, false), Err(Error::Busy)) {
                if !state.orphaned_questions.is_empty() {
                    log::milestones::orphan(
                        file,
                        &mut state,
                        ChatUpdate::OrphanedQuestionsChanged { questions: vec![] },
                    )?;
                }
            } else {
                let orphans = state.recoverable_orphans();
                if !orphans.is_empty() && state.orphaned_questions != orphans {
                    log::milestones::orphan(
                        file,
                        &mut state,
                        ChatUpdate::OrphanedQuestionsChanged { questions: orphans },
                    )?;
                }
            }
            if !state.pending_requests.is_empty() {
                log::milestones::orphan(file,&mut state,ChatUpdate::RequestsChanged { requests: vec![] })?;
            }
            if let Some(id) = state.active() {
                log::milestones::orphan(
                    file, &mut state,
                    ChatUpdate::End {
                        id,
                        status: super::TurnStatus::Interrupted,
                    },
                )?;
            }
            Ok(false)
        })
    }
}
impl ChatFeed {
    pub fn info(&self) -> &ChatInfo {
        &self.info
    }
    pub fn cursor(&self) -> ChatCursor {
        self.cursor
    }
    /// Cancel-safe. Cursor advancement occurs only when the event is returned.
    pub async fn recv(&mut self) -> Result<ChatEvent> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                if self.info.tracks_steering_receipts()
                    && let ChatUpdate::Frame { frame, .. } | ChatUpdate::Notification { frame, .. } = &event.update
                    && frame.edits.iter().any(|e| matches!(e, super::ChatEdit::SteeringInput { status, .. } if self.info.awaits_steering_receipt(*status))) {
                    self.may_have_steering = true;
                }
                match &event.update {
                    ChatUpdate::ContextRewound { binding, .. } => {
                        if let Some(target) = binding.as_ref().and_then(|h| h.target.as_ref()) {
                            target.activate(&mut self.info);
                        }
                        self.may_be_running = false;
                        self.may_have_background = false;
                        self.may_have_requests = false;
                        self.may_have_compaction = false;
                        self.may_have_notification = false;
                        self.may_have_steering = false;
                    }
                    ChatUpdate::HandoffReady { target, .. } => {
                        target.activate(&mut self.info);
                    }
                    ChatUpdate::HandoffTargetReopened { native_id, .. } => {
                        self.info.native_id.clone_from(native_id);
                    }
                    ChatUpdate::RequestsChanged { requests } => {
                        self.may_have_requests = !requests.is_empty()
                    }
                    ChatUpdate::RequestAnswerPending { .. } => {
                        self.may_have_requests = true;
                    }
                    ChatUpdate::TasksChanged { tasks } => {
                        self.may_have_background |= tasks.iter().any(crate::NativeTask::pending_background);
                    }
                    ChatUpdate::ManualCompaction { report } => {
                        self.may_have_compaction = report.status.is_pending()
                    }
                    ChatUpdate::Notification { frame, .. } if frame.source.path.is_empty() => {
                        self.may_have_notification = !frame.edits.iter().any(|e| {
                            matches!(
                                e,
                                super::ChatEdit::Activity {
                                    event: crate::Event::TurnEnded { .. }
                                }
                            )
                        });
                    }
                    ChatUpdate::NotificationsInterrupted => self.may_have_notification = false,
                    ChatUpdate::SteeringUnconfirmed => self.may_have_steering = false,
                    ChatUpdate::Opened { .. } | ChatUpdate::MorphReattached { .. } => {
                        if let ChatUpdate::Opened { antigravity_profile, .. } = &event.update {
                            self.info.antigravity_profile.clone_from(antigravity_profile);
                        }
                        self.may_be_running =
                            matches!(event.update, ChatUpdate::MorphReattached { .. });
                        self.may_have_notification = false;
                        self.may_have_steering = false;
                        self.may_have_background = false;
                        self.may_have_requests = false;
                        self.may_have_compaction = false;
                    }
                    ChatUpdate::Frame { frame, .. }
                        if frame.edits.iter().any(|e| {
                            matches!(
                                e,
                                super::ChatEdit::Activity {
                                    event: crate::Event::NotificationJoined { .. }
                                }
                            )
                        }) =>
                    {
                        self.may_have_notification = false;
                        self.may_be_running = true;
                    }
                    ChatUpdate::NativeBegin { .. }
                    | ChatUpdate::Begin { .. }
                    | ChatUpdate::Frame { .. }
                    | ChatUpdate::InputSent { .. } => self.may_be_running = true,
                    ChatUpdate::End { .. } | ChatUpdate::Captured { .. } => {
                        self.may_be_running = false
                    }
                    _ => {}
                }
                self.cursor = event.cursor;
                return Ok(event);
            }
            let file = self.file.clone();
            let cursor = self.cursor;
            let store = self.store.clone();
            let check_owner = self.may_be_running
                || self.may_have_background
                || self.may_have_requests
                || self.may_have_compaction
                || self.may_have_notification
                || self.may_have_steering;
            (self.ready, self.may_be_running) =
                tokio::task::spawn_blocking(move || -> Result<_> {
                    store.require_available_id(cursor.chat)?;
                    let events = log::with_lock(&mut file.lock().unwrap(), false, |f| {
                        read_batch(f, cursor)
                    })?;
                    let running = if events.is_empty() && check_owner {
                        store.settle_abandoned(cursor.chat)?
                    } else {
                        check_owner
                    };
                    Ok((events, running))
                })
                .await
                .map_err(|e| Error::Closed(e.to_string()))??;
            if !self.may_be_running && self.ready.is_empty() {
                self.may_have_background = false;
                self.may_have_notification = false;
                self.may_have_steering = false;
                self.may_have_requests = false;
                self.may_have_compaction = false;
            }
            if self.ready.is_empty() {
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
    }
}
fn read_batch(file: &mut File, mut previous: ChatCursor) -> Result<VecDeque<ChatEvent>> {
    let length = file.metadata()?.len();
    if length < previous.offset {
        return Err(Error::InvalidCursor);
    }
    let mut events = VecDeque::new();
    if length == previous.offset {
        return Ok(events);
    }
    file.seek(SeekFrom::Start(previous.offset))?;
    let start = previous.offset;
    let mut reader = BufReader::new(file.take(length - start));
    let mut line = Vec::new();
    while events.len() < 64 && previous.offset - start < 256 * 1024 {
        line.clear();
        reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            break;
        }
        let update = serde_json::from_slice(&line)?;
        let cursor = ChatCursor {
            offset: previous.offset + line.len() as u64,
            ..previous
        };
        events.push_back(ChatEvent {
            previous,
            cursor,
            update,
        });
        previous = cursor;
    }
    Ok(events)
}

type Endpoint = (ChatStore, Uuid);
/// Read-only SSE and snapshot endpoints for one saved chat. Mount inside the host's authenticated router.
pub fn event_router(store: ChatStore, chat: Uuid) -> Router {
    Router::new()
        .route("/snapshot", get(snapshot))
        .route("/events", get(events))
        .with_state((store, chat))
}
fn response_error(e: Error) -> Response {
    let code = match &e {
        Error::InvalidCursor => StatusCode::BAD_REQUEST,
        Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, e.to_string()).into_response()
}
async fn snapshot(State((store, chat)): State<Endpoint>) -> Response {
    match tokio::task::spawn_blocking(move || store.feed_snapshot(chat)).await {
        Ok(Ok(snapshot)) => ([("cache-control", "no-store")], Json(snapshot)).into_response(),
        Ok(Err(e)) => response_error(e),
        Err(e) => response_error(Error::Closed(e.to_string())),
    }
}
#[derive(Deserialize)]
struct Replay {
    after: Option<ChatCursor>,
}
async fn events(State((store, chat)): State<Endpoint>, headers: HeaderMap, uri: Uri) -> Response {
    event_response(store, chat, headers, uri).await
}
pub(crate) async fn event_response(
    store: ChatStore,
    chat: Uuid,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let after = match headers.get("last-event-id") {
        Some(id) if !id.is_empty() => match id.to_str().ok().and_then(|s| s.parse().ok()) {
            Some(id) => Some(id),
            None => return response_error(Error::InvalidCursor),
        },
        _ => match Query::<Replay>::try_from_uri(&uri) {
            Ok(query) => query.after,
            Err(_) => return response_error(Error::InvalidCursor),
        },
    };
    let feed = match tokio::task::spawn_blocking(move || store.follow(chat, after)).await {
        Ok(Ok(feed)) => feed,
        Ok(Err(e)) => return response_error(e),
        Err(e) => return response_error(Error::Closed(e.to_string())),
    };
    let stream = stream::try_unfold(feed, |mut feed| async move {
        let event = feed.recv().await?;
        let packet = SseEvent::default()
            .id(event.cursor.to_string())
            .data(serde_json::to_string(&event)?);
        Ok::<_, Error>(Some((packet, feed)))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
