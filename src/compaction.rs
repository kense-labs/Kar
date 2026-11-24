use crate::{Event, EventCursor, Record};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A native boundary, identified even when the provider omits a compaction ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionReport {
    pub cursor: EventCursor,
    pub timestamp_ms: u64,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
impl Record {
    pub fn manual_compaction(&self) -> Option<ManualCompactionReport> {
        self.events.iter().find_map(|event| match event {
            Event::ManualCompaction { report } => Some(report.clone()),
            Event::ManualCompactionRequested => Some(ManualCompactionReport {
                request: self.cursor(),
                response: None,
                status: if self.native["type"] == "user"
                    || matches!(
                        self.native["method"].as_str(),
                        Some(
                            "session/prompt"
                                | "_x.ai/compact_conversation"
                                | "droid.compact_session"
                        )
                    ) {
                    CompactionStatus::Accepted
                } else {
                    CompactionStatus::Dispatched
                },
            }),
            _ => None,
        })
    }
    pub fn compactions(&self) -> impl Iterator<Item = CompactionReport> + '_ {
        let mut imported = self
            .upstream
            .as_ref()
            .and_then(|v| v.pointer("/body/events"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| {
                matches!(
                    v["kind"].as_str(),
                    Some("compaction_started" | "compaction_ended")
                )
            });
        self.events
            .iter()
            .enumerate()
            .filter_map(move |(index, event)| {
                let Event::Compaction { state } = event else {
                    return None;
                };
                let upstream = imported.next().unwrap_or(&Value::Null);
                let native = &self.native;
                let marker = native
                    .pointer("/params/update/_meta/opencode~1compaction")
                    .unwrap_or(&Value::Null);
                let pi = matches!(
                    native["type"].as_str(),
                    Some("compaction_start" | "compaction_end")
                );
                let dsh = native["event"]["type"]
                    .as_str()
                    .is_some_and(|t| matches!(t, "compaction/start" | "compaction/end"));
                let data = &native["event"]["data"];
                let compaction_id = upstream["compactionId"]
                    .as_str()
                    .or_else(|| native.pointer("/item/item_id").and_then(Value::as_str))
                    .or_else(|| dsh.then(|| data["compactionId"].as_str()).flatten())
                    .or_else(|| {
                        native
                            .pointer("/params/notification/summaryId")
                            .and_then(Value::as_str)
                    })
                    .or_else(|| marker["messageId"].as_str())
                    .or_else(|| native.pointer("/params/item/id").and_then(Value::as_str));
                let trigger = upstream["trigger"]
                    .as_str()
                    .or_else(|| native.pointer("/item/trigger").and_then(Value::as_str))
                    .or_else(|| (dsh && data["sourceCommandId"].is_string()).then_some("manual"))
                    .or_else(|| marker["reason"].as_str())
                    .or_else(|| {
                        native
                            .pointer("/compact_metadata/trigger")
                            .and_then(Value::as_str)
                    })
                    .or_else(|| pi.then(|| native["reason"].as_str()).flatten());
                let reason = upstream["reason"]
                    .as_str()
                    .or_else(|| dsh.then(|| data["error"].as_str()).flatten())
                    .or_else(|| native["compact_error"].as_str())
                    .or_else(|| pi.then(|| native["errorMessage"].as_str()).flatten())
                    .or_else(|| marker["error"]["message"].as_str());
                Some(CompactionReport {
                    cursor: EventCursor {
                        stream_id: self.stream_id,
                        seq: self.seq,
                        index,
                    },
                    timestamp_ms: self.timestamp_ms,
                    state: state.clone(),
                    compaction_id: compaction_id.map(str::to_owned),
                    trigger: trigger.map(str::to_owned),
                    reason: reason.map(str::to_owned),
                })
            })
    }
}

/// Status of one host-requested compaction. Evidence cursors refer to the native journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualCompactionReport {
    pub request: crate::Cursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<crate::Cursor>,
    pub status: CompactionStatus,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactionStatus {
    Dispatched,
    Accepted,
    Completed {
        frame: crate::Cursor,
    },
    Aborted {
        frame: crate::Cursor,
    },
    Rejected {
        #[serde(default)]
        code: CompactionRejection,
        reason: String,
    },
    Failed {
        code: CompactionFailure,
        reason: String,
        frame: crate::Cursor,
    },
    Unconfirmed {
        code: CompactionUnconfirmed,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionRejection {
    #[default]
    RuntimeRefused,
    Busy,
    Aborted,
    Disposed,
    RuntimeExited,
    SessionReleased,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFailure {
    NativeFailed,
    NotCompacted,
    CommandChanged,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionUnconfirmed {
    Timeout,
    Signal,
    Disposed,
    RuntimeExited,
    ControllerExited,
    ObservationFailed,
}
impl CompactionStatus {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Dispatched | Self::Accepted)
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl ManualCompactionReport {
    pub(crate) fn unconfirmed(&self, code: CompactionUnconfirmed) -> Self {
        Self {
            status: CompactionStatus::Unconfirmed { code },
            ..self.clone()
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod manual;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use manual::Operation;
#[cfg(not(target_arch = "wasm32"))]
pub use manual::{CompactionOptions, ManualCompaction};
