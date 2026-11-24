pub(crate) mod background;
use crate::{Event, Record, observe::Agent};
pub use background::{
    BackgroundKind, BackgroundWaitReceipt, BackgroundWaitRequest, BackgroundWork,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Shell,
    Agent,
    Tool,
    Workflow,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Stopped,
    /// Cursor stopped observing this child. The underlying agent's outcome is unknown.
    Disconnected,
}
impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Stopped | Self::Disconnected
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "nativeType")]
    pub native_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "toolCallId")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "childSessionId")]
    pub child_session_id: Option<String>,
    /// Initial child model from the native task-start frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskChange {
    Background {
        work: BackgroundWork,
    },
    Started {
        #[serde(alias = "taskType")]
        task_type: TaskType,
        #[serde(flatten)]
        details: TaskDetails,
    },
    Updated {
        status: Option<TaskStatus>,
        background: Option<bool>,
        description: Option<String>,
        error: Option<String>,
    },
    Ended {
        status: TaskStatus,
        summary: Option<String>,
        output: Option<String>,
        #[serde(alias = "outputFile")]
        output_file: Option<String>,
    },
}

/// One native background task as reported by its owning session and agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeTask {
    pub id: String,
    pub source: Agent,
    pub task_type: TaskType,
    pub status: TaskStatus,
    #[serde(flatten)]
    pub details: TaskDetails,
    pub started_ms: Option<u64>,
    /// The native start record distinguishes reused task IDs, including starts in the same millisecond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<crate::Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_work: Option<BackgroundWork>,
    pub ended_ms: Option<u64>,
    pub summary: Option<String>,
    pub output: Option<String>,
    pub output_file: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ended_on_exit: bool,
}

impl NativeTask {
    fn new(id: &str, source: Agent) -> Self {
        Self {
            id: id.into(),
            source,
            task_type: TaskType::Other,
            status: TaskStatus::Running,
            details: TaskDetails::default(),
            started_ms: None,
            started_at: None,
            ended_ms: None,
            background_work: None,
            summary: None,
            output: None,
            output_file: None,
            error: None,
            ended_on_exit: false,
        }
    }
    fn apply(&mut self, change: &TaskChange, at: u64) {
        if self.ended_on_exit
            && !matches!(
                change,
                TaskChange::Updated {
                    status: None,
                    error: None,
                    ..
                }
            )
        {
            if self.error.as_deref() == Some("runtime exited") {
                self.error = None;
            }
            self.ended_on_exit = false;
        }
        match change {
            TaskChange::Background { work } => {
                if work.pending && self.status.is_terminal() {
                    *self = Self::new(&self.id, self.source.clone());
                }
                if self.started_ms.is_none() {
                    self.started_ms = Some(at);
                }
                if self.task_type == TaskType::Other {
                    self.task_type = TaskType::Shell;
                }
                if let Some(description) = &work.description {
                    self.details.description = Some(description.clone());
                }
                if let Some(tool) = &work.tool_use_id {
                    self.details.tool_call_id = Some(tool.clone());
                }
                if !work.pending && !self.status.is_terminal() {
                    self.status = TaskStatus::Disconnected;
                    self.ended_ms = Some(at);
                }
                self.background_work = Some(work.clone());
            }
            TaskChange::Started { task_type, details } => {
                if self.started_ms.is_some() {
                    *self = Self::new(&self.id, self.source.clone());
                }
                self.started_ms = Some(at);
                self.task_type = *task_type;
                if details.background.is_some() {
                    self.details.background = details.background;
                }
                if details.description.is_some() {
                    self.details.description.clone_from(&details.description);
                }
                self.details.native_type.clone_from(&details.native_type);
                self.details.tool_call_id.clone_from(&details.tool_call_id);
                self.details
                    .child_session_id
                    .clone_from(&details.child_session_id);
                self.details.model.clone_from(&details.model);
                self.details.ambient = details.ambient;
            }
            TaskChange::Updated {
                status,
                background,
                description,
                error,
            } => {
                if let Some(status) = status {
                    self.status = *status;
                    if !status.is_terminal() {
                        self.ended_ms = None;
                        self.output = None;
                        self.summary = None;
                        self.output_file = None;
                        self.error = None;
                    }
                }
                if background.is_some() {
                    self.details.background = *background;
                }
                if description.is_some() {
                    self.details.description.clone_from(description);
                }
                if error.is_some() {
                    self.error.clone_from(error);
                }
            }
            TaskChange::Ended {
                status,
                summary,
                output,
                output_file,
            } => {
                self.status = *status;
                self.ended_ms = Some(at);
                self.output.clone_from(output);
                self.summary.clone_from(summary);
                self.output_file.clone_from(output_file);
            }
        }
    }
}

/// Apply reports in wire order. Retain insertion order for task panels and attribution for nested tasks.
pub(crate) fn changes(rows: &[NativeTask], record: &Record, root: bool) -> Vec<NativeTask> {
    let mut changed = Vec::new();
    for e in &record.events {
        match e {
            Event::Task { id, change } => {
                let source = Agent {
                    session_id: record.session_id.clone(),
                    path: record.agent_path.clone(),
                };
                let prior = changed
                    .iter()
                    .rev()
                    .chain(rows.iter().rev())
                    .find(|t: &&NativeTask| t.id == *id && t.source == source);
                let mut next = prior
                    .cloned()
                    .unwrap_or_else(|| NativeTask::new(id, source));
                let start = matches!(change, TaskChange::Started { .. })
                    || matches!(change, TaskChange::Background { work } if work.pending)
                        && (next.started_at.is_none() || next.status.is_terminal());
                next.apply(change, record.timestamp_ms);
                if next.status.is_terminal()
                    && let Some(work) = &mut next.background_work
                {
                    work.pending = false;
                }
                if start {
                    next.started_at = Some(record.cursor());
                }
                if prior != Some(&next) {
                    changed.push(next);
                }
            }
            Event::Exited { reason } if root => {
                let stopped = matches!(
                    reason.as_str(),
                    "closed by host" | "last session handle dropped" | "session closed"
                );
                for prior in rows.iter().filter(|t| !t.status.is_terminal()) {
                    let mut next = prior.clone();
                    next.status = if next.pending_background() {
                        TaskStatus::Disconnected
                    } else if stopped {
                        TaskStatus::Stopped
                    } else {
                        TaskStatus::Failed
                    };
                    next.ended_ms = Some(record.timestamp_ms);
                    next.ended_on_exit = true;
                    if let Some(work) = &mut next.background_work {
                        work.pending = false;
                    }
                    if !stopped && next.error.is_none() {
                        next.error = Some("runtime exited".into());
                    }
                    changed.push(next);
                }
            }
            _ => {}
        }
    }
    changed
}
pub(crate) fn merge(rows: &mut Vec<NativeTask>, updates: Vec<NativeTask>) {
    for task in updates {
        let latest = rows
            .iter_mut()
            .rev()
            .find(|row| row.id == task.id && row.source == task.source);
        if let Some(row) = latest
            && (row.background_work.is_none() || row.started_at == task.started_at)
        {
            *row = task;
        } else {
            // Keep the previous occurrence, including the user turn and wait mark lost on restart.
            rows.push(task);
        }
    }
}
