//! Incremental projections over live records and recordings.
mod conversation;
#[cfg(not(target_arch = "wasm32"))]
pub use conversation::ConversationSubscription;
pub use conversation::{
    AttemptState, Conversation, ConversationInput, ConversationKey, DeliveryState, DropReason,
    EchoEvidence, InputAttempt, InputAttemptKind, InputEcho, InputTurn, TurnPhase, conversation_of,
};
mod session_state;
mod tool_groups;
mod turn_end;
pub use tool_groups::{
    ToolAction, ToolActionKind, ToolActionState, ToolCount, ToolGroupPart, ToolGroupSegment,
    group_tool_activity, tool_group_summary,
};
pub use turn_end::{ObservedInputEnd, ObservedTurnEnd, input_end_after, turn_end_after};
#[cfg(not(target_arch = "wasm32"))]
mod stall;
pub use crate::stream::coalesce_events;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_app_uri: Option<String>,
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
                mcp_app_uri,
                ..
            } => {
                self.native_type = Some(native_type.clone());
                if mcp_app_uri.is_some() {
                    self.mcp_app_uri.clone_from(mcp_app_uri);
                }
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

fn scrub_string(text: &mut String, needles: &[String]) {
    for secret in needles {
        if text.contains(secret) {
            *text = text.replace(secret, "[redacted]");
        }
    }
}

fn redact_mcp_named(value: &mut Value, needles: &[String]) {
    match value {
        Value::Array(items) => {
            for item in items {
                let has_value = item
                    .as_object()
                    .is_some_and(|obj| obj.contains_key("value"));
                redact_mcp_tree(item, needles);
                if has_value && let Some(obj) = item.as_object_mut() {
                    obj.insert("value".into(), Value::String("[redacted]".into()));
                }
            }
        }
        Value::Object(map) => {
            for val in map.values_mut() {
                *val = Value::String("[redacted]".into());
            }
        }
        Value::String(text) => scrub_string(text, needles),
        _ => {}
    }
}

fn redact_mcp_tree(value: &mut Value, needles: &[String]) {
    match value {
        Value::String(text) => scrub_string(text, needles),
        Value::Array(items) => {
            for item in items {
                redact_mcp_tree(item, needles);
            }
        }
        Value::Object(map) => {
            for (k, val) in map {
                if matches!(k.as_str(), "env" | "headers" | "http_headers") {
                    redact_mcp_named(val, needles);
                } else if matches!(k.as_str(), "bearer_token" | "bearerToken") {
                    if !val.is_null() {
                        *val = Value::String("[redacted]".into());
                    }
                } else {
                    redact_mcp_tree(val, needles);
                }
            }
        }
        _ => {}
    }
}

fn scrub_json(value: &mut Value, needles: &[String], error_fields: bool) {
    match value {
        Value::String(text) => scrub_string(text, needles),
        Value::Array(items) => {
            for item in items {
                scrub_json(item, needles, error_fields);
            }
        }
        Value::Object(obj) => {
            for (k, val) in obj {
                if matches!(k.as_str(), "requestedSchema" | "requested_schema")
                    && val["properties"].is_object()
                {
                    let mut properties = val.get_mut("properties").unwrap().take();
                    scrub_json(val, needles, error_fields);
                    for (prop_name, def) in properties.as_object_mut().unwrap() {
                        if (crate::interaction::sensitive_field(prop_name)
                            || def["writeOnly"] == true
                            || def["format"] == "password")
                            && let Some(fields) = def.as_object_mut()
                        {
                            fields.remove("default");
                            fields.remove("examples");
                        }
                        scrub_json(def, needles, error_fields);
                    }
                    val["properties"] = properties;
                } else if crate::interaction::sensitive_field(k)
                    || (error_fields
                        && matches!(
                            k.to_ascii_lowercase().as_str(),
                            "token" | "cookie" | "set-cookie" | "client_secret" | "idtoken"
                        ))
                {
                    *val = Value::String("[redacted]".into());
                } else {
                    scrub_json(val, needles, error_fields || k == "error");
                }
            }
        }
        _ => {}
    }
}

fn scrub_output_part(part: &mut crate::OutputPart, needles: &[String]) {
    match part {
        crate::OutputPart::Text { text } => scrub_string(text, needles),
        crate::OutputPart::Image { data, .. } => scrub_string(data, needles),
        crate::OutputPart::Other { value } => scrub_json(value, needles, false),
    }
}

fn scrub_event(event: &mut Event, needles: &[String]) {
    match event {
        Event::UserMessage { text }
        | Event::TextDelta { text }
        | Event::ReasoningDelta { text }
        | Event::Message { text } => scrub_string(text, needles),
        Event::SteeringInput { text, .. } => scrub_string(text, needles),
        Event::Retry {
            reason: Some(reason),
            ..
        } => scrub_string(reason, needles),
        Event::ToolInputDelta { delta, .. } => scrub_string(delta, needles),
        Event::ToolInput { input, .. } => scrub_json(input, needles, false),
        Event::ToolProgress { output, delta, .. } => {
            if let Some(o) = output {
                scrub_string(o, needles);
            }
            if let Some(d) = delta {
                scrub_string(d, needles);
            }
        }
        Event::ToolResult { result, .. } => {
            if let Some(parts) = &mut result.content {
                for part in parts {
                    scrub_output_part(part, needles);
                }
            }
            for change in &mut result.file_changes {
                scrub_string(&mut change.path, needles);
                if let Some(to) = &mut change.move_to {
                    scrub_string(to, needles);
                }
            }
        }
        Event::TurnDiff { diff, .. } => scrub_string(diff, needles),
        Event::Task { change, .. } => match change {
            crate::TaskChange::Background { work } => {
                if let Some(d) = &mut work.description {
                    scrub_string(d, needles);
                }
                if let Some(c) = &mut work.command {
                    scrub_string(c, needles);
                }
            }
            crate::TaskChange::Started { details, .. } => {
                if let Some(d) = &mut details.description {
                    scrub_string(d, needles);
                }
            }
            crate::TaskChange::Updated {
                description, error, ..
            } => {
                if let Some(d) = description {
                    scrub_string(d, needles);
                }
                if let Some(e) = error {
                    scrub_string(e, needles);
                }
            }
            crate::TaskChange::Ended {
                summary,
                output,
                output_file,
                ..
            } => {
                if let Some(s) = summary {
                    scrub_string(s, needles);
                }
                if let Some(o) = output {
                    scrub_string(o, needles);
                }
                if let Some(f) = output_file {
                    scrub_string(f, needles);
                }
            }
        },
        Event::StructuredOutput { value } | Event::Usage { usage: value } => {
            scrub_json(value, needles, false);
        }
        Event::GoalChanged { goal: Some(goal) } => {
            scrub_string(&mut goal.objective, needles);
            scrub_string(&mut goal.status, needles);
        }
        Event::TurnFailure { failure } => scrub_string(&mut failure.reason, needles),
        Event::RequestDeliveryFailed { reason, .. } | Event::Exited { reason } => {
            scrub_string(reason, needles);
        }
        Event::ManualCompaction { report } => match &mut report.status {
            crate::CompactionStatus::Rejected { reason, .. }
            | crate::CompactionStatus::Failed { reason, .. } => scrub_string(reason, needles),
            _ => {}
        },
        Event::TurnProcessCleanup { cleanup } => {
            if let crate::ProcessCleanupResult::Failed { reason, .. } = &mut cleanup.result {
                scrub_string(reason, needles);
            }
        }
        _ => {}
    }
}

/// Recursively scrub secret substrings and credential-bearing notification fields from a [`Record`].
///
/// Pure and idempotent: returns a sanitized clone without mutating `record`. Both raw JSON strings
/// and JSON-escaped secret substrings embedded inside serialized raw lines are scrubbed.
pub fn redact_record(record: &Record, secrets: &[String]) -> Record {
    let mut needles = Vec::new();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        needles.push(secret.clone());
        if let Ok(quoted) = serde_json::to_string(secret)
            && let Some(escaped) = quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
            && escaped != secret
            && !escaped.is_empty()
        {
            needles.push(escaped.to_owned());
        }
    }
    needles.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    needles.dedup();

    let mut out = record.clone();
    let frame_type = out.frame_type().map(str::to_owned);

    scrub_json(&mut out.native, &needles, false);
    if let Some(ft) = frame_type.as_deref() {
        if ft.starts_with("_x.ai/mcp/") || ft.starts_with("_x.ai/mcp_initialized") {
            redact_mcp_tree(&mut out.native, &needles);
        } else if ft == "_codebuddy.ai/authUrl" {
            if out.native["authUrl"].is_string() {
                out.native["authUrl"] = Value::String("[redacted]".into());
            }
            if out.native["params"]["authUrl"].is_string() {
                out.native["params"]["authUrl"] = Value::String("[redacted]".into());
            }
        }
    }

    if let Some(upstream) = &mut out.upstream {
        scrub_json(upstream, &needles, false);
        if let Some(ft) = frame_type.as_deref()
            && let Some(native) = upstream.pointer_mut("/body/native")
        {
            if ft.starts_with("_x.ai/mcp/") || ft.starts_with("_x.ai/mcp_initialized") {
                redact_mcp_tree(native, &needles);
            } else if ft == "_codebuddy.ai/authUrl" && native["authUrl"].is_string() {
                native["authUrl"] = Value::String("[redacted]".into());
            }
        }
    }

    if let Some(origin) = &mut out.origin
        && let Some(source) = &mut origin.source
    {
        scrub_string(source, &needles);
    }

    if let Some(prompt) = &mut out.prompt
        && let Some(submission) = &mut prompt.submission
    {
        scrub_string(&mut submission.input, &needles);
        for img in &mut submission.images {
            if let Some(path) = &mut img.path {
                scrub_string(path, &needles);
            }
        }
    }

    for echo in &mut out.elided_input_echoes {
        scrub_string(&mut echo.text, &needles);
    }

    for event in &mut out.events {
        scrub_event(event, &needles);
    }

    out
}

