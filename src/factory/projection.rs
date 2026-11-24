use crate::{
    Event, FailureClass, Outcome, TokenReading, TokenTotals, ToolResult, ToolStatus, TurnFailure,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct Projection {
    inputs: HashMap<String, String>,
    tools: HashSet<String>,
    results: HashMap<String, ToolResult>,
    error: Option<String>,
    tokens: TokenTotals,
    children: Option<TokenTotals>,
}

impl Projection {
    pub(crate) fn sent(&mut self, rpc: u64, input: &Value, starts: bool) {
        if starts {
            self.inputs.clear();
            self.tools.clear();
            self.results.clear();
            self.error = None;
        }
        if let Some(id) = input["messageId"].as_str() {
            self.inputs.insert(rpc.to_string(), id.into());
        }
    }
    pub(crate) fn refused(&mut self, rpc: &str) {
        self.inputs.remove(rpc);
    }
    pub(super) fn tool(&mut self, call: &Value, echo: bool, events: &mut Vec<Event>) {
        let Some(id) = call["id"].as_str() else {
            return;
        };
        let first = self.tools.insert(id.into());
        if first {
            let name = call["name"].as_str().unwrap_or("unknown");
            events.push(Event::ToolStarted {
                id: id.into(),
                name: name.into(),
            });
        }
        if (first || !echo)
            && let Some(input) = call.get("input")
        {
            events.push(Event::ToolInput {
                id: id.into(),
                input: input.clone(),
            });
        }
    }
    pub(super) fn result(
        &mut self,
        id: &Value,
        content: &Value,
        error: &Value,
        events: &mut Vec<Event>,
    ) {
        let Some(id) = id.as_str() else { return };
        let status = error.as_bool().map(|failed| {
            if failed {
                ToolStatus::Failed
            } else {
                ToolStatus::Succeeded
            }
        });
        let mut result = ToolResult {
            status,
            content: crate::tool_activity::parts(content),
            ..Default::default()
        };
        let first = !self.results.contains_key(id);
        if let Some(previous) = self.results.get(id) {
            result.status = result.status.or_else(|| previous.status.clone());
            result.content = result.content.or_else(|| previous.content.clone());
            if &result == previous {
                return;
            }
        }
        self.results.insert(id.into(), result.clone());
        events.push(Event::ToolResult {
            id: id.into(),
            result,
        });
        if first {
            events.push(Event::ToolEnded { id: id.into() });
        }
    }
    pub(crate) fn notice(&mut self, packet: &Value, active: Option<&str>) -> Vec<Event> {
        if packet["type"] != "notification" || packet["method"] != "droid.session_notification" {
            return vec![];
        }
        let n = &packet["params"]["notification"];
        if n["type"] == "settings_updated" {
            return super::settings(&n["settings"]);
        }
        if n["type"] == "session_compacted" {
            return vec![Event::Compaction {
                state: "completed".into(),
            }];
        }
        let Some(active) = active else { return vec![] };
        let mut events = Vec::new();
        match n["type"].as_str() {
            Some("assistant_text_delta" | "thinking_text_delta") => {
                if let Some(text) = n["textDelta"].as_str().filter(|s| !s.is_empty()) {
                    events.push(if n["type"] == "thinking_text_delta" {
                        Event::ReasoningDelta { text: text.into() }
                    } else {
                        Event::TextDelta { text: text.into() }
                    });
                }
            }
            Some("tool_call") => self.tool(&n["toolUse"], false, &mut events),
            Some("tool_result") => {
                self.result(&n["toolUseId"], &n["content"], &n["isError"], &mut events)
            }
            Some("create_message") => {
                let msg = &n["message"];
                let blocks = msg["content"].as_array().into_iter().flatten();
                if msg["role"] == "assistant" {
                    for block in blocks {
                        match block["type"].as_str() {
                            Some("text") => {
                                if let Some(text) = block["text"].as_str().filter(|s| !s.is_empty())
                                {
                                    events.push(Event::Message { text: text.into() });
                                }
                            }
                            Some("tool_use") => self.tool(block, true, &mut events),
                            _ => {}
                        }
                    }
                } else if msg["role"] == "user" {
                    if let Some(rpc) = n["requestId"].as_str()
                        && self.inputs.get(rpc).is_some_and(|id| msg["id"] == *id)
                    {
                        self.inputs.remove(rpc);
                        if msg["id"] == active {
                            events.push(Event::TurnStarted);
                        }
                        let text = blocks
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect::<String>();
                        events.push(Event::UserMessage { text });
                    } else {
                        for b in blocks.filter(|b| b["type"] == "tool_result") {
                            self.result(
                                &b["tool_use_id"],
                                &b["content"],
                                &b["is_error"],
                                &mut events,
                            );
                        }
                    }
                }
            }
            Some("queued_messages_discarded" | "queued_message_handed_back") => {
                let ids = n["drainedRequestIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(
                        n["messages"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|entry| &entry["requestId"]),
                    )
                    .chain(n.get("requestId"))
                    .filter_map(Value::as_str);
                for id in ids {
                    if let Some(input_id) = self.inputs.remove(id) {
                        events.push(Event::InputDropped { input_id });
                    }
                }
            }
            Some("error") => self.error = n["message"].as_str().map(str::to_owned),
            Some("llm_retry") => events.push(Event::Retry {
                attempt: n["attempt"].as_u64(),
                max_attempts: None,
                delay_ms: None,
                reason: Some(n["reason"].as_str().unwrap_or("model request retry").into()),
            }),
            Some("agent_turn_completed") if n["turnId"] == active => {
                if let Some(t) = crate::wire::usage::factory_tokens(&n["tokenUsage"]) {
                    self.tokens = self.tokens.plus(t);
                    if let Some(c) = crate::wire::usage::factory_tokens(&n["childTokenUsage"])
                        .filter(|c| c.input > 0 || c.output > 0)
                    {
                        self.children = Some(self.children.unwrap_or_default().plus(c));
                    }
                    events.push(Event::TokenUsage {
                        reading: TokenReading {
                            tokens: Some(self.tokens),
                            total: self.children.map(|c| self.tokens.plus(c)),
                            ..Default::default()
                        },
                    });
                }
                let reason = n["reason"].as_str().unwrap_or("unknown");
                terminal(reason, self.error.take(), &mut events);
                self.inputs.clear();
                self.results.clear();
                self.tools.clear();
            }
            _ => {}
        }
        events
    }
}

pub(super) fn terminal(reason: &str, detail: Option<String>, events: &mut Vec<Event>) {
    let outcome = match reason {
        "completed" | "spec_handoff" => Outcome::Completed,
        "cancelled" => Outcome::Aborted,
        reason => {
            let class = match reason {
                "model_usage_exhausted" => FailureClass::Quota,
                "model_rate_limited" => FailureClass::RateLimited,
                "model_authentication_failed" => FailureClass::Auth,
                "model_request_rejected" | "prompt_rejected" => FailureClass::InvalidRequest,
                "model_provider_unreachable" | "model_provider_unavailable" => {
                    FailureClass::Provider
                }
                "structured_output_missing"
                | "structured_output_invalid"
                | "structured_output_schema_invalid" => FailureClass::OutputSchema,
                "process_exit" => FailureClass::RuntimeExited,
                _ => FailureClass::Unknown,
            };
            events.push(Event::TurnFailure {
                failure: TurnFailure::new(class, detail.unwrap_or_else(|| reason.into())),
            });
            Outcome::Failed
        }
    };
    events.push(Event::TurnEnded { outcome });
}
