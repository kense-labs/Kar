//! Host wait marks scoped to one native background-task occurrence.
use crate::{Cursor, NativeTask, TaskStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
    Command,
    Monitor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundWork {
    pub kind: BackgroundKind,
    pub pending: bool,
    pub awaited: bool,
    pub order: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundWaitRequest {
    pub task_ids: Vec<String>,
    #[serde(default = "enabled")]
    pub waiting: bool,
    /// UI clients can bind a selected task to the exact native start they observed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub started_at: BTreeMap<String, Cursor>,
}
impl BackgroundWaitRequest {
    pub const UNMATCHED: &str = "no matching live background task";
    pub(crate) fn validate(&self) -> crate::Result<()> {
        if !(1..=20).contains(&self.task_ids.len())
            || self
                .task_ids
                .iter()
                .any(|id| id.trim().is_empty() || id.trim() != id)
            || self.started_at.keys().any(|id| !self.task_ids.contains(id))
        {
            return Err(crate::Error::Refused(
                "expected 1 to 20 task IDs with matching optional start cursors".into(),
            ));
        }
        Ok(())
    }
}
fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundWaitReceipt {
    pub matched: Vec<String>,
    pub unmatched: Vec<String>,
}

impl NativeTask {
    pub fn pending_background(&self) -> bool {
        self.background_work.as_ref().is_some_and(|w| w.pending) && !self.status.is_terminal()
    }
    pub fn blocking_background(&self) -> bool {
        self.pending_background() && self.background_work.as_ref().is_some_and(|w| w.awaited)
    }
}

pub(crate) fn controller_lost(rows: &[NativeTask]) -> Vec<NativeTask> {
    rows.iter()
        .filter(|r| r.pending_background())
        .cloned()
        .map(|mut row| {
            row.background_work.as_mut().unwrap().pending = false;
            row.status = TaskStatus::Disconnected;
            row.ended_on_exit = true;
            row.error = Some("owning controller disconnected".into());
            row
        })
        .collect()
}