/// Format a [`crate::FailureClass`] in English with a named runtime, matching Oar's `failureText`.
pub fn failure_class_display_text(
    class: crate::FailureClass,
    runtime_name: &str,
    credential: Option<crate::CredentialProblem>,
) -> String {
    use crate::{CredentialProblem::*, FailureClass::*};
    match class {
        Auth => match credential {
            Some(Missing) => format!("{runtime_name} is not signed in."),
            Some(Rejected) => format!("{runtime_name}'s credentials were rejected."),
            None => format!("{runtime_name} could not authenticate."),
        },
        Quota => format!("{runtime_name} reported that its usage limit was reached."),
        RateLimited => format!("{runtime_name} was rate limited by its provider."),
        Billing => format!("{runtime_name} reported a billing or credit problem."),
        ModelUnavailable => format!("{runtime_name} cannot use the selected model."),
        InputTooLarge => format!("The input is too large for {runtime_name}."),
        LimitReached => {
            format!("{runtime_name} stopped at the turn or budget limit set for it.")
        }
        OutputSchema => {
            format!("{runtime_name} could not produce output matching the output schema.")
        }
        ModelRefused => format!("The model used by {runtime_name} declined to answer."),
        InvalidRequest => format!("{runtime_name} rejected the request as invalid."),
        Overloaded => format!("{runtime_name} reported that its provider is overloaded."),
        Provider => format!("{runtime_name} reported a provider error."),
        RuntimeExited => format!("{runtime_name} exited before the turn finished."),
        Unknown => "The turn failed.".into(),
    }
}

