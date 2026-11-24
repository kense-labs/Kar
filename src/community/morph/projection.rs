use crate::{Event, Outcome, OutputPart, ToolResult, ToolStatus};
use serde_json::{Value, json};

// Morph's shell tools publish a four-line observation header and a separate stderr section.
fn shell_exit_code(tool: &str, output: &str) -> Option<i64> {
    if !matches!(tool, "bash" | "powershell") {
        return None;
    }
    let (code, rest) = output.strip_prefix("exit_code: ")?.split_once('\n')?;
    let digits = code.strip_prefix('-').unwrap_or(code);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let code = code.parse::<i64>().ok()?;
    let (stdout, rest) = rest.strip_prefix("stdout_truncated: ")?.split_once('\n')?;
    let (stderr, rest) = rest.strip_prefix("stderr_truncated: ")?.split_once('\n')?;
    (matches!(stdout, "true" | "false")
        && matches!(stderr, "true" | "false")
        && rest.strip_prefix("stdout:\n")?.contains("\n\nstderr:\n")
        && code.unsigned_abs() < 1 << 53)
        .then_some(code)
}

#[cfg(test)]
#[test]
fn exit_code_requires_the_shell_tools_complete_observation() {
    let sample = "exit_code: -1\nstdout_truncated: true\nstderr_truncated: false\nstdout:\nexit_code: 55\n\nstderr:\n";
    assert_eq!(shell_exit_code("powershell", sample), Some(-1));
    assert_eq!(shell_exit_code("read_file", sample), None);
    for invalid in [
        sample.replace("-1", "+3"),
        sample.replace("-1", "9007199254740992"),
        sample.replace("true", "yes"),
        sample.replace("\n\nstderr:\n", ""),
        format!("prefix\n{sample}"),
    ] {
        assert_eq!(shell_exit_code("bash", &invalid), None);
    }
}

