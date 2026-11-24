//! Incremental projections over live records and recordings.
mod conversation;
pub use conversation::{
    Conversation, ConversationInput, ConversationKey, DeliveryState, InputAttempt, InputAttemptKind,
    AttemptState, DropReason, EchoEvidence, InputEcho, InputTurn, TurnPhase, conversation_of,
};
#[cfg(not(target_arch = "wasm32"))]
pub use conversation::ConversationSubscription;
mod session_state;
mod tool_groups;
mod turn_end;
pub use turn_end::{ObservedInputEnd, ObservedTurnEnd, input_end_after, turn_end_after};
pub use tool_groups::{
    ToolAction, ToolActionKind, ToolActionState, ToolCount, ToolGroupPart, ToolGroupSegment,
    group_tool_activity, tool_group_summary,
};
#[cfg(not(target_arch = "wasm32"))]
mod stall;
use crate::{Cursor, Error, Event, Outcome, Record, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use session_state::{
    AgentStatus, Phase, SessionEdge, SessionGraph, SessionReducer, SessionSnapshot,
};
#[cfg(not(target_arch = "wasm32"))]
pub use stall::{StallInfo, StallSubscription};
use std::collections::BTreeMap;

/// A projected value and the last journal sequence it consumed. An empty stream has no sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Readback<T> {
    pub seq: Option<u64>,
    pub value: T,
}

/// Text actually written as a stdio turn prompt, excluding queue/steer controls and image data.
pub(crate) fn sent_prompt_text(packet: &Value) -> Option<String> {
    let content = if packet["prompt_id"].is_string() && packet["content"].is_array() {
        &packet["content"]
    } else {
        match (packet["method"].as_str(), packet["type"].as_str()) {
            (Some("turn/start"), _) => packet.pointer("/params/input")?,
            (Some("session/prompt"), _) => packet.pointer("/params/prompt")?,
            (_, Some("user")) => packet.pointer("/message/content")?,
            (_, Some("prompt")) => packet.get("message")?,
            _ => return None,
        }
    };
    input_text(content)
}