/// Format a [`crate::TurnFailure`] into a concise, human-readable English display string.
pub fn failure_display_text(failure: &crate::TurnFailure) -> String {
    use crate::{CredentialProblem::*, FailureClass::*};
    let summary = match failure.class {
        Auth => match failure.credential {
            Some(Missing) => "Authentication failed (credentials missing)",
            Some(Rejected) => "Authentication failed (credentials rejected)",
            None => "Authentication failed",
        },
        Quota => "Usage limit reached",
        RateLimited => "Rate limited by provider",
        Billing => "Billing or credit problem",
        ModelUnavailable => "Selected model is unavailable",
        InputTooLarge => "Input exceeds model context or size limit",
        LimitReached => "Turn or budget limit reached",
        OutputSchema => "Output did not match requested schema",
        ModelRefused => "Model declined to answer",
        InvalidRequest => "Invalid request rejected",
        Overloaded => "Provider is overloaded",
        Provider => "Provider error",
        RuntimeExited => "Runtime exited before turn finished",
        Unknown => "Turn failed",
    };
    let reason = failure.reason.trim();
    if reason.is_empty() || summary.eq_ignore_ascii_case(reason) {
        if let Some(code) = failure.status {
            format!("{summary} (HTTP {code})")
        } else {
            summary.to_owned()
        }
    } else if let Some(code) = failure.status
        && !reason.contains(&code.to_string())
    {
        format!("{summary} (HTTP {code}): {reason}")
    } else {
        format!("{summary}: {reason}")
    }
}

