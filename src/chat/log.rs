mod acp_children;
mod claude;
pub(crate) mod pi;
use super::{
    ChatInfo, ChatTurn, Checkpoint, CheckpointPoint, FileChange, TurnStatus,
    timeline::{Frame, MessageIds},
};
use crate::{Direction, Error, Event, Protocol, Record, RecordKind, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

const CHAT_FORMAT: &str = "kar-chat/1";

#[derive(Serialize, Deserialize)]
struct Header {
    format: String,
    chat: ChatInfo,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatUpdate {
    Inherited {
        turns: Vec<ChatTurn>,
    },
    Compaction {
        report: crate::CompactionReport,
    },
    ManualCompaction {
        report: crate::ManualCompactionReport,
    },
    RequestsChanged {
        requests: Vec<crate::PendingRequest>,
    },
    UsageChanged {
        reading: crate::UsageSnapshot,
    },
    ContextChanged {
        usage: crate::ContextUsage,
    },
    TasksChanged {
        tasks: Vec<crate::NativeTask>,
    },
    GoalChanged {
        goal: Option<crate::GoalSnapshot>,
    },
    Opened {
        stream: Uuid,
    },
    /// A new controller is observing the same native Morph task and local turn.
    MorphReattached {
        stream: Uuid,
        id: Uuid,
        task: String,
        open_tools: Vec<String>,
    },
    Begin {
        id: Uuid,
        at: u64,
        text: String,
        #[serde(default)]
        before_tree: Option<String>,
    },
    NativeBegin {
        id: Uuid,
        native_turn: String,
        at: u64,
        before_tree: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steered_from: Option<Uuid>,
    },
    Notification {
        id: Uuid,
        at: u64,
        frame: Frame,
    },
    NotificationsInterrupted,
    SteeringUnconfirmed,
    ClaudeFollowUp {
        from: Uuid,
        inputs: Vec<Uuid>,
        at: u64,
        frame: Frame,
    },
    InputSent {
        id: Uuid,
    },
    PiInput {
        id: Uuid,
        timestamp_ms: u64,
    },
    Captured {
        id: Uuid,
        tree: String,
        files: Vec<FileChange>,
    },
    RestoreStarted {
        target: CheckpointPoint,
    },
    Restored {
        target: CheckpointPoint,
    },
    Frame {
        id: Uuid,
        frame: Frame,
    },
    ChildSession {
        id: Uuid,
        parent: String,
        child: String,
        finished: bool,
    },
    TurnProcessCleanup {
        id: Uuid,
        cursor: crate::Cursor,
        cleanup: crate::TurnProcessCleanup,
    },
    LateTool {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        elided: bool,
        id: Uuid,
        cursor: crate::Cursor,
        native_turn: String,
        source: crate::observe::Agent,
        events: Vec<Event>,
    },
    Reconciled {
        id: Uuid,
        frame: Frame,
    },
    End {
        id: Uuid,
        status: TurnStatus,
    },
}
pub(super) struct State {
    pub notifications: Vec<super::ChatNotification>,
    pub info: ChatInfo,
    pub turns: Vec<ChatTurn>,
    pub goal: Option<Option<crate::GoalSnapshot>>,
    pub tasks: Vec<crate::NativeTask>,
    pub pending_requests: Vec<crate::PendingRequest>,
    pub manual_compaction: Option<crate::ManualCompactionReport>,
    pub last_compaction: Option<crate::CompactionReport>,
    pub usage: Vec<crate::UsageSnapshot>,
    pub context_usage: Option<crate::ContextUsage>,
    pub position: Option<CheckpointPoint>,
    pub restoring: Option<CheckpointPoint>,
    morph: Option<crate::community::morph::recovery::Saved>,
    ids: HashMap<Uuid, usize>,
    seen: HashMap<Uuid, u64>,
    pub(super) stream: Option<Uuid>,
}
impl State {
    pub(super) fn unfinished_turn(&self) -> Option<&ChatTurn> {
        self.turns
            .last()
            .filter(|t| matches!(t.status, TurnStatus::Running | TurnStatus::Interrupted))
    }
    fn new(info: ChatInfo) -> Self {
        Self {
            info,
            notifications: vec![],
            turns: vec![],
            goal: None,
            tasks: vec![],
            pending_requests: vec![],
            manual_compaction: None,
            last_compaction: None,
            usage: vec![],
            context_usage: None,
            position: None,
            restoring: None,
            morph: None,
            ids: HashMap::new(),
            seen: HashMap::new(),
            stream: None,
        }
    }
    pub(super) fn from_snapshot(snapshot: super::ChatFeedSnapshot) -> Self {
        let history = snapshot.history;
        let ids = history
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, i))
            .collect();
        let seen = history
            .cursor
            .into_iter()
            .map(|c| (c.stream_id, c.after_seq))
            .collect();
        Self {
            info: history.chat,
            notifications: history.notifications,
            turns: history.turns,
            goal: history.goal,
            tasks: history.tasks,
            pending_requests: history.pending_requests,
            manual_compaction: history.manual_compaction,
            last_compaction: history.last_compaction,
            usage: history.usage,
            context_usage: history.context_usage,
            position: history.workspace_position,
            restoring: history.pending_rollback,
            morph: None,
            ids,
            seen,
            stream: snapshot.stream,
        }
    }
    pub fn find(&self, id: Uuid) -> Option<&ChatTurn> {
        self.ids.get(&id).map(|i| &self.turns[*i])
    }
    pub fn active(&self) -> Option<Uuid> {
        self.turns
            .last()
            .filter(|t| t.status == TurnStatus::Running)
            .map(|t| t.id)
    }
    pub fn compacting(&self) -> bool {
        self.manual_compaction
            .as_ref()
            .is_some_and(|r| r.status.is_pending())
    }
    pub fn needs_controller(&self) -> bool {
        self.active().is_some()
            || !self.pending_requests.is_empty()
            || self.compacting()
            || self.observing_notifications()
            || self.waiting_steering()
    }
    pub(super) fn check_new_prompt_id(&self, id: Uuid) -> Result<()> {
        if self
            .turns
            .iter()
            .chain(self.notifications.iter().map(|n| &n.activity))
            .any(|t| t.steered_inputs.contains_key(&id))
        {
            return Err(Error::Refused(
                "request id already belongs to a steering input".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn waiting_steering(&self) -> bool {
        self.info.tracks_steering_receipts()
            && self
                .turns
                .iter()
                .chain(self.notifications.iter().map(|n| &n.activity))
                .any(|t| {
                    t.steered_inputs
                        .values()
                        .any(|s| self.info.awaits_steering_receipt(*s))
                })
    }
    pub(super) fn unconfirm_steering(&mut self) {
        if self.info.tracks_steering_receipts() {
            for turn in self
                .turns
                .iter_mut()
                .chain(self.notifications.iter_mut().map(|n| &mut n.activity))
            {
                for status in turn
                    .steered_inputs
                    .values_mut()
                    .filter(|s| self.info.awaits_steering_receipt(**s))
                {
                    *status = crate::SteerStatus::Unconfirmed;
                }
            }
        }
    }
    pub(super) fn observing_notifications(&self) -> bool {
        self.notifications
            .last()
            .is_some_and(|n| n.activity.status == TurnStatus::Running)
    }
    pub(super) fn interrupt_notifications(&mut self) {
        if let Some(last) = self
            .notifications
            .last_mut()
            .filter(|n| n.activity.status == TurnStatus::Running)
        {
            last.activity
                .finish(TurnStatus::Interrupted, &self.info.native_id);
        }
    }
    pub fn abandoned_compaction(&self) -> Option<crate::ManualCompactionReport> {
        self.manual_compaction
            .as_ref()
            .filter(|r| r.status.is_pending())
            .map(|r| r.unconfirmed(crate::CompactionUnconfirmed::ControllerExited))
    }
    pub fn cursor(&self) -> Option<crate::Cursor> {
        let stream_id = self.stream?;
        Some(crate::Cursor {
            stream_id,
            after_seq: *self.seen.get(&stream_id)?,
        })
    }
    fn begin_turn(
        &mut self,
        id: Uuid,
        at: u64,
        text: Option<String>,
        native_turn: Option<String>,
        before_tree: Option<String>,
    ) -> Result<()> {
        if self.active().is_some() || self.ids.contains_key(&id) {
            return Err(Error::Protocol(
                "duplicate or overlapping stored turn".into(),
            ));
        }
        self.ids.insert(id, self.turns.len());
        let mut turn = ChatTurn::new(id, at, &self.info.native_id, text);
        turn.native_turn_id = native_turn;
        if let Some(before_tree) = before_tree {
            turn.checkpoint = Some(Checkpoint {
                parent: self.position,
                before_tree,
                after_tree: None,
                files: vec![],
            });
            self.position = Some(CheckpointPoint {
                turn: id,
                before: true,
            });
        }
        self.turns.push(turn);
        Ok(())
    }
    fn activity_mut(&mut self, id: Uuid) -> Option<&mut ChatTurn> {
        if let Some(index) = self.ids.get(&id) {
            self.turns.get_mut(*index)
        } else {
            self.notifications
                .iter_mut()
                .find(|n| n.activity.id == id)
                .map(|n| &mut n.activity)
        }
    }
    fn take_steering_message(&mut self, from: Uuid, id: Uuid) -> Result<super::TimelineItem> {
        let source = self
            .activity_mut(from)
            .ok_or_else(|| Error::Protocol("unknown steering origin".into()))?;
        let key = id.to_string();
        let index = source
            .items
            .iter()
            .position(|i| {
                matches!(&i.item, super::Item::Message {
            role: super::Role::User, provider_id: Some(provider), ..
        } if provider == &key)
            })
            .ok_or_else(|| Error::Protocol("missing submitted steering input".into()))?;
        Ok(source.items.remove(index))
    }
    fn move_steering_messages(&mut self, target: Uuid, frame: &Frame) -> Result<()> {
        for edit in &frame.edits {
            if let super::ChatEdit::Activity {
                event: Event::SteeringMoved { input_id, from, to },
            } = edit
                && *to == target
            {
                let message = self.take_steering_message(*from, *input_id)?;
                let activity = self
                    .activity_mut(target)
                    .ok_or_else(|| Error::Protocol("unknown steering destination".into()))?;
                activity
                    .steered_inputs
                    .insert(*input_id, crate::SteerStatus::Accepted);
                activity.items.push(message);
            }
        }
        Ok(())
    }
    pub(super) fn apply(&mut self, row: ChatUpdate) -> Result<()> {
        match row {
            ChatUpdate::NotificationsInterrupted => self.interrupt_notifications(),
            ChatUpdate::SteeringUnconfirmed => self.unconfirm_steering(),
            ChatUpdate::Notification { id, at, frame } => {
                if !matches!(self.info.protocol, Protocol::Claude | Protocol::Pi)
                    || self.stream != Some(frame.cursor.stream_id)
                {
                    return Err(Error::Protocol(
                        "notification outside its native stream".into(),
                    ));
                }
                if self
                    .seen
                    .get(&frame.cursor.stream_id)
                    .is_some_and(|s| *s >= frame.cursor.after_seq)
                {
                    return Ok(());
                }
                let known = self.notifications.iter().rposition(|n| n.activity.id == id);
                let index = match known {
                    Some(index) => index,
                    None => {
                        self.interrupt_notifications();
                        self.notifications.push(super::ChatNotification {
                            activity: ChatTurn::new(id, at, &self.info.native_id, None),
                            joined_input: None,
                        });
                        self.notifications.len() - 1
                    }
                };
                self.move_steering_messages(id, &frame)?;
                let notice = &mut self.notifications[index];
                notice.activity.apply(&frame, &self.info.native_id);
                self.seen
                    .insert(frame.cursor.stream_id, frame.cursor.after_seq);
            }
            ChatUpdate::Inherited { turns } => {
                let origin = self
                    .info
                    .forked_from
                    .as_ref()
                    .ok_or_else(|| Error::Protocol("missing fork origin".into()))?;
                if !self.turns.is_empty()
                    || self.stream.is_some()
                    || turns.last().map(|t| t.id) != Some(origin.turn)
                {
                    return Err(Error::Protocol("invalid inherited turn prefix".into()));
                }
                for (i, t) in turns.iter().enumerate() {
                    if (t.items.is_empty() && !t.native_continuation)
                        || t.status == TurnStatus::Running
                        || self.ids.insert(t.id, i).is_some()
                    {
                        return Err(Error::Protocol("invalid inherited turn".into()));
                    }
                }
                self.position = Some(CheckpointPoint {
                    turn: origin.turn,
                    before: false,
                });
                self.turns = turns;
            }
            ChatUpdate::RequestsChanged { requests } => {
                if requests
                    .iter()
                    .any(|r| Some(r.cursor.stream_id) != self.stream)
                {
                    return Err(Error::InvalidCursor);
                }
                self.pending_requests = requests;
            }
            ChatUpdate::ManualCompaction { report } => {
                if self.stream != Some(report.request.stream_id) {
                    return Err(Error::InvalidCursor);
                }
                self.manual_compaction = Some(report);
            }
            ChatUpdate::Compaction { report } => self.last_compaction = Some(report),
            ChatUpdate::UsageChanged { reading } => {
                if (reading.usage.total.is_none() && reading.children.is_empty())
                    || self.stream != Some(reading.cursor.stream_id)
                {
                    return Err(Error::InvalidCursor);
                }
                if let Some(prior) = self
                    .usage
                    .last_mut()
                    .filter(|v| v.cursor.stream_id == reading.cursor.stream_id)
                {
                    if reading.cursor.after_seq <= prior.cursor.after_seq {
                        return Err(Error::InvalidCursor);
                    }
                    *prior = reading;
                } else {
                    self.usage.push(reading);
                }
            }
            ChatUpdate::ContextChanged { usage } => self.context_usage = Some(usage),
            ChatUpdate::TasksChanged { tasks } => crate::tasks::merge(&mut self.tasks, tasks),
            ChatUpdate::GoalChanged { goal } => self.goal = Some(goal),
            ChatUpdate::MorphReattached {
                stream,
                id,
                task,
                open_tools,
            } => {
                if self.info.protocol != Protocol::Morph || self.stream == Some(stream) {
                    return Err(Error::Protocol("invalid Morph reattachment stream".into()));
                }
                let turn = self
                    .turns
                    .last_mut()
                    .filter(|turn| {
                        turn.id == id
                            && turn.input_sent
                            && turn.native_turn_id.as_deref() == Some(task.as_str())
                            && matches!(turn.status, TurnStatus::Running | TurnStatus::Interrupted)
                    })
                    .ok_or_else(|| {
                        Error::Protocol(
                            "Morph reattachment does not match the unfinished task".into(),
                        )
                    })?;
                turn.status = TurnStatus::Running;
                for tool in open_tools {
                    if let Some(activity) =
                        turn.items
                            .iter_mut()
                            .rev()
                            .find_map(|item| match &mut item.item {
                                super::Item::Tool { id, activity }
                                    if id == &tool
                                        && item.source.path.is_empty()
                                        && item.source.session_id.as_deref()
                                            == Some(&self.info.native_id) =>
                                {
                                    Some(activity)
                                }
                                _ => None,
                            })
                    {
                        activity.finished = false;
                    }
                }
                self.pending_requests.clear();
                self.stream = Some(stream);
            }
            ChatUpdate::Opened { stream } => {
                if self.active().is_some() {
                    return Err(Error::Protocol(
                        "chat stream opened with unsettled turn".into(),
                    ));
                }
                self.pending_requests.clear();
                self.stream = Some(stream);
            }
            ChatUpdate::ClaudeFollowUp {
                from,
                inputs,
                at,
                frame,
            } => {
                self.claude_group(from, &inputs, at)
                    .ok_or_else(|| {
                        Error::Protocol(
                            "Claude follow-up does not match its submitted inputs".into(),
                        )
                    })?
                    .apply(self, &frame)?;
            }
            ChatUpdate::InputSent { id } => {
                let index = *self
                    .ids
                    .get(&id)
                    .ok_or_else(|| Error::Protocol("unknown sent input".into()))?;
                self.turns[index].input_sent = true;
            }
            ChatUpdate::Begin {
                id,
                at,
                text,
                before_tree,
            } => {
                self.begin_turn(id, at, Some(text), None, before_tree)?;
            }
            ChatUpdate::NativeBegin {
                id,
                native_turn,
                at,
                before_tree,
                steered_from,
            } => {
                self.begin_turn(id, at, None, Some(native_turn), before_tree)?;
                if let Some(from) = steered_from {
                    let input = self.take_steering_message(from, id)?;
                    let turn = self.turns.last_mut().expect("just began turn");
                    turn.native_continuation = false;
                    turn.input_sent = true;
                    turn.steered_from = Some(from);
                    turn.items.push(input);
                }
            }
            ChatUpdate::Captured { id, tree, files } => {
                let cp = self
                    .turns
                    .last_mut()
                    .filter(|t| t.id == id)
                    .and_then(|t| t.checkpoint.as_mut())
                    .filter(|cp| cp.after_tree.is_none())
                    .ok_or_else(|| Error::Protocol("unexpected final checkpoint".into()))?;
                cp.after_tree = Some(tree);
                cp.files = files;
                self.position = Some(CheckpointPoint {
                    turn: id,
                    before: false,
                });
            }
            ChatUpdate::RestoreStarted { target } => self.restoring = Some(target),
            ChatUpdate::Restored { target } => {
                if self.restoring != Some(target) {
                    return Err(Error::Protocol("unexpected rollback completion".into()));
                }
                self.position = Some(target);
                self.restoring = None;
            }
            ChatUpdate::ChildSession {
                id,
                parent,
                child,
                finished,
            } => {
                let index = *self
                    .ids
                    .get(&id)
                    .ok_or_else(|| Error::Protocol("unknown child owner turn".into()))?;
                let turn = &mut self.turns[index];
                if !matches!(self.info.runtime.as_str(), "cursor" | "opencode")
                    || child.is_empty()
                    || child == parent
                    || child == self.info.native_id
                    || (parent != self.info.native_id && !turn.child_sessions.contains_key(&parent))
                    || turn
                        .child_sessions
                        .get(&child)
                        .is_some_and(|prior| *prior != parent)
                {
                    return Err(Error::Protocol("invalid child session binding".into()));
                }
                if finished {
                    if !turn.child_sessions.contains_key(&child) {
                        return Err(Error::Protocol("unbound child completion".into()));
                    }
                    for item in &mut turn.items {
                        if item.source.session_id.as_ref() == Some(&child) {
                            match &mut item.item {
                                super::Item::Message { complete, .. } => *complete = true,
                                super::Item::Tool { activity, .. } => activity.finished = true,
                                _ => {}
                            }
                        }
                    }
                } else {
                    turn.child_sessions.insert(child, parent);
                }
            }
            ChatUpdate::Frame { id, frame } => {
                if self.stream != Some(frame.cursor.stream_id) {
                    return Err(Error::Protocol(
                        "chat frame belongs to another stream".into(),
                    ));
                }
                if self
                    .seen
                    .get(&frame.cursor.stream_id)
                    .is_some_and(|n| *n >= frame.cursor.after_seq)
                {
                    return Ok(());
                }
                let index = *self
                    .ids
                    .get(&id)
                    .ok_or_else(|| Error::Protocol("unknown chat frame turn".into()))?;
                if self.active() != Some(id) {
                    let turn = &self.turns[index];
                    let child = frame
                        .source
                        .session_id
                        .as_ref()
                        .is_some_and(|id| turn.child_sessions.contains_key(id));
                    let known_tools = !frame.edits.is_empty() && frame.edits.iter().all(|edit|matches!(edit, super::ChatEdit::Activity {event} if turn.owns_tool_update(&frame.source,event)));
                    let steering_update = frame.source.path.is_empty()
                        && frame.source.session_id.as_deref() == Some(&self.info.native_id)
                        && matches!(
                            frame.edits.as_slice(),
                            [super::ChatEdit::SteeringInput { .. }]
                        );
                    let allowed = (self.info.runtime == "opencode"
                        && (child || known_tools || steering_update))
                        || (self.info.runtime == "cursor" && (child || known_tools))
                        || (self.info.protocol == Protocol::Claude && steering_update)
                        || ((self.info.runtime == "grok" || self.info.protocol == Protocol::Pi) && steering_update
                            && frame.edits.iter().any(|e| matches!(e, super::ChatEdit::SteeringInput {id, ..} if turn.steered_inputs.contains_key(id))));
                    let late_followup_child =
                        turn.steered_from.is_some() && !frame.source.path.is_empty();
                    if frame.native_turn.is_some() || !(allowed || late_followup_child) {
                        return Err(Error::Protocol("chat frame outside its turn".into()));
                    }
                }
                self.seen
                    .insert(frame.cursor.stream_id, frame.cursor.after_seq);
                self.move_steering_messages(id, &frame)?;
                let mode = if self.info.protocol == Protocol::Dsh {
                    super::timeline::ToolEvents::DshStream
                } else {
                    super::timeline::ToolEvents::Live
                };
                self.turns[index].apply_edits(&frame, &self.info.native_id, mode);
                if self.info.protocol == Protocol::Morph {
                    crate::community::morph::recovery::Saved::record(&mut self.morph, id, &frame);
                }
                if let Some(notice) = self
                    .notifications
                    .last_mut()
                    .filter(|n| n.activity.status == TurnStatus::Running)
                {
                    if frame.edits.iter().any(|edit| matches!(edit, super::ChatEdit::Activity { event: Event::NotificationJoined { id: group, input_id } } if *group == notice.activity.id && *input_id == id)) {
                        notice.joined_input = Some(id);
                    }
                    if notice.joined_input == Some(id)
                        && self.turns[index].status != TurnStatus::Running
                    {
                        notice
                            .activity
                            .finish(self.turns[index].status, &self.info.native_id);
                    }
                }
            }
            ChatUpdate::TurnProcessCleanup {
                id,
                cursor,
                cleanup,
            } => {
                if self.stream != Some(cursor.stream_id) {
                    return Err(Error::InvalidCursor);
                }
                if self
                    .seen
                    .get(&cursor.stream_id)
                    .is_some_and(|seq| *seq >= cursor.after_seq)
                {
                    return Ok(());
                }
                let turn = self
                    .ids
                    .get(&id)
                    .and_then(|i| self.turns.get_mut(*i))
                    .filter(|turn| turn.native_turn_id.as_deref() == Some(&cleanup.turn_id))
                    .ok_or_else(|| {
                        Error::Protocol("cleanup did not match its native turn".into())
                    })?;
                turn.process_cleanups.push(cleanup);
                self.seen.insert(cursor.stream_id, cursor.after_seq);
            }
            ChatUpdate::LateTool {
                elided,
                id,
                cursor,
                native_turn,
                source,
                events,
            } => {
                if self.stream != Some(cursor.stream_id) {
                    return Err(Error::InvalidCursor);
                }
                if self
                    .seen
                    .get(&cursor.stream_id)
                    .is_some_and(|last| *last >= cursor.after_seq)
                {
                    return Ok(());
                }
                let turn = self
                    .ids
                    .get(&id)
                    .and_then(|i| self.turns.get_mut(*i))
                    .filter(|t| {
                        t.status != TurnStatus::Running
                            && t.native_turn_id.as_ref() == Some(&native_turn)
                    })
                    .ok_or_else(|| {
                        Error::Protocol("late tool frame did not match its native turn".into())
                    })?;
                if events.is_empty() || !events.iter().all(|e| turn.owns_tool_update(&source, e)) {
                    return Err(Error::Protocol(
                        "late tool frame did not match its original call".into(),
                    ));
                }
                let frame = Frame {
                    morph_progress: None,
                    elided,
                    cursor: cursor.clone(),
                    native_turn: None,
                    source,
                    edits: events
                        .into_iter()
                        .map(|event| super::ChatEdit::Activity { event })
                        .collect(),
                };
                turn.apply(&frame, &self.info.native_id);
                self.seen.insert(cursor.stream_id, cursor.after_seq);
            }
            ChatUpdate::End { id, status } => {
                if self.active() != Some(id) || status == TurnStatus::Running {
                    return Err(Error::Protocol("invalid stored turn ending".into()));
                }
                self.turns
                    .last_mut()
                    .unwrap()
                    .finish(status, &self.info.native_id);
            }
            ChatUpdate::PiInput { id, timestamp_ms } => {
                if self.info.protocol != Protocol::Pi || self.active() != Some(id) {
                    return Err(Error::Protocol(
                        "Pi input receipt outside its active turn".into(),
                    ));
                }
                let turn = self.turns.last_mut().unwrap();
                if turn.pi_input_timestamp_ms.is_some() {
                    return Err(Error::Protocol("duplicate Pi input receipt".into()));
                }
                turn.pi_input_timestamp_ms = Some(timestamp_ms);
                turn.input_sent = true;
            }
            ChatUpdate::Reconciled { id, mut frame } => {
                let native_proves_input = match self.info.protocol {
                    Protocol::Dsh => true,
                    Protocol::KimiKap => {
                        frame.source.path.is_empty()
                            && frame.source.session_id.as_deref()==Some(self.info.native_id.as_str())
                            && frame.edits.iter().any(|edit|matches!(edit,
                                super::ChatEdit::Activity {event:Event::NativeInput{input_id,receipt:crate::InputReceipt::Started}} if *input_id==id))
                    }
                    Protocol::Acp if self.info.runtime == "opencode" => {
                        frame.source.path.is_empty()
                            && frame.source.session_id.as_deref()
                                == Some(self.info.native_id.as_str())
                            && self.find(id).is_some_and(|t| {
                                crate::providers::opencode_recovery::has_witness(
                                    t,
                                    &self.info.native_id,
                                )
                            })
                    }
                    Protocol::Pi => self
                        .find(id)
                        .is_some_and(|t| t.pi_input_timestamp_ms.is_some()),
                    Protocol::Claude => {
                        frame
                            .native_turn
                            .as_deref()
                            .and_then(|s| Uuid::parse_str(s).ok())
                            == Some(id)
                            && frame.source.path.is_empty()
                            && frame.source.session_id.as_deref() == Some(&self.info.native_id)
                            && self
                                .find(id)
                                .is_some_and(|t| t.input_sent && !t.native_continuation)
                    }
                    Protocol::Factory => {
                        frame
                            .native_turn
                            .as_deref()
                            .and_then(|s| Uuid::parse_str(s).ok())
                            == Some(id)
                    }
                    _ => false,
                };
                let last = self
                    .turns
                    .last_mut()
                    .filter(|t| {
                        t.id == id
                            && frame.native_turn.is_some()
                            && (t.native_turn_id == frame.native_turn
                                || (t.native_turn_id.is_none() && native_proves_input))
                            && matches!(t.status, TurnStatus::Running | TurnStatus::Interrupted)
                    })
                    .ok_or_else(|| {
                        Error::Protocol(
                            "native recovery does not match the interrupted turn".into(),
                        )
                    })?;
                if native_proves_input && !last.native_continuation {
                    last.input_sent = true;
                }
                if matches!(
                    self.info.protocol,
                    Protocol::Pi | Protocol::Codex | Protocol::KimiKap | Protocol::Factory
                ) {
                    // Older journals stored full native transcripts as individual start/end events.
                    super::timeline::snapshot_tools(&mut frame.edits, frame.elided);
                    last.apply_transcript(&frame, &self.info.native_id);
                    if self.info.protocol == Protocol::Factory
                        && last.status == TurnStatus::Interrupted
                    {
                        last.finish(TurnStatus::Interrupted, &self.info.native_id);
                    }
                } else if self.info.protocol == Protocol::Dsh {
                    last.apply_edits(
                        &frame,
                        &self.info.native_id,
                        super::timeline::ToolEvents::DshHistory,
                    );
                } else if self.info.protocol == Protocol::Claude {
                    last.apply_claude_history(&frame, &self.info.native_id);
                } else if self.info.protocol == Protocol::Acp && self.info.runtime == "opencode" {
                    last.apply_transcript(&frame, &self.info.native_id);
                } else {
                    last.apply(&frame, &self.info.native_id);
                }
            }
        }
        Ok(())
    }
}
pub(super) fn header(file: &mut File, info: &ChatInfo) -> Result<()> {
    append(
        file,
        &Header {
            format: CHAT_FORMAT.to_owned(),
            chat: info.clone(),
        },
    )
}
fn append(file: &mut File, row: &impl Serialize) -> Result<()> {
    with_lock(file, true, |file| append_locked(file, row))
}
pub(super) fn append_locked(file: &mut File, row: &impl Serialize) -> Result<()> {
    let mut data = serde_json::to_vec(row)?;
    data.push(b'\n');
    file.write_all(&data)?;
    file.sync_data()?;
    Ok(())
}
pub(super) fn with_lock<T>(
    file: &mut File,
    exclusive: bool,
    work: impl FnOnce(&mut File) -> Result<T>,
) -> Result<T> {
    if exclusive {
        file.lock()?;
    } else {
        file.lock_shared()?;
    }
    let result = work(file);
    file.unlock()?;
    result
}
pub(super) fn parse_header(reader: &mut impl BufRead, id: Option<Uuid>) -> Result<(State, u64)> {
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(Error::Protocol("incomplete chat header".into()));
    }
    let header: Header = serde_json::from_slice(&line)?;
    if header.format != CHAT_FORMAT || id.is_some_and(|id| header.chat.id != id) {
        return Err(Error::Protocol(
            "unsupported chat format or mismatched file identity".into(),
        ));
    }
    Ok((State::new(header.chat), line.len() as u64))
}
pub(super) fn info(file: File, id: Uuid) -> Result<ChatInfo> {
    Ok(parse_header(&mut BufReader::new(file), Some(id))?.0.info)
}
pub(super) fn staged_info(file: File) -> Result<ChatInfo> {
    Ok(parse_header(&mut BufReader::new(file), None)?.0.info)
}
pub(super) fn inherit(file: &mut File, turns: Vec<ChatTurn>) -> Result<()> {
    append(file, &ChatUpdate::Inherited { turns })
}
pub(super) fn read(file: &mut File, id: Uuid) -> Result<(State, u64)> {
    with_lock(file, false, |file| read_locked(file, id))
}
pub(super) fn read_locked(file: &mut File, id: Uuid) -> Result<(State, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(len));
    let (mut state, mut valid) = parse_header(&mut reader, Some(id))?;
    let mut line = Vec::new();
    loop {
        line.clear();
        reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            break;
        }
        state.apply(serde_json::from_slice(&line)?)?;
        valid += line.len() as u64;
    }
    Ok((state, valid))
}
pub(super) struct TurnCapture {
    pub store: super::ChatStore,
    pub id: Uuid,
    pub before: super::checkpoint::Capture,
    pub _lease: super::checkpoint::WorkspaceLease,
}
struct ControllerLease {
    file: File,
    closing: Option<(tokio::sync::oneshot::Sender<Result<()>>, Result<()>)>,
}
impl Drop for ControllerLease {
    fn drop(&mut self) {
        // Explicit unlock also releases a Unix lock if a concurrently spawned
        // child has inherited the file description and has not exec'd yet.
        let unlocked = self.file.unlock().map_err(Error::from);
        if let Some((done, flush)) = self.closing.take() {
            let _ = done.send(flush.and(unlocked));
        }
    }
}
pub(super) struct Log {
    file: File,
    pub state: State,
    error: Option<String>,
    ids: MessageIds,
    closed_codex_turns: HashMap<String, Uuid>,
    notification_ids: HashMap<Uuid, MessageIds>,
    ledger: crate::usage::Ledger,
    graph: crate::observe::SessionGraph,
    dispatched: bool,
    morph_watch: Option<(super::checkpoint::WorkspaceLease, super::ChatStore)>,
    pub(super) native_store: Option<super::ChatStore>,
    pub(super) checkpoint: Option<TurnCapture>,
    // A queued native steer can start before its started frame reaches the host.
    // Keep the observed boundary and its workspace lease across that transition.
    steering_boundary: Option<TurnCapture>,
    // Last field: drop outstanding workspace leases before acknowledging close.
    lease: ControllerLease,
}
impl Drop for Log {
    fn drop(&mut self) {
        if self.lease.closing.is_some() {
            // Detached sends, rollbacks and forks can keep a sink alive after a
            // caller disconnects. Let the last writer finish its native outcome.
            let result = self.healthy().and_then(|()| match self.state.active() {
                Some(id) => self.end(id, TurnStatus::Interrupted),
                None => Ok(()),
            });
            if let Some((_, flush)) = &mut self.lease.closing {
                *flush = result;
            }
        }
    }
}
impl Log {
    pub fn new(file: File, lease: File, info: ChatInfo) -> Self {
        Self::loaded(file, lease, State::new(info))
    }
    pub fn loaded(file: File, lease: File, state: State) -> Self {
        Self {
            file,
            lease: ControllerLease {
                file: lease,
                closing: None,
            },
            state,
            error: None,
            ids: MessageIds::default(),
            closed_codex_turns: HashMap::new(),
            notification_ids: HashMap::new(),
            ledger: Default::default(),
            graph: Default::default(),
            dispatched: false,
            morph_watch: None,
            native_store: None,
            checkpoint: None,
            steering_boundary: None,
        }
    }
    pub fn finish_on_release(&mut self) -> tokio::sync::oneshot::Receiver<Result<()>> {
        let (done, released) = tokio::sync::oneshot::channel();
        self.lease.closing = Some((done, Ok(())));
        released
    }
    pub fn healthy(&self) -> Result<()> {
        if let Some(e) = &self.error {
            return Err(Error::Io(std::io::Error::other(e.clone())));
        }
        Ok(())
    }
    fn commit(&mut self, row: ChatUpdate) -> Result<()> {
        self.healthy()?;
        let result = append(&mut self.file, &row).and_then(|_| self.state.apply(row));
        if let Err(e) = &result {
            self.error = Some(format!("chat journal write: {e}"));
        }
        result
    }
    pub fn settle_abandoned_controls(&mut self) -> Result<()> {
        if let Some(report) = self.state.abandoned_compaction() {
            self.commit(ChatUpdate::ManualCompaction { report })?;
        }
        if self.state.waiting_steering() {
            self.commit(ChatUpdate::SteeringUnconfirmed)?;
        }
        if self.state.observing_notifications() {
            self.commit(ChatUpdate::NotificationsInterrupted)?;
        }
        Ok(())
    }
    pub fn attach(&mut self, stream: Uuid) -> Result<()> {
        self.ledger = Default::default();
        self.graph = Default::default();
        self.closed_codex_turns.clear();
        self.commit(ChatUpdate::Opened { stream })
    }
    pub async fn recover(
        &mut self,
        session: &crate::Session,
        options: &crate::SessionOptions,
        store: &super::ChatStore,
    ) -> Result<()> {
        if session.protocol() == Protocol::Claude {
            let history = claude::history(&self.state.info, options, &self.state.turns).await?;
            for id in history.received {
                self.input_sent(id)?;
            }
            if !history.edits.is_empty()
                && let Some(last) = self.state.unfinished_turn()
                && history.origin == Some(last.id)
                && let Some(frame) = session.recover_transcript(
                    "claude/transcript",
                    last.id.to_string(),
                    history.edits,
                    options,
                    |edits| last.transcript_adds_content(session.id(), edits) == Some(true),
                )?
            {
                self.commit(ChatUpdate::Reconciled { id: last.id, frame })?;
            }
            if let Some(from) = history.origin {
                for (batch, edits) in history.groups {
                    let Some(group) = self.state.claude_group(from, &batch.inputs, batch.at) else {
                        continue;
                    };
                    let membership_changed = group.changed(&self.state);
                    if let Some(frame) = session.recover_transcript(
                        "claude/follow-up",
                        group.turn.id.to_string(),
                        edits,
                        options,
                        |edits| {
                            group
                                .turn
                                .transcript_adds_content(session.id(), edits)
                                .is_some_and(|content| content || membership_changed)
                        },
                    )? {
                        self.commit(ChatUpdate::ClaudeFollowUp {
                            from,
                            inputs: batch.inputs,
                            at: batch.at,
                            frame,
                        })?;
                    }
                }
            }
        }
        if let Some(last) = self.state.unfinished_turn()
            && let Some(frame) = if session.protocol() == Protocol::Factory {
                if let Some(prompt) = last.prompt() {
                    session.recover_factory_turn(options, last, prompt).await?
                } else {
                    None
                }
            } else if self.state.info.runtime == "opencode" && session.protocol() == Protocol::Acp {
                crate::providers::opencode_recovery::recover(
                    &self.state.info,
                    session,
                    options,
                    last,
                )
                .await?
            } else if session.protocol() == Protocol::Pi {
                session.recover_pi_turn(options, last).await?
            } else if session.protocol() == Protocol::Morph {
                use crate::community::morph::recovery::Resume;
                match self.state.morph.as_ref() {
                    Some(saved) => match session.recover_morph_turn(options, last, saved).await? {
                        Resume::Unconfirmed => None,
                        Resume::Finished(frame) => Some(frame),
                        Resume::Running {
                            task,
                            seq,
                            open_tools,
                        } => {
                            let lease = store.workspace_lease(&self.state.info.workspace)?;
                            saved.watch(session).await?;
                            self.morph_watch = Some((lease, store.clone()));
                            self.dispatched = true;
                            self.ids.resume_morph(&task, seq);
                            return self.commit(ChatUpdate::MorphReattached {
                                stream: session.stream_id(),
                                id: last.id,
                                task,
                                open_tools,
                            });
                        }
                    },
                    None => None,
                }
            } else {
                session
                    .records()
                    .iter()
                    .find_map(|r| match session.protocol() {
                        Protocol::Codex => {
                            Frame::codex_recovery(last.native_turn_id.as_deref()?, session.id(), r)
                        }
                        Protocol::KimiKap => Frame::kimi_recovery(last, session.id(), r),
                        Protocol::Dsh => crate::dsh::recovery::frame(last, session.id(), r),
                        _ => None,
                    })
            }
        {
            self.commit(ChatUpdate::Reconciled { id: last.id, frame })?;
        }
        if let Some(id) = self.state.active() {
            self.end(id, TurnStatus::Interrupted)?;
        }
        Ok(())
    }
    pub fn existing(&self, id: Uuid, text: &str) -> Result<Option<ChatTurn>> {
        self.healthy()?;
        if let Some(prior) = self.state.find(id) {
            if prior.prompt() != Some(text) {
                return Err(Error::Refused(
                    "request id already belongs to a different prompt".into(),
                ));
            }
            return Ok(Some(prior.clone()));
        }
        self.state.check_new_prompt_id(id)?;
        if self.state.restoring.is_some() {
            return Err(Error::Refused(
                "finish the pending workspace rollback first".into(),
            ));
        }
        if self.state.active().is_some()
            || self.state.compacting()
            || self.state.observing_notifications()
        {
            return Err(Error::Busy);
        }
        Ok(None)
    }
    pub fn begin(&mut self, id: Uuid, text: &str, before_tree: String) -> Result<()> {
        self.ids = MessageIds::default();
        self.dispatched = false;
        self.commit(ChatUpdate::Begin {
            id,
            at: super::now(),
            text: text.into(),
            before_tree: Some(before_tree),
        })
    }
    pub fn input_sent(&mut self, id: Uuid) -> Result<()> {
        if self.state.find(id).is_some_and(|t| t.input_sent) {
            return self.healthy();
        }
        self.commit(ChatUpdate::InputSent { id })
    }
    pub fn captured(&mut self, id: Uuid, tree: String, files: Vec<FileChange>) -> Result<()> {
        self.commit(ChatUpdate::Captured { id, tree, files })
    }
    pub(super) fn finish_checkpoint(&mut self, id: Uuid) -> Result<()> {
        if self.checkpoint.as_ref().is_some_and(|c| c.id == id) {
            let pending = self.checkpoint.take().expect("matched pending checkpoint");
            let after = pending.store.capture(
                &self.state.info,
                CheckpointPoint {
                    turn: id,
                    before: false,
                },
                None,
            )?;
            self.captured(id, after.tree.clone(), pending.before.changes(&after))?;
            if self.state.info.protocol == Protocol::Claude
                && self.state.find(id).is_some_and(|t| {
                    t.steered_inputs
                        .values()
                        .any(|s| *s == crate::SteerStatus::Queued)
                })
            {
                self.steering_boundary = Some(TurnCapture {
                    before: after,
                    ..pending
                });
            }
        }
        Ok(())
    }
    fn native_begin(&mut self, r: &Record, historical: bool) -> Result<()> {
        let Some(store) = &self.native_store else {
            return Ok(());
        };
        let steering = r.events.iter().find_map(|e| match e {
            Event::SteeringTurnStarted { input_id, from } => Some((*input_id, *from)),
            _ => None,
        });
        let native_run = r.events.iter().find_map(|event| match event {
            Event::NativeTurnStarted { id, native_turn } => Some((*id, native_turn.as_str())),
            _ => None,
        });
        let started = match self.state.info.protocol {
            Protocol::Acp if self.state.info.runtime == "grok" => native_run.is_some(),
            Protocol::Claude => steering.is_some(),
            Protocol::Codex => r.native["method"] == "turn/started",
            Protocol::Dsh => r.native["event"]["type"] == "turn/start",
            _ => false,
        };
        if self.state.compacting()
            || !started
            || r.session_id.as_deref() != Some(&self.state.info.native_id)
            || !r.agent_path.is_empty()
            || self.state.active().is_some()
        {
            return Ok(());
        }
        let Some(native) = native_run
            .map(|(_, name)| name)
            .or(r.span_id.as_deref())
            .filter(|s| !s.is_empty())
        else {
            return Ok(());
        };
        if native_run.map_or_else(
            || {
                self.state
                    .turns
                    .iter()
                    .any(|t| t.native_turn_id.as_deref() == Some(native))
            },
            |(id, _)| self.state.find(id).is_some(),
        ) {
            return Ok(());
        }
        let id = native_run
            .map(|(id, _)| id)
            .or_else(|| steering.map(|(id, _)| id))
            .unwrap_or_else(Uuid::new_v4);
        // Replayed frames can reconstruct conversation history, but the workspace may have
        // changed since this boundary. Only take before/after checkpoints on live observation.
        let mut checkpoint = None;
        if !historical
            && steering.is_some()
            && let Some(mut boundary) = self.steering_boundary.take()
        {
            store.pin_capture(
                &self.state.info,
                CheckpointPoint {
                    turn: id,
                    before: true,
                },
                &boundary.before,
            )?;
            boundary.id = id;
            checkpoint = Some(boundary);
        } else if !historical && !(steering.is_some() && r.native["type"] == "result") {
            let lease = store.workspace_lease(&self.state.info.workspace);
            match lease {
                Ok(lease) => {
                    let before = store.capture(
                        &self.state.info,
                        CheckpointPoint {
                            turn: id,
                            before: true,
                        },
                        None,
                    )?;
                    checkpoint = Some(TurnCapture {
                        store: store.clone(),
                        id,
                        before,
                        _lease: lease,
                    });
                }
                Err(Error::Busy) => {}
                Err(e) => return Err(e),
            }
        }
        self.ids = MessageIds::default();
        self.dispatched = true;
        self.commit(ChatUpdate::NativeBegin {
            id,
            native_turn: native.into(),
            at: r.timestamp_ms,
            before_tree: checkpoint.as_ref().map(|c| c.before.tree.clone()),
            steered_from: steering.map(|(_, from)| from),
        })?;
        self.checkpoint = checkpoint;
        Ok(())
    }
    pub fn start_restore(&mut self, target: CheckpointPoint) -> Result<()> {
        if self.state.restoring == Some(target) {
            return Ok(());
        }
        self.commit(ChatUpdate::RestoreStarted { target })
    }
    pub fn finish_restore(&mut self, target: CheckpointPoint) -> Result<()> {
        self.commit(ChatUpdate::Restored { target })
    }
    pub(super) fn import_prompt(
        &mut self,
        input: Option<Uuid>,
        text: String,
        at: u64,
    ) -> Result<()> {
        if let Some(id) = self.state.active() {
            self.end(id, TurnStatus::Interrupted)?;
        }
        self.ids = MessageIds::default();
        self.dispatched = true;
        self.commit(ChatUpdate::Begin {
            id: input
                .filter(|id| self.state.find(*id).is_none())
                .unwrap_or_else(Uuid::new_v4),
            at,
            text,
            before_tree: None,
        })
    }
    pub(super) fn import_record(&mut self, r: &Record) -> Result<()> {
        self.record(r, true)
    }
    pub fn end(&mut self, id: Uuid, status: TurnStatus) -> Result<()> {
        self.healthy()?;
        if self.state.active() == Some(id) {
            self.commit(ChatUpdate::End { id, status })?;
        }
        Ok(())
    }
    fn record(&mut self, r: &Record, historical: bool) -> Result<()> {
        self.healthy()?;
        let normalized;
        let r = if self.state.info.runtime == "cursor"
            && let Some(path) = self.cursor_source_path(r)
        {
            normalized = Record {
                agent_path: path,
                ..r.clone()
            };
            &normalized
        } else {
            r
        };
        // Cursor replays historical child spawns and terminal states during session/load.
        if self.state.info.runtime == "cursor"
            && historical
            && !self.dispatched
            && r.native["method"] == "session/update"
        {
            return self.reconcile_cursor_child(r);
        }
        let root =
            r.agent_path.is_empty() && r.session_id.as_deref() == Some(&self.state.info.native_id);
        // A dedicated steering RPC can answer after the original prompt, or after
        // the next turn has started. Its captured owner survives both races.
        if root
            && let Some((id, status)) = r.events.iter().find_map(|e| match e {
                Event::SteeringInput {
                    chat_turn: Some(id),
                    status,
                    ..
                } => Some((*id, *status)),
                _ => None,
            })
            && (self.state.info.protocol == Protocol::Claude || self.state.active() != Some(id))
            && let Some(frame) =
                self.ids
                    .frame(self.state.info.protocol, &self.state.info.native_id, r)
        {
            if matches!(self.state.info.protocol, Protocol::Claude | Protocol::Pi)
                && self.state.find(id).is_none()
            {
                self.commit(ChatUpdate::Notification {
                    id,
                    at: r.timestamp_ms,
                    frame,
                })?;
            } else {
                self.commit(ChatUpdate::Frame { id, frame })?;
            }
            if self.steering_boundary.is_some()
                && status != crate::SteerStatus::FollowUp
                && !self.state.waiting_steering()
            {
                self.steering_boundary.take();
            }
            return Ok(());
        }
        if let Some(reading) = self
            .ledger
            .observe(&self.state.info.native_id, &mut self.graph, r)
            && (reading.usage.total.is_some() || !reading.children.is_empty())
            && !self.state.usage.last().is_some_and(|prior| {
                prior.cursor.stream_id == r.stream_id
                    && prior.usage == reading.usage
                    && prior.children == reading.children
            })
        {
            self.commit(ChatUpdate::UsageChanged { reading })?;
        }
        if r.events.iter().any(|e| {
            matches!(
                e,
                Event::ApprovalRequested { .. }
                    | Event::RequestResolved { .. }
                    | Event::Exited { .. }
                    | Event::Released
            )
        }) {
            let mut requests = self.state.pending_requests.clone();
            if crate::interaction::update_pending(&mut requests, r, root) {
                self.commit(ChatUpdate::RequestsChanged { requests })?;
            }
        }
        if root {
            if let Some(report) = r.manual_compaction() {
                self.commit(ChatUpdate::ManualCompaction { report })?;
            }
            for report in r.compactions() {
                self.commit(ChatUpdate::Compaction { report })?;
            }
        }
        if root
            && let Some(usage) = r.events.iter().find_map(|e| {
                if let Event::ContextUsage { usage } = e {
                    Some(usage)
                } else {
                    None
                }
            })
            && self.state.context_usage.as_ref() != Some(usage)
        {
            self.commit(ChatUpdate::ContextChanged {
                usage: usage.clone(),
            })?;
        }
        let changes = crate::tasks::changes(&self.state.tasks, r, root);
        if !changes.is_empty() {
            self.commit(ChatUpdate::TasksChanged { tasks: changes })?;
        }
        if r.agent_path.is_empty()
            && r.session_id.as_deref() == Some(&self.state.info.native_id)
            && let Some(goal) = r.events.iter().find_map(|event| {
                if let Event::GoalChanged { goal } = event {
                    Some(goal)
                } else {
                    None
                }
            })
            && self.state.goal.as_ref() != Some(goal)
        {
            self.commit(ChatUpdate::GoalChanged { goal: goal.clone() })?;
        }
        self.native_begin(r, historical)?;
        if matches!(self.state.info.protocol, Protocol::Claude | Protocol::Pi) {
            if root
                && self.state.observing_notifications()
                && r.events
                    .iter()
                    .any(|e| matches!(e, Event::Exited { .. } | Event::Released))
            {
                self.commit(ChatUpdate::NotificationsInterrupted)?;
            }
            if let Some(id) = r.events.iter().find_map(|e| match e {
                Event::NotificationActivity { id, .. } => Some(*id),
                _ => None,
            }) {
                if let Some(frame) = self.notification_ids.entry(id).or_default().frame(
                    self.state.info.protocol,
                    &self.state.info.native_id,
                    r,
                ) {
                    if self
                        .state
                        .find(id)
                        .is_some_and(|t| t.steered_from.is_some())
                    {
                        self.commit(ChatUpdate::Frame { id, frame })?;
                        if self.state.active() != Some(id) {
                            self.finish_checkpoint(id)?;
                        }
                    } else {
                        // Independent activity breaks the contiguous workspace boundary.
                        self.steering_boundary.take();
                        self.commit(ChatUpdate::Notification {
                            id,
                            at: r.timestamp_ms,
                            frame,
                        })?;
                    }
                }
                return Ok(());
            }
        }
        if root && let [Event::TurnProcessCleanup { cleanup }] = r.events.as_slice() {
            if let Some(id) = self
                .state
                .turns
                .iter()
                .rev()
                .find(|t| t.native_turn_id.as_deref() == Some(&cleanup.turn_id))
                .map(|t| t.id)
            {
                self.commit(ChatUpdate::TurnProcessCleanup {
                    id,
                    cursor: r.cursor(),
                    cleanup: cleanup.clone(),
                })?;
            }
            return Ok(());
        }
        if root
            && self.state.info.protocol == Protocol::Codex
            && let Some(native_turn) = &r.span_id
            && let Some(&id) = self.closed_codex_turns.get(native_turn)
        {
            let source = crate::observe::Agent {
                session_id: r.session_id.clone(),
                path: r.agent_path.clone(),
            };
            let turn = self.state.find(id).expect("recorded native turn");
            let events: Vec<_> = r
                .events
                .iter()
                .filter(|e| turn.owns_tool_update(&source, e))
                .cloned()
                .collect();
            if !events.is_empty() {
                self.commit(ChatUpdate::LateTool {
                    elided: r.elided,
                    id,
                    cursor: r.cursor(),
                    native_turn: native_turn.clone(),
                    source,
                    events,
                })?;
            }
            return Ok(());
        }
        if self.state.info.runtime == "cursor" && self.record_cursor_child(r)? {
            return Ok(());
        }
        if self.state.info.runtime == "opencode" && self.record_opencode(r, historical)? {
            return Ok(());
        }
        let Some(id) = self.state.active() else {
            return Ok(());
        };
        if r.direction == Direction::ToRuntime
            && r.kind == RecordKind::Request
            && (r
                .events
                .iter()
                .any(|e| matches!(e, Event::PromptDispatched))
                || (self.state.info.protocol == Protocol::Morph && r.native.get("task").is_some())
                || (self.state.info.protocol == Protocol::Cybion
                    && r.native.get("input").is_some()))
        {
            self.dispatched = true;
            if r.events
                .iter()
                .any(|e| matches!(e, Event::PromptDispatched))
            {
                self.input_sent(id)?;
            }
        }
        if !self.dispatched {
            return Ok(());
        }
        if self.state.info.protocol == Protocol::Codex
            && r.agent_path.is_empty()
            && r.session_id.as_deref() == Some(&self.state.info.native_id)
            && let (Some(expected), Some(reported)) = (
                self.state
                    .turns
                    .last()
                    .and_then(|t| t.native_turn_id.as_ref()),
                &r.span_id,
            )
            && expected != reported
        {
            return Ok(());
        }
        if self.state.info.protocol == Protocol::Pi
            && r.direction == Direction::FromRuntime
            && r.agent_path.is_empty()
            && r.session_id.as_deref() == Some(&self.state.info.native_id)
            && matches!(
                r.native["type"].as_str(),
                Some("message_start" | "message_end")
            )
            && r.native["message"]["role"] == "user"
            && let Some(timestamp_ms) = r.native["message"]["timestamp"].as_u64()
            && self
                .state
                .find(id)
                .is_some_and(|t| t.pi_input_timestamp_ms.is_none())
        {
            self.commit(ChatUpdate::PiInput { id, timestamp_ms })?;
        }
        if let Some(frame) = self
            .ids
            .frame(self.state.info.protocol, &self.state.info.native_id, r)
        {
            self.commit(ChatUpdate::Frame { id, frame })?;
            if self.state.active().is_none() {
                if self.state.info.protocol == Protocol::Codex
                    && let Some(native) =
                        self.state.find(id).and_then(|t| t.native_turn_id.as_ref())
                {
                    self.closed_codex_turns.insert(native.clone(), id);
                }
                if let Some((lease, store)) = self.morph_watch.take()
                    && let Some(turn) = self.state.find(id)
                    && turn.status != TurnStatus::Interrupted
                    && let Some(cp) = turn.checkpoint.as_ref().filter(|c| c.after_tree.is_none())
                {
                    let before = store.saved_capture(&self.state.info, &cp.before_tree)?;
                    self.checkpoint = Some(TurnCapture {
                        store,
                        id,
                        before,
                        _lease: lease,
                    });
                    self.finish_checkpoint(id)?;
                } else if self.native_store.is_some() {
                    self.finish_checkpoint(id)?;
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone)]
pub(crate) struct Sink(pub(super) Arc<Mutex<Log>>);
impl Sink {
    pub(crate) fn claude_result_ids(&self) -> std::collections::HashSet<Uuid> {
        let log = self.0.lock().unwrap();
        if log.state.info.protocol != Protocol::Claude {
            return Default::default();
        }
        log.state
            .turns
            .iter()
            .chain(log.state.notifications.iter().map(|n| &n.activity))
            .filter_map(|t| t.native_result_id)
            .collect()
    }
    /// Called before a record reaches subscribers or a completed turn is acknowledged.
    pub(crate) fn capture(&self, r: &Record, historical: bool) {
        let mut log = self.0.lock().unwrap();
        if let Err(e) = log.record(r, historical) {
            log.error.get_or_insert_with(|| e.to_string());
        }
    }
}