pub(crate) fn input_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_owned());
    }
    let blocks: Vec<_> = content
        .as_array()?
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Agent {
    pub session_id: Option<String>,
    pub path: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentView {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub user_messages: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub steered_inputs: BTreeMap<uuid::Uuid, crate::SteerStatus>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_receipts: BTreeMap<uuid::Uuid, crate::InputReceipt>,
    /// Text still being streamed since the most recent complete message.
    pub partial: String,
    pub messages: Vec<String>,
    pub tools: BTreeMap<String, Tool>,
    #[serde(default)]
    pub turn_diffs: BTreeMap<String, String>,
    /// The anchor of the last finished turn, when the runtime supplied one.
    #[serde(default)]
    pub last_fork_anchor: Option<String>,
    pub latest_usage: Option<Value>,
    pub last_outcome: Option<Outcome>,
    #[serde(
        default,
        deserialize_with = "crate::stream::present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_structured_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<crate::TurnFailure>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub elided: bool,
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub finished: bool,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub preview: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub input_partial: bool,
    #[serde(default)]
    pub result: Option<crate::ToolResult>,
}
impl Tool {
    pub(crate) fn id(event: &Event) -> Option<&str> {
        match event {
            Event::ToolStarted { id, .. }
            | Event::ToolIdentity { id, .. }
            | Event::ToolEnded { id }
            | Event::ToolInputDelta { id, .. }
            | Event::ToolInput { id, .. }
            | Event::ToolProgress { id, .. }
            | Event::ToolResult { id, .. } => Some(id),
            _ => None,
        }
    }
    pub(crate) fn apply(&mut self, event: &Event, elided: bool) {
        if self.finished && matches!(event, Event::ToolStarted { .. }) {
            *self = Self::default();
        }
        self.elided |= elided;
        match event {
            Event::ToolStarted { name, .. } => self.name = Some(name.clone()),
            Event::ToolEnded { .. } => self.finished = true,
            Event::ToolIdentity {
                native_type,
                server,
                tool_name,
                ..
            } => {
                self.native_type = Some(native_type.clone());
                if server.is_some() {
                    self.server.clone_from(server);
                }
                if tool_name.is_some() {
                    self.tool_name.clone_from(tool_name);
                }
            }
            Event::ToolInputDelta { delta, .. } => {
                if !self.input_partial {
                    self.input = Some(Value::String(String::new()));
                    self.input_partial = true;
                }
                if let Some(Value::String(text)) = &mut self.input {
                    text.push_str(delta);
                }
            }
            Event::ToolInput { input, .. } => {
                self.input = Some(input.clone());
                self.input_partial = false;
            }
            Event::ToolProgress { output, delta, .. } => {
                if let Some(v) = output {
                    self.preview.clone_from(v);
                }
                if let Some(v) = delta {
                    self.preview.push_str(v);
                }
            }
            Event::ToolResult { result, .. } => {
                self.finished = true;
                self.result = Some(result.clone());
            }
            _ => {}
        }
    }
}
/// Feed one ordered stream. Duplicates, gaps and a different stream are errors.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionView {
    pub cursor: Option<Cursor>,
    pub agents: BTreeMap<Agent, AgentView>,
    pub closed: bool,
}
impl SessionView {
    pub fn apply(&mut self, rec: &Record) -> Result<()> {
        let valid = self.cursor.as_ref().map_or(rec.seq == 0, |c| {
            c.stream_id == rec.stream_id && c.after_seq.checked_add(1) == Some(rec.seq)
        });
        if !valid {
            return Err(Error::InvalidCursor);
        }
        self.cursor = Some(rec.cursor());
        // Older Rust streams have the host's submitted prompt only in their outbound native request.
        let previous_prompt = if rec.upstream.is_none()
            && rec.direction == crate::Direction::ToRuntime
            && rec.kind == crate::RecordKind::Request
            && !rec.events.iter().any(|e| {
                matches!(
                    e,
                    Event::UserMessage { .. }
                        | Event::SteeringInput { .. }
                        | Event::ManualCompactionRequested
                )
            }) {
            sent_prompt_text(&rec.native)
        } else {
            None
        };
        if rec.events.is_empty() && previous_prompt.is_none() {
            return Ok(());
        }
        let view = self
            .agents
            .entry(Agent {
                session_id: rec.session_id.clone(),
                path: rec.agent_path.clone(),
            })
            .or_default();
        if let Some(text) = previous_prompt {
            view.user_messages.push(text);
        }
        for event in &rec.events {
            match event {
                Event::Model { model } => view.model = Some(model.clone()),
                Event::Effort { effort } => view.effort = Some(effort.clone()),
                Event::UserMessage { text } => view.user_messages.push(text.clone()),
                Event::NativeInput { input_id, receipt } => {
                    view.input_receipts.insert(*input_id, *receipt);
                    if *receipt != crate::InputReceipt::Handled
                        && let Some(outcome) = receipt.settled_outcome()
                    {
                        view.last_outcome = Some(outcome);
                        view.last_failure = rec.turn_failure().cloned();
                        view.last_structured_output = None;
                        view.last_fork_anchor = None;
                    }
                }
                Event::SteeringInput {
                    input_id,
                    text,
                    status,
                    ..
                } => {
                    if view.steered_inputs.insert(*input_id, *status).is_none() {
                        view.user_messages.push(text.clone());
                    }
                }
                Event::TextDelta { text } => view.partial.push_str(text),
                Event::Message { text } => {
                    view.partial.clear();
                    view.messages.push(text.clone());
                }
                e if Tool::id(e).is_some() => {
                    view.tools
                        .entry(Tool::id(e).unwrap().to_owned())
                        .or_default()
                        .apply(e, rec.elided);
                }
                Event::TurnDiff { turn_id, diff } => {
                    view.turn_diffs.insert(turn_id.clone(), diff.clone());
                }
                Event::Usage { usage } => view.latest_usage = Some(usage.clone()),
                Event::TurnEnded { outcome } => {
                    for tool in view.tools.values_mut() {
                        tool.finished = true;
                    }
                    view.last_structured_output = rec.structured_output().cloned();
                    view.last_failure = rec.turn_failure().cloned();
                    view.last_fork_anchor = None;
                    if !view.partial.is_empty() {
                        view.messages.push(std::mem::take(&mut view.partial));
                    }
                    view.last_outcome = Some(outcome.clone());
                }
                Event::ForkAnchor { anchor } => view.last_fork_anchor = Some(anchor.clone()),
                Event::Released | Event::Exited { .. } => self.closed = true,
                _ => {}
            }
        }
        Ok(())
    }
}
