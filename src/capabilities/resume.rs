//! Timing of native prompt settings when opening an existing session.
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvidedOnResume {
    /// The first model turn after resuming sees the supplied value.
    Immediate,
    /// The saved context remains in use until the next native compaction.
    NextCompaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OmittedOnResume {
    Saved,
    /// Uses the same defaults (including user configuration) as a new session.
    Default,
    /// Retains the saved value until native compaction, then uses the defaults.
    SavedUntilCompaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ResumeOptionSemantics {
    pub provided: ProvidedOnResume,
    pub omitted: OmittedOnResume,
}

/// Verified native resume behavior. A missing entry means the adapter makes
/// no timing declaration for that option. ChatStore normally resupplies its
/// saved settings when it opens a controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ResumedSessionOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<ResumeOptionSemantics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<ResumeOptionSemantics>,
}

impl ResumedSessionOptions {
    pub(crate) fn for_runtime(id: &str) -> Option<Self> {
        use OmittedOnResume::{Default, Saved, SavedUntilCompaction};
        use ProvidedOnResume::{Immediate, NextCompaction};
        let fresh = ResumeOptionSemantics {
            provided: Immediate,
            omitted: Default,
        };
        let retained = ResumeOptionSemantics {
            provided: Immediate,
            omitted: Saved,
        };
        let deferred = ResumeOptionSemantics {
            provided: NextCompaction,
            omitted: SavedUntilCompaction,
        };
        let (base, append) = match id {
            "claude" => (Some(deferred), deferred),
            "codex" => (Some(retained), deferred),
            "dsh" | "pi" | "qwen" => (Some(fresh), fresh),
            "pi-durable" => (None, retained),
            _ => return None,
        };
        Some(Self {
            system_prompt: base,
            append_system_prompt: Some(append),
        })
    }
}
