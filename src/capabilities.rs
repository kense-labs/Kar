//! What a host can determine from an installation before opening a native session.
#[cfg(not(target_arch = "wasm32"))]
mod installed;
mod resume;
use crate::TaskType;
pub use resume::{OmittedOnResume, ProvidedOnResume, ResumeOptionSemantics, ResumedSessionOptions};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpFirstModelRequest {
    MayStartPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpStartup {
    PerServer {
        first_model_request: McpFirstModelRequest,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContextUsageCapability {
    pub tokens: bool,
    pub context_window: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionDuringTurn {
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManualCompactionCapability {
    Supported { active_turn: CompactionDuringTurn },
    Unsupported { reason: &'static str },
}
impl ManualCompactionCapability {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) const IDLE_ONLY: Self = Self::Supported {
        active_turn: CompactionDuringTurn::Reject,
    };
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Supported { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CompactionEvents {
    pub started: bool,
    pub ended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentResultSource {
    ParentToolResult,
    ChildText,
    TaskOutput,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SubagentResults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<SubagentResultSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<SubagentResultSource>,
}

/// Missing fields require a handshake, extra dependencies or more native evidence.
/// The opened `Session::capabilities` takes precedence over this declaration.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCapabilityDeclaration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steering: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_input: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_task: Option<bool>,
    /// A supported manual compaction starts when the input slot is idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_compaction: Option<ManualCompactionCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_events: Option<CompactionEvents>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsageCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_question: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_changes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_session_file: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_transports: Option<&'static [McpTransport]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_startup: Option<McpStartup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_event_types: Option<&'static [TaskType]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_results: Option<SubagentResults>,
    /// Native behavior when changing or omitting a setting on session resume.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_session_options: Option<ResumedSessionOptions>,
    /// Settings always refused on this installation. Values can have additional constraints.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub refused_session_options: BTreeMap<&'static str, &'static str>,
}

impl SessionCapabilityDeclaration {
    /// Installation-independent declaration for a borrowed browser Pi Durable Harness.
    pub fn pi_durable() -> Self {
        Self {
            resumed_session_options: ResumedSessionOptions::for_runtime("pi-durable"),
            steering: Some(true),
            image_input: Some(false),
            stop_task: Some(false),
            manual_compaction: Some(ManualCompactionCapability::Unsupported {
                reason: "the browser harness exposes no verified manual compaction control",
            }),
            mcp_transports: Some(&[]),
            task_event_types: Some(&[]),
            context_usage: Some(ContextUsageCapability {
                tokens: false,
                context_window: false,
            }),
            refused_session_options: BTreeMap::from([
                (
                    "additional_directories",
                    "the harness owns the workspace roots",
                ),
                ("disallowed_tools", "the harness owns tool selection"),
                (
                    "end_turn_processes_on_abort",
                    "the browser owns no native process",
                ),
                ("env", "the harness owns the execution environment"),
                (
                    "exclude_user_configuration",
                    "the harness owns configuration",
                ),
                ("fork", "the browser adapter has no anchored native fork"),
                ("goals", "the harness exposes no goal control"),
                ("launch_args", "the browser owns no native process"),
                ("mcp_servers", "the harness owns the tool catalog"),
                (
                    "model_requests",
                    "the harness has no root model request capture",
                ),
                (
                    "name",
                    "the browser adapter exposes no native session naming",
                ),
                (
                    "native_memory_default",
                    "the harness owns background memory",
                ),
                (
                    "output_schema",
                    "the browser adapter has no output schema channel",
                ),
                ("pi_project_trusted", "the harness owns project resources"),
                (
                    "pi_allow_project_package_install",
                    "the harness owns package installation",
                ),
                (
                    "service_tier",
                    "the harness has no per-conversation service tier",
                ),
                ("session_log_upload", "the harness owns logging"),
                (
                    "system_prompt",
                    "the harness accepts only appended system instructions",
                ),
            ]),
            ..Self::default()
        }
    }
}