fn invocation(args: &Value) -> Option<String> {
    let fields = args.as_object().filter(|a| !a.is_empty())?;
    if fields.len() == 1
        && let Some(command) = args["cmd"].as_str().or(args["command"].as_str())
    {
        return Some(command.into());
    }
    Some(args.to_string())
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Projection {
    seq: u64,
    calls: Vec<(String, String)>,
    text: String,
    reasoning: String,
    messages: Vec<String>,
    model: String,
}
// The native collector counts evicted entries and may retain older subtask rows.
// A suffix is usable only when every entry after our durable cursor is present.
pub(crate) fn covers_suffix(after: u64, trace: &Value) -> bool {
    let rows = trace["entries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let Some(total) = (rows.len() as u64).checked_add(trace["omitted"].as_u64().unwrap_or(0))
    else {
        return false;
    };
    let mut cursor = after;
    for row in rows {
        let Some(seq) = row["seq"].as_u64() else {
            return false;
        };
        if seq > after {
            if cursor.checked_add(1) != Some(seq) {
                return false;
            }
            cursor = seq;
        }
    }
    cursor == total
}
impl Projection {
    pub(super) fn cursor(&self) -> u64 {
        self.seq
    }
    pub(super) fn open_tools(&self) -> Vec<String> {
        self.calls.iter().map(|(id, _)| id.clone()).collect()
    }

    pub(super) fn covers_remaining(&self, trace: &Value) -> bool {
        covers_suffix(self.seq, trace)
    }
    pub(super) fn replay(
        &mut self,
        edits: &[crate::chat::ChatEdit],
        seq: u64,
        reasoning_bytes: Option<usize>,
    ) -> bool {
        use crate::chat::{ChatEdit, Role};
        for edit in edits {
            match edit {
                ChatEdit::Text {
                    role: Role::Assistant,
                    text,
                    final_text,
                    ..
                } => {
                    if *final_text {
                        self.text.clear();
                    } else {
                        self.text.push_str(text);
                    }
                }
                ChatEdit::Text {
                    role: Role::Reasoning,
                    text,
                    ..
                } => self.reasoning.push_str(text),
                ChatEdit::Activity {
                    event: Event::ToolStarted { id, name },
                } => self.calls.push((id.clone(), name.clone())),
                ChatEdit::Activity {
                    event: Event::ToolEnded { id },
                } => {
                    if let Some(i) = self.calls.iter().rposition(|(call, _)| call == id) {
                        self.calls.remove(i);
                    }
                }
                _ => {}
            }
        }
        self.seq = self.seq.max(seq);
        // The native snapshot may replace its reasoning. Earlier fragments remain on the timeline.
        if let Some(n) = reasoning_bytes {
            let Some(start) = self.reasoning.len().checked_sub(n) else {
                return false;
            };
            if !self.reasoning.is_char_boundary(start) {
                return false;
            }
            self.reasoning.drain(..start);
        }
        true
    }
    fn trace(&mut self, task: &str, trace: &Value, events: &mut Vec<Event>) {
        for row in trace["entries"].as_array().into_iter().flatten() {
            let Some(seq) = row["seq"].as_u64().filter(|n| *n > self.seq) else {
                continue;
            };
            let e = &row["event"];
            self.seq = seq;
            self.model_event(e, events);
            let tool = e["tool_name"].as_str().unwrap_or("tool");
            let native = e["activity_id"].as_str().filter(|s| !s.is_empty());
            let open = self
                .calls
                .iter()
                .rposition(|(id, name)| native.map_or(name == tool, |n| n == id));
            match e["kind"].as_str() {
                Some("tool_start") => {
                    let id = native
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("{task}:{seq}"));
                    self.calls.push((id.clone(), tool.into()));
                    events.push(Event::ToolStarted {
                        id: id.clone(),
                        name: tool.into(),
                    });
                    if let Some(input) = invocation(&e["args"]) {
                        events.push(Event::ToolInput {
                            id,
                            input: input.into(),
                        });
                    }
                }
                Some("tool_output") => {
                    if let Some(index) = open {
                        let id = &self.calls[index].0;
                        events.push(Event::ToolUpdated { id: id.clone() });
                        if let Some(output) = e["text"].as_str().filter(|s| !s.is_empty()) {
                            events.push(Event::ToolProgress {
                                id: id.clone(),
                                output: Some(output.into()),
                                delta: None,
                            });
                        }
                    }
                }
                Some("tool_done") => {
                    let id = open
                        .map(|i| self.calls.remove(i).0)
                        .or_else(|| native.map(str::to_owned))
                        .unwrap_or_else(|| format!("{task}:{seq}"));
                    let text = e["text"].as_str().filter(|s| !s.is_empty());
                    let failed = e["error"].as_str().is_some_and(|s| !s.is_empty())
                        || matches!(e["status"].as_str(), Some("failed" | "error"));
                    let status = if failed {
                        Some(ToolStatus::Failed)
                    } else if e["status"] == "done" {
                        Some(ToolStatus::Succeeded)
                    } else {
                        None
                    };
                    if text.is_some() || status.is_some() {
                        events.push(Event::ToolResult {
                            id: id.clone(),
                            result: ToolResult {
                                status,
                                exit_code: text.and_then(|s| shell_exit_code(tool, s)),
                                content: text.map(|s| vec![OutputPart::Text { text: s.into() }]),
                                ..Default::default()
                            },
                        });
                    }
                    events.push(Event::ToolEnded { id });
                }
                Some("steer_applied") => {
                    if let Some(text) = e["text"].as_str() {
                        events.push(Event::UserMessage { text: text.into() });
                    }
                }
                Some(
                    "context_compaction_start"
                    | "context_compaction_done"
                    | "context_compaction_failed",
                ) => {
                    let state = match e["kind"].as_str() {
                        Some("context_compaction_start") => "started",
                        Some("context_compaction_done") => "completed",
                        _ => "failed",
                    };
                    events.push(Event::Compaction {
                        state: state.into(),
                    });
                }
                Some("llm_retry") => {
                    if let Some(reason) = e["text"].as_str() {
                        events.push(Event::Retry {
                            attempt: None,
                            max_attempts: None,
                            delay_ms: None,
                            reason: Some(reason.into()),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    fn model_event(&mut self, row: &Value, events: &mut Vec<Event>) {
        if let Some(model) = row["model"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != self.model)
        {
            self.model = model.into();
            events.push(Event::Model {
                model: model.into(),
            });
        }
    }
    fn output(&mut self, text: &str, events: &mut Vec<Event>) {
        if text.is_empty() || text == self.text {
            return;
        }
        if let Some(delta) = text.strip_prefix(&self.text) {
            events.push(Event::TextDelta { text: delta.into() });
        } else {
            let previous = std::mem::take(&mut self.text);
            events.push(Event::Message {
                text: previous.clone(),
            });
            self.messages.push(previous);
            events.push(Event::TextDelta { text: text.into() });
        }
        self.text = text.into();
    }
    fn reasoning(&mut self, text: Option<&str>, events: &mut Vec<Event>) {
        if let Some(text) = text.filter(|s| !s.is_empty() && *s != self.reasoning) {
            events.push(Event::ReasoningDelta {
                text: text.strip_prefix(&self.reasoning).unwrap_or(text).into(),
            });
            self.reasoning = text.into();
        }
    }
    pub fn snapshot(&mut self, id: &str, v: &Value) -> Vec<Event> {
        let mut events = vec![];
        self.trace(id, &v["trace"], &mut events);
        self.reasoning(v["reasoning"].as_str(), &mut events);
        if v["preview"] != true
            && !matches!(v["status"].as_str(), Some("failed" | "canceled"))
            && v["error"].as_str().is_none_or(str::is_empty)
            && let Some(text) = v["text"].as_str()
        {
            self.output(text, &mut events);
        }
        events
    }
    pub(super) fn outcome(v: &Value) -> Option<Outcome> {
        match v["status"].as_str()? {
            "done" => Some(Outcome::Completed),
            "failed" => Some(Outcome::Failed),
            "canceled" => Some(Outcome::Aborted),
            _ => None,
        }
    }
    pub fn finish(
        mut self,
        id: &str,
        v: &Value,
        outcome: Outcome,
    ) -> (Vec<Event>, String, Option<crate::TurnFailure>) {
        let mut events = vec![];
        let r = &v["result"];
        self.model_event(v, &mut events);
        self.trace(id, &r["trace"], &mut events);
        self.reasoning(r["reasoning"].as_str(), &mut events);
        if outcome == Outcome::Completed
            && let Some(value) = r.pointer("/final/output").filter(|v| !v.is_null())
        {
            if let Some(text) = value.as_str() {
                self.output(text, &mut events);
            } else {
                self.output(&value.to_string(), &mut events);
            }
        }
        if !self.text.is_empty() {
            self.messages.push(self.text);
        }
        let failure = (outcome == Outcome::Failed).then(|| {
            let reason = v["error"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| v.pointer("/result/error").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .unwrap_or("Morph task failed");
            crate::TurnFailure::new(crate::failure::prose(reason), reason)
        });
        if let Some(failure) = failure.clone() {
            events.push(Event::TurnFailure { failure });
        }
        events.push(Event::TurnEnded { outcome });
        (events, self.messages.join("\n"), failure)
    }
}
pub(super) fn context(v: &Value) -> Vec<Event> {
    let c = &v["context"];
    if c["available"] != true {
        return vec![];
    }
    vec![
        Event::ContextUsage {
            usage: crate::ContextUsage {
                tokens: c["used_input_tokens"].as_u64(),
                context_window: c["context_window_tokens"].as_u64(),
                percent: c["usage_ratio"].as_f64().map(|v| v * 100.0),
            },
        },
        Event::Usage {
            usage: json!({"context":{"tokens":c["used_input_tokens"],"contextWindow":c["context_window_tokens"],"percent":c["usage_ratio"].as_f64().map(|n|n*100.0)}}),
        },
    ]
}
