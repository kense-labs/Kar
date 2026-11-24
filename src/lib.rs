#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]
#[cfg(not(target_arch = "wasm32"))]
pub mod account;
pub mod brands;
mod compaction;
pub mod credentials;
pub mod forms;
#[cfg(not(target_arch = "wasm32"))]
pub mod host_tools;
#[cfg(not(target_arch = "wasm32"))]
pub use host_tools::serve_mcp_tools;
#[cfg(not(target_arch = "wasm32"))]
pub mod inventory;
#[cfg(not(target_arch = "wasm32"))]
pub mod orientation;
#[cfg(not(target_arch = "wasm32"))]
mod private_dir;
#[cfg(not(target_arch = "wasm32"))]
mod process;
#[cfg(not(target_arch = "wasm32"))]
pub mod providers;
#[cfg(not(target_arch = "wasm32"))]
mod terminal;
pub use compaction::{
    CompactionFailure, CompactionRejection, CompactionReport, CompactionStatus,
    CompactionUnconfirmed, ManualCompactionReport,
};
#[cfg(not(target_arch = "wasm32"))]
pub use compaction::{CompactionOptions, ManualCompaction};
mod context;
mod error;
mod open_error;
pub use open_error::{
    NativeCause, OpenError, ResumeCwdReason, RuntimeFailureCode, UnavailableReason,
};
#[cfg(not(target_arch = "wasm32"))]
mod opening;
pub use context::{ContextBreakdown, ContextCategory, ContextItem, ContextKind, ContextUsage};
#[cfg(not(target_arch = "wasm32"))]
mod acp_models;
#[cfg(not(target_arch = "wasm32"))]
mod cursor_models;
mod failure;
mod goal;
#[cfg(not(target_arch = "wasm32"))]
mod grok;
pub use failure::{CredentialProblem, FailureAdvice, FailureClass, RetryTiming, TurnFailure};
#[cfg(not(target_arch = "wasm32"))]
pub use goal::GoalSetOptions;
pub use goal::GoalSnapshot;
mod permissions;
pub use permissions::{PermissionResource, PolicyChoice, PolicyOption, ResourceId};
mod questions;
pub use questions::{FieldKind, QuestionField, QuestionForm, QuestionOption, QuestionReply};
mod interaction;
pub use interaction::{PendingRequest, PermissionDecision, RequestControls, RequestKind};
#[cfg(not(target_arch = "wasm32"))]
mod antigravity;
mod placement;
pub use placement::{Execution, Opening, PlacementRequest, SessionPlacement};
mod capabilities;
pub use capabilities::{
    CompactionDuringTurn, CompactionEvents, ContextUsageCapability, ManualCompactionCapability,
    McpFirstModelRequest, McpStartup, McpTransport, OmittedOnResume, ProvidedOnResume,
    ResumeOptionSemantics, ResumedSessionOptions, SessionCapabilityDeclaration,
    SubagentResultSource, SubagentResults,
};
#[cfg(not(target_arch = "wasm32"))]
mod options;
#[cfg(not(target_arch = "wasm32"))]
pub use options::{
    McpServer, NativeMemory, NativeMemoryDefault, SessionLogUpload, SessionSettings,
    UserConfigurationCategory,
};
#[cfg(not(target_arch = "wasm32"))]
pub mod chat;
pub mod observe;
mod tasks;
mod usage;
#[cfg(not(target_arch = "wasm32"))]
pub mod workbench;
#[cfg(not(target_arch = "wasm32"))]
pub mod workbench_git;
pub use usage::{
    AgentTokens, CacheWriteByTtl, SessionUsage, TokenReading, TokenTotals, UsageSnapshot,
};
mod tool_activity;
pub use tasks::{NativeTask, TaskChange, TaskDetails, TaskStatus, TaskType};
pub use tool_activity::{ChangeKind, FileChange, OutputPart, ToolResult, ToolStatus};
#[cfg(not(target_arch = "wasm32"))]
mod dsh;
#[cfg(not(target_arch = "wasm32"))]
mod factory;
#[cfg(not(target_arch = "wasm32"))]
mod kimi;
#[cfg(not(target_arch = "wasm32"))]
mod model_request;
#[cfg(not(target_arch = "wasm32"))]
mod native_file;
#[cfg(not(target_arch = "wasm32"))]
mod pi_fork;
#[cfg(not(target_arch = "wasm32"))]
mod pi_mcp;
#[cfg(not(target_arch = "wasm32"))]
mod pi_sdk;
#[cfg(not(target_arch = "wasm32"))]
mod qoder;
#[cfg(not(target_arch = "wasm32"))]
pub use model_request::{ModelRequest, ModelRequests};
#[cfg(not(target_arch = "wasm32"))]
mod resources;
#[cfg(not(target_arch = "wasm32"))]
pub use resources::{ResourceScope, SessionResources};
#[cfg(not(target_arch = "wasm32"))]
mod runtime;
#[cfg(not(target_arch = "wasm32"))]
mod session;
mod stream;
#[cfg(not(target_arch = "wasm32"))]
mod transport;
#[cfg(not(target_arch = "wasm32"))]
mod wire;
pub use error::{Error, Result};
#[cfg(not(target_arch = "wasm32"))]
pub use runtime::{
    AcpTimeouts, ApprovalMode, McpToolsListChanged, Protocol, Runtime, SessionOptions, runtime,
    runtimes,
};
#[cfg(not(target_arch = "wasm32"))]
pub use session::{
    Capabilities, Delivery, PromptDecision, PromptInterrupt, PromptRun, PromptRunOptions, Session,
    Turn, TurnReport,
};
pub use stream::{
    ControlReceipt, Cursor, Direction, Event, EventCursor, EventSubscription, ImageReference,
    ImageSubmission, InputReceipt, McpStartupStatus, ObservedEvent, Outcome, ProcessCleanupResult,
    PromptIdentity, Record, RecordKind, SteerStatus, Subscription, TurnProcessCleanup,
};

#[cfg(not(target_arch = "wasm32"))]
mod installation;
#[cfg(not(target_arch = "wasm32"))]
pub use installation::{
    InstallationIssue, InstallationOptions, InstallationSnapshot, InstallationSource,
    clear_installation_cache,
};
#[cfg(not(target_arch = "wasm32"))]
mod management;
#[cfg(not(target_arch = "wasm32"))]
pub use management::{AccountAction, Catalog, Installation, Inventory};

#[cfg(not(target_arch = "wasm32"))]
pub mod agents;
#[cfg(not(target_arch = "wasm32"))]
pub mod community;

#[cfg(not(target_arch = "wasm32"))]
mod side_question;
#[cfg(not(target_arch = "wasm32"))]
pub use side_question::SideQuestionOptions;
#[cfg(not(target_arch = "wasm32"))]
mod setup;
#[cfg(not(target_arch = "wasm32"))]
pub use setup::{InstallPlan, UpdateCheck};

#[cfg(not(target_arch = "wasm32"))]
mod input;
#[cfg(not(target_arch = "wasm32"))]
pub use input::InputImage;

pub mod voyage;

#[cfg(target_arch = "wasm32")]
pub mod browser;
