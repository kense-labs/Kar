use crate::{
    Cursor, Event, Outcome, Protocol, Record,
    observe::{Agent, Tool},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Aborted,
    Failed,
    Interrupted,
}
impl From<&Outcome> for TurnStatus {
    fn from(o: &Outcome) -> Self {
        match o {
            Outcome::Completed => Self::Completed,
            Outcome::Aborted => Self::Aborted,
            Outcome::Failed => Self::Failed,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Reasoning,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    Message {
        role: Role,
        text: String,
        provider_id: Option<String>,
        complete: bool,
    },
    Tool {
        id: String,
        activity: Tool,
    },
    Retry {
        status: RetryStatus,
    },
    Compaction {
        report: crate::CompactionReport,
    },
    Diff {
        turn_id: String,
        diff: String,
    },
}
/// A provider retry as observed at the time of its native notification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryStatus {
    pub received_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimelineItem {
    pub source: Agent,
    #[serde(flatten)]
    pub item: Item,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatTurn {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<super::Attachment>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub steered_attachments: BTreeMap<Uuid, Vec<super::Attachment>>,
    /// Stable across restarts; also the caller's idempotency key.
    pub id: Uuid,
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_input_receipt: Option<crate::InputReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_cleanups: Vec<crate::TurnProcessCleanup>,
    /// UUID of the native terminal packet, distinct from this turn's input UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_result_id: Option<Uuid>,
    /// Input UUIDs the native model loop explicitly included in its result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_result_inputs: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::TurnFailure>,
    #[serde(
        default,
        deserialize_with = "crate::stream::present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_output: Option<serde_json::Value>,
    /// The native runtime initiated this turn independently of a host input.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub native_continuation: bool,
    /// The earlier activity in which this independently executed input was submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steered_from: Option<Uuid>,
    /// Native ACP child sessions bound to this turn and each child's parent session.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub child_sessions: std::collections::BTreeMap<String, String>,
    pub items: Vec<TimelineItem>,
    /// For a steered user message, its provider_id is the key in this map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub steered_inputs: BTreeMap<Uuid, crate::SteerStatus>,
    pub fork_anchor: Option<String>,
    #[serde(default)]
    pub native_turn_id: Option<String>,
    /// Timestamp from Pi's first native user echo in this admitted turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_input_timestamp_ms: Option<u64>,
    /// The native transport reported a successful prompt write or submission.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub input_sent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<super::Checkpoint>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolEvents {
    Live,
    DshStream,
    DshHistory,
}

/// A terminal transcript identifies each native call by ID. Turn its full
/// payload into one replacement, including for cards locally closed on crash.
pub(super) fn snapshot_tools(edits: &mut Vec<Edit>, elided: bool) {
    if !edits
        .iter()
        .any(|edit| matches!(edit, Edit::Activity {event} if Tool::id(event).is_some()))
    {
        return;
    }
    let mut positions = std::collections::HashMap::new();
    for edit in std::mem::take(edits) {
        if let Edit::Activity { event } = &edit
            && let Some(id) = Tool::id(event)
        {
            let position = *positions.entry(id.to_owned()).or_insert_with(|| {
                let index = edits.len();
                edits.push(Edit::ToolSnapshot {
                    id: id.into(),
                    activity: Tool::default(),
                });
                index
            });
            if let Edit::ToolSnapshot { activity, .. } = &mut edits[position] {
                activity.apply(event, elided);
            }
        } else {
            edits.push(edit);
        }
    }
}

impl ChatTurn {
    /// None means the native blocks conflict with a locally observed item.
    /// Otherwise report whether any message or tool information would change.
    pub(crate) fn transcript_adds_content(&self, root: &str, edits: &[Edit]) -> Option<bool> {
        use std::collections::HashMap;
        let mut messages = HashMap::new();
        let mut accepted = HashMap::new();
        let mut tools = HashMap::new();
        let mut changed = false;
        for edit in edits {
            match edit {
                Edit::Text {
                    key: Some(id),
                    role,
                    text,
                    ..
                } => {
                    messages.insert(id.as_str(), (role, text, false));
                }
                Edit::ToolSnapshot { id, activity } => {
                    tools.insert(id.as_str(), (activity, false));
                }
                Edit::SteeringInput {
                    id,
                    text,
                    status: crate::SteerStatus::Accepted,
                    ..
                } => {
                    if accepted
                        .insert(id.to_string(), (text.as_str(), false))
                        .is_some()
                    {
                        return None;
                    }
                    match self.steered_inputs.get(id)? {
                        crate::SteerStatus::Submitted
                        | crate::SteerStatus::Queued
                        | crate::SteerStatus::Unconfirmed => changed = true,
                        crate::SteerStatus::Accepted => {}
                        _ => return None,
                    }
                }
                _ => {}
            }
        }
        for item in self
            .items
            .iter()
            .filter(|i| i.source.path.is_empty() && i.source.session_id.as_deref() == Some(root))
        {
            match &item.item {
                Item::Message {
                    role: Role::User,
                    provider_id,
                    text,
                    ..
                } => {
                    if let Some((expected, seen)) =
                        provider_id.as_ref().and_then(|id| accepted.get_mut(id))
                    {
                        if *seen || *expected != text {
                            return None;
                        }
                        *seen = true;
                    }
                }
                Item::Message {
                    role,
                    text,
                    provider_id,
                    complete,
                } => {
                    let (native_role, native_text, seen) =
                        messages.get_mut(provider_id.as_deref()?)?;
                    if *native_role != role
                        || !native_text.starts_with(text.as_str())
                        || (*complete && *native_text != text)
                    {
                        return None;
                    }
                    *seen = true;
                    changed |= !complete;
                }
                Item::Tool { id, activity } => {
                    let (native, seen) = tools.get_mut(id.as_str())?;
                    if (activity.name.is_some() && activity.name != native.name)
                        || (!activity.input_partial
                            && activity.input.is_some()
                            && activity.input != native.input)
                        || (activity.result.is_some() && activity.result != native.result)
                    {
                        return None;
                    }
                    *seen = true;
                    changed |= activity.input_partial
                        || activity.name != native.name
                        || activity.input != native.input
                        || activity.result != native.result;
                }
                _ => {}
            }
        }
        if accepted.values().any(|(_, seen)| !seen) {
            return None;
        }
        Some(
            changed
                || messages.values().any(|(_, _, seen)| !seen)
                || tools.values().any(|(_, seen)| !seen),
        )
    }

    pub fn tool_groups(&self, runtime: &str) -> Vec<crate::observe::ToolGroupSegment> {
        use crate::observe::ToolGroupPart;
        crate::observe::group_tool_activity(
            runtime,
            self.items.iter().map(|entry| {
                let part = match &entry.item {
                    Item::Tool { id, activity } => ToolGroupPart::Tool { id, tool: activity },
                    Item::Message {
                        role: Role::Reasoning,
                        ..
                    } => ToolGroupPart::Reasoning,
                    _ => ToolGroupPart::Other,
                };
                (&entry.source, part)
            }),
        )
    }

    pub(super) fn new(id: Uuid, started_ms: u64, root: &str, text: Option<String>) -> Self {
        Self {
            id,
            started_ms,
            finished_ms: None,
            attachments: Vec::new(),
            steered_attachments: BTreeMap::new(),
            status: TurnStatus::Running,
            native_input_receipt: None,
            process_cleanups: Vec::new(),
            native_result_id: None,
            native_result_inputs: Vec::new(),
            failure: None,
            structured_output: None,
            native_continuation: text.is_none(),
            steered_from: None,
            child_sessions: Default::default(),
            steered_inputs: Default::default(),
            items: text
                .into_iter()
                .map(|text| TimelineItem {
                    source: Agent {
                        session_id: Some(root.into()),
                        path: vec![],
                    },
                    item: Item::Message {
                        role: Role::User,
                        text,
                        provider_id: None,
                        complete: true,
                    },
                })
                .collect(),
            fork_anchor: None,
            native_turn_id: None,
            pi_input_timestamp_ms: None,
            input_sent: false,
            checkpoint: None,
        }
    }
    pub(crate) fn prompt(&self) -> Option<&str> {
        if self.native_continuation {
            return None;
        }
        match &self.items.first()?.item {
            Item::Message {
                role: Role::User,
                text,
                ..
            } => Some(text),
            _ => None,
        }
    }
    pub(super) fn owns_tool_update(&self, source: &Agent, event: &Event) -> bool {
        !matches!(event, Event::ToolStarted { .. })
            && Tool::id(event).is_some_and(|id| {
                self.items
                    .iter()
                    .any(|i| &i.source == source && matches!(&i.item,Item::Tool{id:k,..} if k==id))
            })
    }
    pub(super) fn finish(&mut self, status: TurnStatus, root: &str) {
        self.status = status;
        for item in &mut self.items {
            match &mut item.item {
                Item::Message { complete, .. }
                    if status == TurnStatus::Completed
                        && !item
                            .source
                            .session_id
                            .as_ref()
                            .is_some_and(|s| self.child_sessions.contains_key(s)) =>
                {
                    *complete = true
                }
                Item::Tool { activity, .. }
                    if item.source.path.is_empty()
                        && item.source.session_id.as_deref() == Some(root) =>
                {
                    activity.finished = true
                }
                _ => {}
            }
        }
    }
    pub(super) fn apply_transcript(&mut self, frame: &Frame, root: &str) {
        let observed = self.items.len();
        self.apply(frame, root);
        if self.items.len() == observed {
            return;
        }

        // Insert previously unseen native items before their next observed sibling.
        // Existing child-session output and host input receipts keep their relative order.
        let order: std::collections::HashMap<_, _> = frame
            .edits
            .iter()
            .enumerate()
            .filter_map(|(i, edit)| {
                let key = match edit {
                    Edit::Text {
                        role,
                        key: Some(id),
                        ..
                    } => (Some(*role), id.as_str()),
                    Edit::ToolSnapshot { id, .. } => (None, id.as_str()),
                    _ => return None,
                };
                Some((key, i))
            })
            .collect();
        let rank = |entry: &TimelineItem| {
            if entry.source != frame.source {
                return None;
            }
            match &entry.item {
                Item::Message {
                    role,
                    provider_id: Some(id),
                    ..
                } => order.get(&(Some(*role), id.as_str())).copied(),
                Item::Tool { id, .. } => order.get(&(None, id.as_str())).copied(),
                _ => None,
            }
        };
        let mut additions = self.items.split_off(observed).into_iter().peekable();
        for entry in std::mem::take(&mut self.items) {
            if let Some(position) = rank(&entry) {
                while additions
                    .peek()
                    .and_then(&rank)
                    .is_some_and(|i| i < position)
                {
                    self.items.push(additions.next().unwrap());
                }
            }
            self.items.push(entry);
        }
        self.items.extend(additions);
    }
    pub(super) fn apply_claude_history(&mut self, frame: &Frame, root: &str) {
        self.apply_transcript(frame, root);
        if self.status == TurnStatus::Interrupted {
            self.finish(TurnStatus::Interrupted, root);
        }
    }
    pub(super) fn apply(&mut self, frame: &Frame, root: &str) {
        self.apply_edits(frame, root, ToolEvents::Live);
    }

    pub(super) fn apply_edits(&mut self, frame: &Frame, root: &str, mode: ToolEvents) {
        // Interruption closes the local cards. Replayed start events still
        // belong to those calls, up to each native end in the recovered turn.
        let mut recovered = (mode == ToolEvents::DshHistory).then(std::collections::HashMap::new);
        if let Some(id) = &frame.native_turn {
            self.native_turn_id = Some(id.clone());
        }
        let is_root =
            frame.source.path.is_empty() && frame.source.session_id.as_deref() == Some(root);
        self.input_sent |= is_root && frame.morph_progress.as_ref().is_some_and(|p| p.accepted);
        for edit in &frame.edits {
            match edit {
                Edit::FinishedAt { at } => {
                    if is_root && self.status != TurnStatus::Running {
                        self.finished_ms.get_or_insert(*at);
                    }
                }

                Edit::ToolSnapshot { id, activity } => {
                    let existing = self.items.iter_mut().find(|item| {
                        item.source == frame.source
                            && matches!(&item.item, Item::Tool {id: key, ..} if key == id)
                    });
                    if let Some(TimelineItem {
                        item:
                            Item::Tool {
                                activity: saved, ..
                            },
                        ..
                    }) = existing
                    {
                        let preview = std::mem::take(&mut saved.preview);
                        saved.clone_from(activity);
                        if saved.preview.is_empty() {
                            saved.preview = preview;
                        }
                    } else {
                        self.items.push(TimelineItem {
                            source: frame.source.clone(),
                            item: Item::Tool {
                                id: id.clone(),
                                activity: activity.clone(),
                            },
                        });
                    }
                }
                Edit::NativeResult { id, inputs } => {
                    self.native_result_id = Some(*id);
                    self.native_result_inputs.clone_from(inputs);
                }
                Edit::SteeringInput {
                    id,
                    text,
                    status,
                    attachments,
                    ..
                } => {
                    if !attachments.is_empty() {
                        self.steered_attachments.insert(*id, attachments.clone());
                    }
                    self.steered_inputs.insert(*id, *status);
                    self.text(&frame.source, Role::User, Some(&id.to_string()), text, true);
                }
                Edit::UserInput { id, text } => {
                    if !(is_root && uuid::Uuid::parse_str(id).ok() == Some(self.id)) {
                        self.text(&frame.source, Role::User, Some(id), text, true);
                    }
                }

                Edit::Retry { status } => self.items.push(TimelineItem {
                    source: frame.source.clone(),
                    item: Item::Retry {
                        status: status.clone(),
                    },
                }),
                Edit::Compaction { report } => self.items.push(TimelineItem {
                    source: frame.source.clone(),
                    item: Item::Compaction {
                        report: report.clone(),
                    },
                }),
                Edit::Text {
                    role,
                    text,
                    key,
                    final_text,
                } => {
                    if *role == Role::User && is_root && self.prompt() == Some(text.as_str()) {
                        continue;
                    }
                    self.text(&frame.source, *role, key.as_deref(), text, *final_text);
                }
                Edit::Activity { event } => {
                    if let Some(id) = Tool::id(event) {
                        // DSH can stream a new call's arguments before dispatch
                        // announces its name, including when it reuses a call id.
                        let starts_call = matches!(event, Event::ToolStarted { .. })
                            || (mode == ToolEvents::DshStream
                                && self.status == TurnStatus::Running
                                && matches!(event, Event::ToolInputDelta { .. }));
                        let mut matching = self.items.iter_mut().filter(|i| {
                            i.source == frame.source
                                && matches!(&i.item,Item::Tool{id:key,..} if key==id)
                        });
                        let existing = if let Some(calls) = recovered.as_mut() {
                            let occurrence = calls.entry(id).or_insert(0usize);
                            let item = matching.nth(*occurrence);
                            if matches!(event, Event::ToolEnded { .. }) {
                                *occurrence += 1;
                            }
                            item
                        } else {
                            matching.next_back()
                        };
                        if let Some(TimelineItem {
                            item: Item::Tool { activity, .. },
                            ..
                        }) = existing
                            && (mode == ToolEvents::DshHistory
                                || !activity.finished
                                || !starts_call)
                        {
                            activity.apply(event, frame.elided);
                        } else {
                            let mut activity = Tool::default();
                            activity.apply(event, frame.elided);
                            self.items.push(TimelineItem {
                                source: frame.source.clone(),
                                item: Item::Tool {
                                    id: id.into(),
                                    activity,
                                },
                            });
                        }
                    } else if let Event::TurnDiff { turn_id, diff } = event {
                        let existing = self.items.iter_mut().find(|i| {
                            i.source == frame.source
                                && matches!(&i.item,Item::Diff{turn_id:k,..} if k==turn_id)
                        });
                        if let Some(TimelineItem {
                            item: Item::Diff { diff: d, .. },
                            ..
                        }) = existing
                        {
                            d.clone_from(diff);
                        } else {
                            self.items.push(TimelineItem {
                                source: frame.source.clone(),
                                item: Item::Diff {
                                    turn_id: turn_id.clone(),
                                    diff: diff.clone(),
                                },
                            });
                        }
                    } else if is_root {
                        match event {
                            Event::NativeInput { input_id, receipt } if input_id == &self.id => {
                                self.native_input_receipt = Some(*receipt);
                                if let Some(outcome) = receipt.settled_outcome() {
                                    self.finish((&outcome).into(), root);
                                }
                            }
                            Event::TurnFailure { failure } => self.failure = Some(failure.clone()),
                            Event::TurnEnded { outcome } => self.finish(outcome.into(), root),
                            Event::StructuredOutput { value } => {
                                self.structured_output = Some(value.clone())
                            }
                            Event::ForkAnchor { anchor } => self.fork_anchor = Some(anchor.clone()),
                            Event::Exited { .. } | Event::Released => {
                                self.finish(TurnStatus::Interrupted, root)
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    fn text(
        &mut self,
        source: &Agent,
        role: Role,
        key: Option<&str>,
        text: &str,
        final_text: bool,
    ) {
        // Provider identities win. A final snapshot may give an anonymous streamed message its first id.
        let exact = key.and_then(|id| {
            self.items.iter().rposition(|i| {
                &i.source == source
                    && matches!(&i.item,
            Item::Message{role:r,provider_id:Some(k),..} if *r==role && k==id)
            })
        });
        let open = exact.or_else(|| {
            self.items
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, item)| &item.source == source)
                .take_while(|(_, item)| !matches!(item.item, Item::Tool { .. }))
                .find_map(|(index, item)| match &item.item {
                    Item::Message {
                        role: r,
                        provider_id,
                        complete: false,
                        ..
                    } if *r == role && (key.is_none() || provider_id.is_none()) => Some(index),
                    _ => None,
                })
        });
        if let Some(i) = open
            && let Item::Message {
                text: body,
                provider_id,
                complete,
                ..
            } = &mut self.items[i].item
        {
            if final_text {
                body.clear();
                body.push_str(text);
                *complete = true;
            } else if !*complete {
                body.push_str(text);
            }
            if provider_id.is_none() {
                *provider_id = key.map(str::to_owned);
            }
        } else {
            self.items.push(TimelineItem {
                source: source.clone(),
                item: Item::Message {
                    role,
                    text: text.into(),
                    provider_id: key.map(str::to_owned),
                    complete: final_text,
                },
            });
        }
    }
}

/// Native task cursor persisted atomically with its visible chat edits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MorphProgress {
    pub task: String,
    pub seq: u64,
    pub reasoning_bytes: Option<usize>,
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gap: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub morph_progress: Option<MorphProgress>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub elided: bool,
    pub cursor: Cursor,
    #[serde(default)]
    pub native_turn: Option<String>,
    pub source: Agent,
    pub edits: Vec<Edit>,
}
impl Frame {
    pub(crate) fn morph_recovery(
        journal: &crate::stream::Journal,
        root: &str,
        task: &str,
        events: Vec<Event>,
        options: &crate::SessionOptions,
    ) -> crate::Result<Option<Self>> {
        let mut body = serde_json::to_value(events)?;
        crate::transport::redact(&mut body, &crate::options::session_secrets(options));
        let record = journal
            .append(
                crate::Direction::FromRuntime,
                crate::RecordKind::Frame,
                Some(root.into()),
                vec![],
                Some(task.into()),
                serde_json::json!({"type":"morph/task-recovery"}),
                serde_json::from_value(body)?,
            )
            .ok_or_else(|| {
                crate::Error::Closed("session closed during Morph task recovery".into())
            })?;
        Ok(MessageIds::default()
            .frame(Protocol::Morph, root, &record)
            .map(|mut frame| {
                frame.native_turn = Some(task.into());
                frame
            }))
    }
    pub(crate) fn transcript(
        journal: &crate::stream::Journal,
        root: &str,
        turn: String,
        kind: &str,
        mut edits: Vec<Edit>,
        options: &crate::SessionOptions,
        accept: impl FnOnce(&[Edit]) -> bool,
    ) -> crate::Result<Option<Self>> {
        snapshot_tools(&mut edits, false);
        let mut body = serde_json::json!({"type":kind,"turnId":turn,"edits":edits});
        crate::transport::redact(&mut body, &crate::options::session_secrets(options));
        let edits: Vec<Edit> = serde_json::from_value(body["edits"].clone())?;
        if !accept(&edits) {
            return Ok(None);
        }
        let record = journal
            .append(
                crate::Direction::FromRuntime,
                crate::RecordKind::Frame,
                Some(root.into()),
                vec![],
                Some(turn.clone()),
                body,
                vec![],
            )
            .ok_or_else(|| {
                crate::Error::Closed("session closed during transcript recovery".into())
            })?;
        Ok(Some(Self {
            elided: false,
            cursor: record.cursor(),
            morph_progress: None,
            native_turn: Some(turn),
            source: Agent {
                session_id: Some(root.into()),
                path: vec![],
            },
            edits,
        }))
    }
    /// Restore full native item snapshots over the matching partial local items.
    pub(super) fn kimi_recovery(local: &ChatTurn, root: &str, record: &Record) -> Option<Self> {
        let n = &record.native;
        if !record.agent_path.is_empty()
            || record.session_id.as_deref() != Some(root)
            || n["type"] != "turns"
            || n["session_id"] != root
            || n["agent_id"] != "main"
        {
            return None;
        }
        let (turn, claimed) = crate::kimi::recovery_turn(&n["turns"], local)?;
        let id = turn["turn_id"].as_str()?;
        let ended = turn["status"] == "ended";
        let mut edits = Vec::new();
        if claimed {
            edits.push(Edit::Activity {
                event: Event::NativeInput {
                    input_id: local.id,
                    receipt: crate::InputReceipt::Started,
                },
            });
        }
        let mut seen = std::collections::HashSet::new();
        if ended {
            for item in turn["items"].as_array().into_iter().flatten() {
                let Some(key) = item["item_id"].as_str() else {
                    continue;
                };
                if !seen.insert(key) {
                    return None;
                }
                match item["type"].as_str() {
                    Some(kind @ ("assistant_message" | "reasoning")) => {
                        if let Some(text) = item["text"].as_str() {
                            edits.push(Edit::Text {
                                role: if kind == "reasoning" {
                                    Role::Reasoning
                                } else {
                                    Role::Assistant
                                },
                                key: Some(key.into()),
                                text: text.into(),
                                final_text: true,
                            });
                        }
                    }
                    Some("tool_call") => edits.extend(
                        crate::kimi::tool_snapshot(key, item)
                            .into_iter()
                            .map(|event| Edit::Activity { event }),
                    ),
                    _ => {}
                }
            }
            let mut events = Vec::new();
            let outcome = crate::kimi::turn_outcome(turn, &mut events);
            events.push(Event::TurnEnded { outcome });
            edits.extend(events.into_iter().map(|event| Edit::Activity { event }));
        }
        snapshot_tools(&mut edits, record.elided);
        Some(Self {
            elided: record.elided,
            cursor: record.cursor(),
            morph_progress: None,
            native_turn: Some(id.into()),
            source: Agent {
                session_id: Some(root.into()),
                path: Vec::new(),
            },
            edits,
        })
    }
    /// Reconcile only a known turn, using a terminal snapshot in the real thread/resume response.
    pub(super) fn codex_recovery(id: &str, root: &str, record: &Record) -> Option<Self> {
        let thread = record.native.pointer("/result/thread")?;
        if thread["id"].as_str() != Some(root) {
            return None;
        }
        let turn = thread["turns"]
            .as_array()?
            .iter()
            .find(|v| v["id"].as_str() == Some(id))?;
        let outcome = match turn["status"].as_str()? {
            "completed" => Outcome::Completed,
            "interrupted" => Outcome::Aborted,
            "failed" => Outcome::Failed,
            _ => return None,
        };
        let mut edits = vec![];
        let mut seen = std::collections::HashSet::new();
        for item in turn["items"].as_array().into_iter().flatten() {
            let Some(key) = item["id"].as_str() else {
                continue;
            };
            if !seen.insert(key) {
                return None;
            }
            match item["type"].as_str() {
                Some("agentMessage") => {
                    if let Some(text) = item["text"].as_str() {
                        edits.push(Edit::Text {
                            role: Role::Assistant,
                            key: Some(key.into()),
                            text: text.into(),
                            final_text: true,
                        });
                    }
                }
                Some("reasoning") => {
                    let sections = |field: &str| {
                        item[field]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    let summary = sections("summary");
                    let text = if summary.is_empty() {
                        sections("content")
                    } else {
                        summary
                    };
                    if !text.is_empty() {
                        edits.push(Edit::Text {
                            role: Role::Reasoning,
                            key: Some(key.into()),
                            text,
                            final_text: true,
                        });
                    }
                }
                _ => {
                    // A native history item carries a full tool snapshot. Reuse live mappings
                    // so the same tool id updates a partially observed card after a crash.
                    for method in ["item/started", "item/completed"] {
                        let packet = serde_json::json!({"method":method,"params":{"threadId":root,"turnId":id,"item":item}});
                        edits.extend(
                            crate::wire::project(Protocol::Codex, &packet, false)
                                .into_iter()
                                .filter(|event| Tool::id(event).is_some())
                                .map(|event| Edit::Activity { event }),
                        );
                    }
                }
            }
        }
        if outcome == Outcome::Failed {
            edits.push(Edit::Activity {
                event: Event::TurnFailure {
                    failure: crate::wire::failure::codex(&turn["error"]),
                },
            });
        }
        edits.push(Edit::Activity {
            event: Event::ForkAnchor { anchor: id.into() },
        });
        edits.push(Edit::Activity {
            event: Event::TurnEnded { outcome },
        });
        snapshot_tools(&mut edits, record.elided);
        Some(Self {
            elided: record.elided,
            cursor: record.cursor(),
            morph_progress: None,
            native_turn: Some(id.into()),
            source: Agent {
                session_id: Some(root.into()),
                path: vec![],
            },
            edits,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    FinishedAt {
        at: u64,
    },
    ToolSnapshot {
        id: String,
        activity: Tool,
    },
    NativeResult {
        id: Uuid,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inputs: Vec<Uuid>,
    },
    SteeringInput {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<super::Attachment>,
        id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        text: String,
        status: crate::SteerStatus,
    },
    /// A native user message correlated to an input UUID, including steering inside a turn.
    UserInput {
        id: String,
        text: String,
    },
    Retry {
        status: RetryStatus,
    },
    Compaction {
        report: crate::CompactionReport,
    },
    Text {
        role: Role,
        text: String,
        key: Option<String>,
        final_text: bool,
    },
    Activity {
        event: Event,
    },
}
/// Only chat data and message identity cross the persistence boundary. Handshake/configuration frames stay in memory.
#[derive(Default)]
pub(super) struct MessageIds {
    morph_trace: BTreeMap<String, u64>,
    pi_replies: BTreeMap<Agent, u64>,
    stream: BTreeMap<Agent, String>,
    claude_text_blocks: BTreeMap<(Agent, String), VecDeque<u64>>,
    claude_packets: BTreeMap<(Agent, String), String>,
}
pub(super) fn pi_reply_key(ordinal: u64) -> String {
    format!("pi:assistant:{ordinal}")
}
impl MessageIds {
    pub(super) fn resume_morph(&mut self, task: &str, seq: u64) {
        self.morph_trace.insert(task.into(), seq);
    }

    // Claude Code can send one assistant envelope per block, each with a one-element
    // content array and the same model message id. The stream carries the original index.
    fn claude_text_key(&mut self, source: &Agent, n: &Value, array_index: u64) -> Option<String> {
        let message = n.pointer("/message/id").and_then(Value::as_str);
        let packet = n["uuid"].as_str();
        let id = message.or(packet)?;
        let pending = self
            .claude_text_blocks
            .get_mut(&(source.clone(), id.into()));
        if n.pointer("/message/content")?.as_array()?.len() > 1 {
            if let Some(pending) = pending {
                pending.retain(|i| *i != array_index);
            }
            return Some(format!("{id}#{array_index}"));
        }
        let packet = packet.map(|uuid| (source.clone(), uuid.to_owned()));
        if let Some(key) = packet.as_ref().and_then(|p| self.claude_packets.get(p)) {
            return Some(key.clone());
        }
        let key = match pending.and_then(VecDeque::pop_front) {
            Some(index) => format!("{id}#{index}"),
            None => format!(
                "{}#{array_index}",
                packet.as_ref().map_or(id, |(_, uuid)| uuid)
            ),
        };
        if let Some(packet) = packet {
            self.claude_packets.insert(packet, key.clone());
        }
        Some(key)
    }
    pub(super) fn frame(&mut self, p: Protocol, root: &str, r: &Record) -> Option<Frame> {
        let n = &r.native;
        let source = Agent {
            session_id: r.session_id.clone().or_else(|| Some(root.into())),
            path: r.agent_path.clone(),
        };
        let claude = matches!(p, Protocol::Claude | Protocol::Qoder | Protocol::Qwen);
        let morph_accepted = p == Protocol::Morph
            && r.kind == crate::RecordKind::Response
            && r.direction == crate::Direction::FromRuntime
            && r.events.contains(&Event::PromptDispatched)
            && r.span_id.as_deref().is_some_and(|id| n["id"] == id);
        let native_turn = if morph_accepted {
            r.span_id.clone()
        } else if p == Protocol::Codex
            && source.path.is_empty()
            && source.session_id.as_deref() == Some(root)
        {
            n.pointer("/result/turn/id")
                .or_else(|| {
                    (n["method"] == "turn/started")
                        .then(|| n.pointer("/params/turn/id"))
                        .flatten()
                })
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else if p == Protocol::Acp
            && source.path.is_empty()
            && r.direction == crate::Direction::ToRuntime
            && n["method"] == "session/prompt"
            && r.events.contains(&Event::PromptDispatched)
        {
            n.pointer("/params/_meta/promptId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else if p == Protocol::Factory
            && source.path.is_empty()
            && r.events.contains(&Event::TurnStarted)
        {
            n.pointer("/params/notification/message/id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else if p == Protocol::Dsh
            && source.path.is_empty()
            && r.events.contains(&Event::TurnStarted)
        {
            r.span_id.clone()
        } else if p == Protocol::KimiKap
            && source.path.is_empty()
            && r.events.contains(&Event::TurnStarted)
        {
            n.pointer("/turn/turn_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            None
        };
        if claude && n.pointer("/event/type").and_then(Value::as_str) == Some("message_start") {
            self.stream.remove(&source);
            if let Some(id) = n.pointer("/event/message/id").and_then(Value::as_str) {
                self.stream.insert(source.clone(), id.into());
            }
        }
        if p == Protocol::Claude
            && n.pointer("/event/type").and_then(Value::as_str) == Some("content_block_start")
            && n.pointer("/event/content_block/type")
                .and_then(Value::as_str)
                == Some("text")
            && let Some(message) = self.stream.get(&source)
            && let Some(index) = n.pointer("/event/index").and_then(Value::as_u64)
        {
            let pending = self
                .claude_text_blocks
                .entry((source.clone(), message.clone()))
                .or_default();
            if !pending.contains(&index) {
                pending.push_back(index);
            }
        }
        if p == Protocol::Pi
            && n["type"] == "message_start"
            && n.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
        {
            let count = self.pi_replies.entry(source.clone()).or_default();
            *count += 1;
            self.stream.insert(source.clone(), pi_reply_key(*count));
        }
        let block_key = |id: &str, index: u64| format!("{id}#{index}");
        let key = match p {
            Protocol::Codex => n
                .pointer("/params/itemId")
                .or_else(|| n.pointer("/params/item/id"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            Protocol::Pi => self
                .stream
                .get(&source)
                .zip(
                    n.pointer("/assistantMessageEvent/contentIndex")
                        .and_then(Value::as_u64),
                )
                .map(|(id, i)| block_key(id, i)),
            Protocol::Factory => {
                let notice = &n["params"]["notification"];
                notice["messageId"]
                    .as_str()
                    .zip(notice["blockIndex"].as_u64())
                    .map(|(id, index)| block_key(id, index))
            }
            Protocol::Dsh => n["itemId"].as_str().map(str::to_owned),
            Protocol::KimiKap => n["item_id"]
                .as_str()
                .or_else(|| n["item"]["item_id"].as_str())
                .map(str::to_owned),
            Protocol::Acp => n
                .pointer("/params/update/messageId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Protocol::Claude if n["type"] == "stream_event" => self
                .stream
                .get(&source)
                .zip(n.pointer("/event/index").and_then(Value::as_u64))
                .map(|(id, index)| block_key(id, index)),
            _ => None,
        };
        let message = if p == Protocol::Factory {
            &n["params"]["notification"]["message"]
        } else {
            &n["message"]
        };
        let mut text_blocks = message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, b)| b["type"] == "text")
            .map(|(i, _)| i as u64);
        let mut edits = Vec::new();
        let mut compactions = r.compactions();
        for event in &r.events {
            let (role, text, final_text) = match event {
                Event::Retry {
                    attempt,
                    max_attempts,
                    delay_ms,
                    reason,
                } => {
                    edits.push(Edit::Retry {
                        status: RetryStatus {
                            received_ms: r.timestamp_ms,
                            attempt: *attempt,
                            max_attempts: *max_attempts,
                            delay_ms: *delay_ms,
                            reason: reason.clone(),
                        },
                    });
                    continue;
                }
                Event::Compaction { .. } => {
                    if let Some(report) = compactions.next() {
                        edits.push(Edit::Compaction { report });
                    }
                    continue;
                }
                Event::UserMessage { text }
                    if p == Protocol::Factory && message["id"].is_string() =>
                {
                    edits.push(Edit::UserInput {
                        id: message["id"].as_str().unwrap().into(),
                        text: text.clone(),
                    });
                    continue;
                }
                Event::SteeringInput {
                    input_id,
                    text,
                    status,
                    ..
                } => {
                    edits.push(Edit::SteeringInput {
                        attachments: Vec::new(),
                        id: *input_id,
                        request_id: r.response_to.clone(),
                        text: text.clone(),
                        status: *status,
                    });
                    continue;
                }
                Event::UserMessage { text } => (Role::User, text, true),
                Event::TextDelta { text } => (Role::Assistant, text, false),
                Event::Message { text } => (Role::Assistant, text, true),
                Event::ReasoningDelta { text } => (Role::Reasoning, text, false),
                e if Tool::id(e).is_some()
                    || matches!(
                        e,
                        Event::NotificationActivity { .. }
                            | Event::NotificationJoined { .. }
                            | Event::SteeringMoved { .. }
                            | Event::NativeInput { .. }
                            | Event::TurnDiff { .. }
                            | Event::StructuredOutput { .. }
                            | Event::TurnFailure { .. }
                            | Event::TurnEnded { .. }
                            | Event::ForkAnchor { .. }
                            | Event::Released
                            | Event::Exited { .. }
                    ) =>
                {
                    edits.push(Edit::Activity { event: e.clone() });
                    continue;
                }
                _ => continue,
            };
            let key = if p == Protocol::Factory && matches!(event, Event::Message { .. }) {
                message["id"]
                    .as_str()
                    .zip(text_blocks.next())
                    .map(|(id, index)| block_key(id, index))
            } else if p == Protocol::Claude && matches!(event, Event::Message { .. }) {
                text_blocks
                    .next()
                    .and_then(|i| self.claude_text_key(&source, n, i))
            } else if matches!(p, Protocol::Qwen | Protocol::Qoder)
                && matches!(event, Event::Message { .. })
            {
                n.pointer("/message/id")
                    .and_then(Value::as_str)
                    .or_else(|| n["uuid"].as_str())
                    .zip(text_blocks.next())
                    .map(|(id, i)| block_key(id, i))
            } else {
                key.clone()
            };
            edits.push(Edit::Text {
                role,
                text: text.clone(),
                key,
                final_text,
            });
        }
        if p == Protocol::Pi
            && n["type"] == "message_end"
            && n.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
        {
            for (i, b) in n["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let (role, field) = match b["type"].as_str() {
                    Some("text") => (Role::Assistant, "text"),
                    Some("thinking") => (Role::Reasoning, "thinking"),
                    _ => continue,
                };
                if let Some(text) = b[field].as_str() {
                    edits.push(Edit::Text {
                        role,
                        text: text.into(),
                        key: self.stream.get(&source).map(|id| block_key(id, i as u64)),
                        final_text: true,
                    });
                }
            }
        }
        if p == Protocol::Claude
            && source.path.is_empty()
            && source.session_id.as_deref() == Some(root)
            && r.events
                .iter()
                .any(|e| matches!(e, Event::TurnEnded { .. }))
            && let Some(id) = crate::wire::claude_input::result_id(n)
        {
            edits.push(Edit::NativeResult {
                id,
                inputs: crate::wire::claude_input::model_inputs(n),
            });
        }
        let mut morph_progress = None;
        if p == Protocol::Morph
            && let Some(task) = r.span_id.as_ref()
        {
            let watched = n["task_id"] == task.as_str() && r.kind == crate::RecordKind::Frame;
            let seq = if watched {
                n.pointer("/trace/entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row["seq"].as_u64())
                    .max()
                    .unwrap_or(0)
            } else {
                0
            };
            let previous = self.morph_trace.get(task).copied().unwrap_or_default();
            if morph_accepted
                || r.events.contains(&Event::TurnStarted)
                || (watched && (!edits.is_empty() || seq > previous))
            {
                if seq > previous {
                    self.morph_trace.insert(task.clone(), seq);
                }
                morph_progress = Some(MorphProgress {
                    task: task.clone(),
                    seq,
                    reasoning_bytes: watched
                        .then(|| {
                            n["reasoning"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(str::len)
                        })
                        .flatten(),
                    accepted: morph_accepted,
                    gap: seq > previous
                        && !crate::community::morph::covers_suffix(previous, &n["trace"]),
                });
            }
        }
        if r.events
            .iter()
            .any(|e| matches!(e, Event::TurnEnded { .. }))
        {
            edits.push(Edit::FinishedAt { at: r.timestamp_ms });
        }
        if edits.is_empty() && native_turn.is_none() && morph_progress.is_none() {
            None
        } else {
            Some(Frame {
                elided: r.elided,
                cursor: r.cursor(),
                morph_progress,
                native_turn,
                source,
                edits,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_recovery_requires_one_matching_root_opener_before_a_host_receipt() {
        use serde_json::json;
        let local = ChatTurn::new(Uuid::new_v4(), 0, "root", Some("same words".into()));
        let id = local.id;
        let native = json!({"kind":"agent","status":"running","turn_id":"t",
            "items":[{"type":"user_message","item_id":"u","placement":"opener","origin":{"kind":"user"},
                "prompt_ids":[id],"text":"same words"}]});
        let journal = crate::stream::Journal::new(None).unwrap();
        journal.bind_root("root");
        journal.push(
            crate::Direction::FromRuntime,
            crate::RecordKind::Frame,
            None,
            vec![],
            json!({"type":"turns","session_id":"root","agent_id":"main","turns":[native]}),
            vec![],
        );
        let mut record = (*journal.records()[0]).clone();
        let frame = Frame::kimi_recovery(&local, "root", &record).unwrap();
        assert!(
            matches!(frame.edits.as_slice(),[Edit::Activity{event: Event::NativeInput{input_id,receipt:crate::InputReceipt::Started}}] if *input_id==id)
        );
        for (path, value) in [
            ("/turns/0/items/0/placement", json!("steer")),
            ("/turns/0/items/0/origin/kind", json!("system")),
            ("/turns/0/kind", json!("system")),
            ("/turns/0/items/0/prompt_ids", json!([id, Uuid::new_v4()])),
            ("/agent_id", json!("child")),
            ("/session_id", json!("foreign")),
            ("/turns/0/items/0/prompt_ids", json!([Uuid::new_v4()])),
        ] {
            let mut other = record.clone();
            *other.native.pointer_mut(path).unwrap() = value;
            assert!(
                Frame::kimi_recovery(&local, "root", &other).is_none(),
                "{path}"
            );
        }
        record.native["turns"].as_array_mut().unwrap().push(native);
        assert!(Frame::kimi_recovery(&local, "root", &record).is_none());
    }

    #[test]
    fn native_transcript_updates_one_closed_tool_and_orders_new_siblings() {
        let root = "root";
        let mut turn = ChatTurn::new(Uuid::new_v4(), 0, root, Some("input".into()));
        let mut frame = Frame {
            morph_progress: None,
            elided: false,
            cursor: Cursor {
                stream_id: Uuid::new_v4(),
                after_seq: 2,
            },
            native_turn: Some("round".into()),
            source: turn.items[0].source.clone(),
            edits: vec![
                Edit::Activity {
                    event: Event::ToolStarted {
                        id: "call".into(),
                        name: "Read".into(),
                    },
                },
                Edit::Activity {
                    event: Event::ToolInputDelta {
                        id: "call".into(),
                        delta: "partial".into(),
                    },
                },
            ],
        };
        turn.apply(&frame, root);
        turn.finish(TurnStatus::Interrupted, root);
        let mut transcript = vec![
            Edit::Text {
                role: Role::Reasoning,
                key: Some("before".into()),
                text: "first".into(),
                final_text: true,
            },
            frame.edits[0].clone(),
            Edit::Activity {
                event: Event::ToolInput {
                    id: "call".into(),
                    input: serde_json::json!({"path":"one"}),
                },
            },
            Edit::Activity {
                event: Event::ToolEnded { id: "call".into() },
            },
        ];
        snapshot_tools(&mut transcript, false);
        frame.edits = transcript;
        turn.apply_transcript(&frame, root);
        turn.apply_transcript(&frame, root);
        assert_eq!(turn.items.len(), 3);
        assert!(matches!(
            &turn.items[1].item,
            Item::Message {
                role: Role::Reasoning,
                ..
            }
        ));
        assert!(
            matches!(&turn.items[2].item, Item::Tool{id,activity} if id=="call" && activity.finished && !activity.input_partial && activity.input==Some(serde_json::json!({"path":"one"})))
        );
    }

    #[test]
    fn dsh_reconciles_interrupted_cards_and_preserves_reused_call_ids() {
        let root = "dsh-root";
        let mut turn = ChatTurn::new(Uuid::new_v4(), 0, root, Some("question".into()));
        let mut frame = Frame {
            morph_progress: None,
            elided: false,
            cursor: Cursor {
                stream_id: Uuid::new_v4(),
                after_seq: 1,
            },
            native_turn: Some("1".into()),
            source: turn.items[0].source.clone(),
            edits: vec![],
        };
        let start = |name: &str| Event::ToolStarted {
            id: "same-id".into(),
            name: name.into(),
        };
        let input = |val: Value| Event::ToolInput {
            id: "same-id".into(),
            input: val,
        };
        let delta = |s: &str| Event::ToolInputDelta {
            id: "same-id".into(),
            delta: s.into(),
        };
        let end = || Event::ToolEnded {
            id: "same-id".into(),
        };
        let edits = |events: Vec<Event>| {
            events
                .into_iter()
                .map(|event| Edit::Activity { event })
                .collect()
        };
        frame.edits = edits(vec![
            start("read"),
            input(1.into()),
            end(),
            delta("pa"),
            start("write"),
        ]);
        turn.apply_edits(&frame, root, ToolEvents::DshStream);
        assert!(
            matches!(&turn.items[1].item, Item::Tool { activity,.. } if activity.input==Some(1.into()) && !activity.input_partial)
        );
        assert!(
            matches!(&turn.items[2].item, Item::Tool { activity,.. } if activity.input.as_ref().and_then(Value::as_str)==Some("pa") && !activity.finished)
        );
        turn.finish(TurnStatus::Interrupted, root);
        frame.edits = edits(vec![
            start("read"),
            input(1.into()),
            end(),
            delta("rtial"),
            start("write"),
            input(2.into()),
            end(),
        ]);
        turn.apply_edits(&frame, root, ToolEvents::DshHistory);
        let cards: Vec<_> = turn
            .items
            .iter()
            .filter_map(|i| match &i.item {
                Item::Tool { id, activity } => Some((
                    id.as_str(),
                    activity.name.as_deref(),
                    activity.input.as_ref(),
                    activity.finished,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            cards,
            vec![
                ("same-id", Some("read"), Some(&1.into()), true),
                ("same-id", Some("write"), Some(&2.into()), true)
            ]
        );
    }
    #[test]
    fn inserts_recovered_output_before_its_observed_root_sibling() {
        let mut turn = ChatTurn::new(Uuid::new_v4(), 0, "root", Some("question".into()));
        let source = turn.items[0].source.clone();
        turn.items.push(TimelineItem {
            source: source.clone(),
            item: Item::Tool {
                id: "read".into(),
                activity: Tool::default(),
            },
        });
        let child = TimelineItem {
            source: Agent {
                session_id: Some("child".into()),
                path: vec![],
            },
            item: Item::Message {
                role: Role::Assistant,
                provider_id: Some("reply".into()),
                text: "child".into(),
                complete: true,
            },
        };
        turn.items.push(child.clone());
        let frame = Frame {
            morph_progress: None,
            elided: false,
            cursor: Cursor {
                stream_id: Uuid::new_v4(),
                after_seq: 1,
            },
            native_turn: Some("native-user".into()),
            source,
            edits: vec![
                Edit::Text {
                    role: Role::Reasoning,
                    key: Some("thought".into()),
                    text: "before read".into(),
                    final_text: true,
                },
                Edit::ToolSnapshot {
                    id: "read".into(),
                    activity: Tool {
                        finished: true,
                        ..Tool::default()
                    },
                },
                Edit::Text {
                    role: Role::Assistant,
                    key: Some("reply".into()),
                    text: "after read".into(),
                    final_text: true,
                },
            ],
        };
        turn.apply_transcript(&frame, "root");
        assert!(matches!(&turn.items[1].item, Item::Message{text,..} if text=="before read"));
        assert!(
            matches!(&turn.items[2].item,Item::Tool{id,activity} if id=="read" && activity.finished)
        );
        assert_eq!(turn.items[3], child);
        assert!(matches!(&turn.items[4].item, Item::Message{text,..} if text=="after read"));
        assert_eq!(turn.items.len(), 5);
    }
}