/// Human-readable English display label for a running or message phase identifier.
pub fn phase_display_label(phase: &str) -> &'static str {
    match phase.trim() {
        "waiting_model" => "Waiting for model",
        "thinking" | "reasoning" => "Thinking",
        "responding" => "Responding",
        "compacting" => "Compacting context",
        "tool" => "Running tool",
        "commentary" => "Commentary",
        "final_answer" | "final" => "Final answer",
        "planning" | "plan" => "Planning",
        "idle" => "Idle",
        "running" => "Running",
        _ => "Working",
    }
}

#[cfg(test)]
mod display_and_redaction_tests {
    use super::*;
    use crate::{
        CredentialProblem, Direction, FailureClass, OutputPart, RecordKind, ToolResult, ToolStatus,
        TurnFailure, stream::Journal,
    };
    use serde_json::json;

    #[test]
    fn redact_record_scrubs_secrets_raw_escaped_lines_and_credential_notifications() {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("sess");
        let secret = "sk-live-super-secret-value".to_owned();
        let quoted_secret = "tok\"with\\quote".to_owned();
        let raw_line = serde_json::to_string(&json!({"header": &quoted_secret})).unwrap();

        journal.push(
            crate::stream::RecordStamp::frame(Direction::FromRuntime, "_x.ai/mcp/servers_updated"),
            RecordKind::Frame,
            Some("sess".into()),
            vec![],
            json!({
                "servers": [{"name": "db", "env": {"DB_URL": "postgres://secret"}, "bearer_token": "tok-123"}],
                "rawLine": raw_line,
                "api_key": "inline-key",
                "note": format!("leaked {secret} here"),
            }),
            vec![
                Event::TextDelta {
                    text: format!("assistant saw {secret}"),
                },
                Event::ToolInput {
                    id: "c1".into(),
                    input: json!({"cmd": format!("curl -H {secret}")}),
                },
                Event::ToolResult {
                    id: "c1".into(),
                    result: ToolResult {
                        status: Some(ToolStatus::Failed),
                        content: Some(vec![OutputPart::Text {
                            text: format!("err: {secret} and {raw_line}"),
                        }]),
                        ..Default::default()
                    },
                },
            ],
        );

        let original = journal.records().pop().unwrap();
        let secrets = vec![secret.clone(), quoted_secret.clone()];
        let cleaned = redact_record(&original, &secrets);
        let serialized = serde_json::to_string(&cleaned).unwrap();
        assert!(!serialized.contains(&secret));
        assert!(!serialized.contains("tok\\\"with\\\\quote"));
        assert!(!serialized.contains("postgres://secret"));
        assert!(!serialized.contains("tok-123"));
        assert!(!serialized.contains("inline-key"));
        assert_eq!(cleaned.native["servers"][0]["env"]["DB_URL"], "[redacted]");
        assert_eq!(cleaned.native["servers"][0]["bearer_token"], "[redacted]");
        // Idempotence: running a second time produces the exact same JSON.
        let cleaned_twice = redact_record(&cleaned, &secrets);
        assert_eq!(
            serde_json::to_value(&cleaned).unwrap(),
            serde_json::to_value(&cleaned_twice).unwrap()
        );
    }

    #[test]
    fn failure_and_phase_display_helpers_cover_all_variants() {
        let mut auth = TurnFailure::new(FailureClass::Auth, "token expired");
        auth.status = Some(401);
        auth.credential = Some(CredentialProblem::Rejected);
        let rendered = failure_display_text(&auth);
        assert!(rendered.contains("credentials rejected"));
        assert!(rendered.contains("HTTP 401"));
        assert!(rendered.contains("token expired"));

        assert_eq!(
            failure_class_display_text(
                FailureClass::Auth,
                "Codex",
                Some(CredentialProblem::Missing)
            ),
            "Codex is not signed in."
        );
        assert_eq!(
            failure_class_display_text(FailureClass::Quota, "Claude", None),
            "Claude reported that its usage limit was reached."
        );

        assert_eq!(phase_display_label("waiting_model"), "Waiting for model");
        assert_eq!(phase_display_label("thinking"), "Thinking");
        assert_eq!(phase_display_label("responding"), "Responding");
        assert_eq!(phase_display_label("compacting"), "Compacting context");
        assert_eq!(phase_display_label("tool"), "Running tool");
        assert_eq!(phase_display_label("commentary"), "Commentary");
        assert_eq!(phase_display_label("final_answer"), "Final answer");
        assert_eq!(phase_display_label("custom_unknown"), "Working");
    }
}
