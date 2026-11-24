//! Project-scoped chat list state, replayable independently of provider sessions.
mod queries;
use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{
    Error, Result,
    chat::{ChatHistory, TurnStatus},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_ms: Option<u64>,
    pub pinned_ms: Option<u64>,
    pub pin_order: Option<String>,
    pub active_order: Option<String>,
    pub tags: Vec<String>,
    pub settled_override: Option<String>,
    pub settled_ms: Option<u64>,
    pub unsettled_ms: Option<u64>,
    pub auto_settle_disabled_ms: Option<u64>,
    pub snoozed_until_ms: Option<u64>,
    pub snoozed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snooze_observation: Option<SnoozeObservation>,
    pub last_wake_ms: Option<u64>,
    pub visited_ms: Option<u64>,
    pub last_opened_ms: Option<u64>,
    pub last_search_query: Option<String>,
    pub updated_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SnoozeObservation {
    command: Uuid,
    host_turn: Option<Uuid>,
    questions: Vec<crate::Cursor>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Rename {
        title: String,
    },
    Tag {
        tag: String,
        tagged: bool,
    },
    Pin {
        order: Option<String>,
    },
    Unpin,
    ReorderPin {
        order: String,
    },
    ReorderActive {
        order: String,
    },
    Settle {
        expected_turn: Option<Uuid>,
        settled_ms: Option<u64>,
    },
    AutoSettle {
        snapshot_ms: u64,
        settled_ms: Option<u64>,
    },
    Unsettle {
        restore_ms: Option<u64>,
    },
    AutoSettleEnabled {
        enabled: bool,
    },
    Snooze {
        until_ms: u64,
    },
    Unsnooze,
    Visit {
        visited_ms: u64,
        #[serde(default)]
        opened: bool,
        search_query: Option<String>,
    },
    MarkUnread,
}
#[derive(Clone, Serialize, Deserialize)]
struct Command {
    command_id: Uuid,
    #[serde(flatten)]
    action: Action,
}
#[derive(Clone, Serialize, Deserialize)]
struct Change {
    sequence: u64,
    project: Uuid,
    chat: Uuid,
    metadata: Metadata,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Store {
    pub(super) rows: BTreeMap<Uuid, Metadata>,
    changes: Vec<Change>,
    commands: BTreeMap<Uuid, (Action, usize)>,
}
impl Store {
    pub(super) fn sequence(&self) -> u64 {
        self.changes.len() as u64
    }
}
fn bad(s: &str) -> Error {
    Error::Refused(s.into())
}
fn nonempty(s: &str) -> Result<&str> {
    if s.is_empty() || s.trim() != s {
        Err(bad("value must be trimmed and nonempty"))
    } else {
        Ok(s)
    }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
impl Metadata {
    pub(super) fn acknowledges_question(&self, cursor: &crate::Cursor) -> bool {
        self.snooze_observation
            .as_ref()
            .is_some_and(|seen| seen.questions.contains(cursor))
    }
    pub(super) fn new_host_turn(&self, host: Option<(Uuid, u64)>) -> bool {
        host.is_some_and(|(id, started)| {
            self.snoozed_ms
                .is_some_and(|at| match &self.snooze_observation {
                    Some(seen) => seen.host_turn != Some(id) && started >= at,
                    None => started > at,
                })
        })
    }
    fn settle(&mut self, t: u64) {
        if self.settled_override.as_deref() != Some("settled")
            || self.settled_ms.is_none()
            || self.pinned_ms.is_some()
        {
            self.settled_ms = Some(t);
        }
        self.settled_override = Some("settled".into());
        self.pinned_ms = None;
        self.pin_order = None;
        self.active_order = None;
        self.unsettled_ms = None;
        self.snooze_observation = None;
        self.snoozed_ms = None;
        self.snoozed_until_ms = None;
    }
    fn wake(&mut self, at: u64) {
        if self.snoozed_until_ms.take().is_some() {
            self.last_wake_ms = Some(at);
        }
        self.snooze_observation = None;
        self.snoozed_ms = None;
    }
    fn mutate(
        &mut self,
        command: Uuid,
        action: &Action,
        history: &ChatHistory,
        queued: bool,
        at: u64,
    ) -> Result<()> {
        let latest = history.turns.last();
        let active = latest.is_some_and(|t| t.status == TurnStatus::Running)
            || !history.pending_requests.is_empty()
            || history.tasks.iter().any(crate::NativeTask::blocking_background);
        match action {
            Action::Rename { title } => self.title = Some(nonempty(title)?.to_owned()),
            Action::Tag { tag, tagged } => {
                nonempty(tag)?;
                if tag.encode_utf16().count() > 64 || tag.chars().any(char::is_control) {
                    return Err(bad("invalid chat tag"));
                }
                if *tagged {
                    if !self.tags.contains(tag) {
                        self.tags.push(tag.clone());
                    }
                } else {
                    self.tags.retain(|t| t != tag);
                }
            }
            Action::Pin { order } => {
                if self.pinned_ms.is_none() {
                    self.pin_order = order
                        .as_deref()
                        .map(nonempty)
                        .transpose()?
                        .map(str::to_owned);
                    self.pinned_ms = Some(at);
                }
                if self.settled_override.as_deref() == Some("settled") {
                    self.settled_override = Some("active".into());
                    self.settled_ms = None;
                    self.unsettled_ms = Some(at);
                }
                self.wake(at);
            }
            Action::Unpin => {
                self.pinned_ms = None;
                self.pin_order = None;
            }
            Action::ReorderPin { order } => {
                if self.pinned_ms.is_none() {
                    return Err(bad("chat is not pinned"));
                }
                self.pin_order = Some(nonempty(order)?.into());
            }
            Action::ReorderActive { order } => {
                self.active_order = Some(nonempty(order)?.into());
            }
            Action::Settle {
                expected_turn,
                settled_ms,
            } => {
                if expected_turn.is_some_and(|id| latest.is_none_or(|t| t.id != id)) {
                    return Err(bad("latest chat turn changed"));
                }
                if active {
                    return Err(Error::Busy);
                }
                self.settle(settled_ms.unwrap_or(at));
            }
            Action::AutoSettle {
                snapshot_ms,
                settled_ms,
            } => {
                let changed = latest
                    .map_or(history.chat.created_ms, |t| {
                        t.finished_ms.unwrap_or(t.started_ms)
                    })
                    .max(self.updated_ms);
                if active
                    || queued
                    || self.pinned_ms.is_some()
                    || self.settled_override.is_some()
                    || self.auto_settle_disabled_ms.is_some()
                    || changed > *snapshot_ms
                {
                    return Err(bad("chat changed or automatic settlement is disabled"));
                }
                self.settle(settled_ms.unwrap_or(at));
            }
            Action::Unsettle { restore_ms } => {
                self.settled_override = Some("active".into());
                self.settled_ms = None;
                self.unsettled_ms = restore_ms.or(Some(at));
                self.wake(at);
            }
            Action::AutoSettleEnabled { enabled } => {
                if *enabled {
                    self.auto_settle_disabled_ms = None;
                } else {
                    self.auto_settle_disabled_ms.get_or_insert(at);
                }
            }
            Action::Snooze { until_ms } => {
                let approval = history
                    .pending_requests
                    .iter()
                    .any(|r| r.kind != crate::RequestKind::Question || !r.agent_path.is_empty());
                let held_by_quota = latest.is_some_and(|t| {
                    t.status == TurnStatus::Failed
                        && t.failure.as_ref().is_some_and(|f| {
                            f.class == crate::FailureClass::Quota
                                && f.resets_at_ms.is_some_and(|end| end > at)
                        })
                });
                if approval || (queued && !held_by_quota) || *until_ms <= at {
                    return Err(bad(
                        "snooze requires a future time, with no pending approval or dispatchable queued input",
                    ));
                }
                self.snooze_observation = Some(SnoozeObservation {
                    command,
                    host_turn: history
                        .turns
                        .iter()
                        .rev()
                        .find(|t| !t.native_continuation)
                        .map(|t| t.id),
                    questions: history
                        .pending_requests
                        .iter()
                        .map(|r| r.cursor.clone())
                        .collect(),
                });
                self.snoozed_until_ms = Some(*until_ms);
                self.snoozed_ms = Some(at);
            }
            Action::Unsnooze => self.wake(at),
            Action::Visit {
                visited_ms,
                opened,
                search_query,
            } => {
                if search_query
                    .as_ref()
                    .is_some_and(|s| s.encode_utf16().count() > 200)
                {
                    return Err(bad("visit search query exceeds 200 characters"));
                }
                self.visited_ms = Some(self.visited_ms.unwrap_or(0).max(*visited_ms));
                if *opened && self.last_opened_ms.is_none_or(|v| v <= *visited_ms) {
                    self.last_opened_ms = Some(*visited_ms);
                    self.last_search_query = search_query.clone();
                }
            }
            Action::MarkUnread => {
                let end = history
                    .turns
                    .iter()
                    .rev()
                    .find(|t| t.status == TurnStatus::Completed)
                    .map(|t| t.finished_ms.unwrap_or(t.started_ms))
                    .ok_or_else(|| bad("chat has no completed turn to mark unread"))?;
                self.visited_ms = Some(end.saturating_sub(1));
                if let Some(until) = self.snoozed_until_ms.filter(|end| *end <= at) {
                    self.wake(until);
                }
            }
        }
        if !matches!(
            action,
            Action::ReorderActive { .. }
                | Action::Tag { .. }
                | Action::Visit { .. }
                | Action::MarkUnread
        ) {
            self.updated_ms = at;
        }
        Ok(())
    }
}

async fn change(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(req): Json<Command>,
) -> std::result::Result<Json<Change>, ApiError> {
    blocking(move || {
        let info = app.binding(p, c)?;
        let out = app.update(|db| {
            if let Some((action, index)) = db.thread_state.commands.get(&req.command_id) {
                let prior = db
                    .thread_state
                    .changes
                    .get(*index)
                    .ok_or_else(|| Error::Protocol("missing chat change receipt".into()))?;
                return if *action == req.action && prior.project == p && prior.chat == c {
                    Ok(prior.clone())
                } else {
                    Err(bad("command ID belongs to a different chat change"))
                };
            }
            let project = db.projects.get(&p).ok_or_else(super::missing)?;
            if !app.owns_chat(db, project, &info)? {
                return Err(super::missing());
            }
            if db.archived.contains(&c)
                && !matches!(req.action, Action::Visit { .. } | Action::Rename { .. })
            {
                return Err(bad("chat is archived"));
            }
            let s = &mut db.thread_state;
            let m = s.rows.entry(c).or_default();
            app.0.chats.with_admission_state(c, |history, queued| {
                m.mutate(req.command_id, &req.action, &history, queued, now())
            })?;
            let idx = s.changes.len();
            let change = Change {
                sequence: idx as u64 + 1,
                project: p,
                chat: c,
                metadata: m.clone(),
            };
            s.commands.insert(req.command_id, (req.action, idx));
            s.changes.push(change.clone());
            Ok(change)
        })?;
        app.0
            .thread_updates
            .send_modify(|seen| *seen = (*seen).max(out.sequence));
        Ok(out)
    })
    .await
}
async fn metadata(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> std::result::Result<Json<Metadata>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        Ok(app.thread_metadata(c))
    })
    .await
}
impl Workbench {
    pub(super) fn wake_sent_chat(
        &self,
        project: Uuid,
        chat: Uuid,
        seen: &Metadata,
        at: u64,
    ) -> Result<Metadata> {
        let meta = self.update(|db| {
            let store = &mut db.thread_state;
            let row = store.rows.entry(chat).or_default();
            // A send observed before a later snooze cannot wake the newer command.
            if row.snoozed_ms != seen.snoozed_ms
                || row.snoozed_until_ms != seen.snoozed_until_ms
                || row.snooze_observation.as_ref().map(|v| v.command)
                    != seen.snooze_observation.as_ref().map(|v| v.command)
            {
                return Ok((row.clone(), None));
            }
            row.snoozed_ms = None;
            row.snoozed_until_ms = None;
            row.snooze_observation = None;
            row.updated_ms = row.updated_ms.max(at);
            let metadata = row.clone();
            let sequence = store.sequence() + 1;
            store.changes.push(Change {
                sequence,
                project,
                chat,
                metadata: metadata.clone(),
            });
            Ok((metadata, Some(sequence)))
        })?;
        if let Some(sequence) = meta.1 {
            self.0
                .thread_updates
                .send_modify(|seen| *seen = (*seen).max(sequence));
        }
        Ok(meta.0)
    }
    pub(super) fn thread_metadata(&self, id: Uuid) -> Metadata {
        self.0
            .catalog
            .lock()
            .unwrap()
            .thread_state
            .rows
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{p}/chats/{c}/metadata",
            get(metadata).post(change),
        )
        .merge(queries::routes())
}
