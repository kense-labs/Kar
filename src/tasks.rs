use crate::{Event, Record, observe::Agent};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskChange {
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
            ended_ms: None,
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
                    .chain(rows)
                    .find(|t: &&NativeTask| t.id == *id && t.source == source);
                let mut next = prior
                    .cloned()
                    .unwrap_or_else(|| NativeTask::new(id, source));
                next.apply(change, record.timestamp_ms);
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
                    next.status = if stopped {
                        TaskStatus::Stopped
                    } else {
                        TaskStatus::Failed
                    };
                    next.ended_ms = Some(record.timestamp_ms);
                    next.ended_on_exit = true;
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
        if let Some(row) = rows
            .iter_mut()
            .find(|t| t.id == task.id && t.source == task.source)
        {
            *row = task;
        } else {
            rows.push(task);
        }
    }
}
