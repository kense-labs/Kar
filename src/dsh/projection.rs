use crate::{
    ContextUsage, Event, FailureClass, Outcome, TaskChange, TaskDetails, TaskStatus, TaskType,
    TokenReading, TokenTotals, ToolResult, ToolStatus, TurnFailure,
};
use serde_json::Value;

fn string(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}
pub(super) fn text(v: &Value, kind: &str) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == kind)
        .filter_map(|b| b["text"].as_str().or_else(|| b["thinking"].as_str()))
        .collect()
}
#[derive(Default)]
pub(super) struct Projection {
    pub cursor: Option<i64>,
    pub active: Option<u64>,
    window: Option<u64>,
    model: Option<String>,
    effort: Option<String>,
    spent: TokenTotals,
    // Live attempt IDs restart at one when the vendor process restarts. Committed
    // settlements delimit retries inside the durable (turn, step) pair.
    last_settlement: Option<(u64, u64, u64)>,
    live_key: Option<(String, String)>,
}
impl Projection {
    pub fn message_key(&self, data: &Value) -> Option<String> {
        let turn = data["turn"].as_u64()?;
        let step = data["step"].as_u64()?;
        let previous = self
            .last_settlement
            .filter(|(t, s, _)| *t == turn && *s == step)
            .map_or(0, |(_, _, seq)| seq + 1);
        Some(format!("dsh/{turn}/{step}/{previous}"))
    }

    pub fn observe_settlement(&mut self, e: &Value) {
        if matches!(
            e["type"].as_str(),
            Some("assistant/message" | "assistant/attempt")
        ) {
            let d = &e["data"];
            self.last_settlement = d["turn"]
                .as_u64()
                .zip(d["step"].as_u64())
                .zip(e["seq"].as_u64())
                .map(|((turn, step), seq)| (turn, step, seq));
        }
    }

    pub fn start_attempt(&mut self, f: &Value) {
        self.live_key = string(&f["attemptId"]).zip(self.message_key(f));
    }

    pub fn stream_key(&self, f: &Value) -> Option<String> {
        self.live_key
            .as_ref()
            .filter(|(id, _)| f["attemptId"] == *id)
            .map(|(_, key)| key.clone())
    }

    pub fn chunk(&mut self, f: &Value) -> Vec<Event> {
        if f["type"] == "start" {
            self.start_attempt(f);
        }
        if f["type"] != "chunk" {
            return vec![];
        }
        let c = &f["chunk"];
        match c["type"].as_str() {
            Some("text-delta") => string(&c["text"]).map(|text| Event::TextDelta { text }),
            Some("reasoning-delta") => {
                string(&c["text"]).map(|text| Event::ReasoningDelta { text })
            }
            Some("tool-call-delta") => {
                c["id"]
                    .as_str()
                    .zip(string(&c["argumentsDelta"]))
                    .map(|(id, delta)| Event::ToolInputDelta {
                        id: id.into(),
                        delta,
                    })
            }
            _ => None,
        }
        .into_iter()
        .collect()
    }
    fn usage(&mut self, v: &Value, committed: bool, out: &mut Vec<Event>) {
        let (Some(i), Some(o)) = (v["inputTokens"].as_i64(), v["outputTokens"].as_i64()) else {
            return;
        };
        let read = v["cacheReadTokens"].as_i64();
        let write = v["cacheWriteTokens"].as_i64();
        self.spent = self.spent.plus(TokenTotals {
            input: i
                .saturating_add(read.unwrap_or(0))
                .saturating_add(write.unwrap_or(0)),
            output: o,
            cache_read: read,
            cache_write: write,
            reasoning: v["reasoningTokens"].as_i64(),
        });
        out.push(Event::TokenUsage {
            reading: TokenReading {
                tokens: Some(self.spent),
                ..Default::default()
            },
        });
        if committed && let Some(n) = v["totalTokens"].as_u64() {
            out.push(Event::ContextUsage {
                usage: ContextUsage::counts(Some(n), self.window, false),
            });
        }
    }
    fn selection(
        &mut self,
        provider: &Value,
        model: &Value,
        effort: &Value,
        initial: bool,
        events: &mut Vec<Event>,
    ) {
        if (!initial || self.model.is_none())
            && let Some(provider) = provider.as_str().filter(|s| !s.is_empty())
            && let Some(model) = model.as_str().filter(|s| !s.is_empty())
        {
            let model = format!("{provider}/{model}");
            if self.model.as_ref() != Some(&model) {
                self.model = Some(model.clone());
                events.push(Event::Model { model });
            }
        }
        if (!initial || self.effort.is_none())
            && let Some(effort) = effort.as_str().filter(|s| !s.is_empty())
            && self.effort.as_deref() != Some(effort)
        {
            self.effort = Some(effort.into());
            events.push(Event::Effort {
                effort: effort.into(),
            });
        }
    }

    pub fn event(&mut self, session: &str, e: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        self.observe_settlement(e);
        let d = &e["data"];
        match e["type"].as_str() {
            Some("user/message") if d["source"]["kind"] == "user" => out.push(Event::UserMessage {
                text: text(&d["content"], "text"),
            }),
            Some("turn/start") => {
                self.active = d["turn"].as_u64();
                out.push(Event::TurnStarted);
            }
            Some("turn/end") => {
                self.active = None;
                let reason = &d["reason"];
                let outcome = match reason["kind"].as_str() {
                    Some("completed") => Outcome::Completed,
                    Some("aborted") => Outcome::Aborted,
                    _ => {
                        out.push(Event::TurnFailure {
                            failure: failure(reason),
                        });
                        Outcome::Failed
                    }
                };
                out.push(Event::TurnEnded { outcome });
                if let Some(seq) = e["seq"].as_u64() {
                    out.push(Event::ForkAnchor {
                        anchor: seq.to_string(),
                    });
                }
            }
            Some("assistant/message") => {
                let msg = text(&d["message"]["content"], "text");
                if !msg.is_empty() {
                    out.push(Event::Message { text: msg });
                }
                self.usage(&d["usage"], true, &mut out);
            }
            Some("assistant/attempt") => {
                if let Some(v) = d["stream"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .rev()
                    .map(|e| &e["chunk"])
                    .find(|c| c["type"] == "usage")
                {
                    self.usage(&v["usage"], false, &mut out);
                }
            }
            Some("request/context") => self.window = d["contextWindow"].as_u64(),
            Some("request/header") => {
                let cfg = &d["header"]["config"];
                self.selection(
                    &cfg["provider"],
                    &cfg["model"],
                    &cfg["reasoningEffort"],
                    false,
                    &mut out,
                );
            }
            Some("subagent/descriptor") => {
                self.selection(
                    &d["agentProvider"],
                    &d["agentModel"],
                    &d["agentReasoningEffort"],
                    true,
                    &mut out,
                );
            }
            Some("tool/call") => {
                if let (Some(id), Some(name)) = (string(&d["callId"]), string(&d["name"])) {
                    out.push(Event::ToolStarted {
                        id: id.clone(),
                        name,
                    });
                    if let Some(args) = d["arguments"].as_str() {
                        out.push(Event::ToolInput {
                            id,
                            input: serde_json::from_str(args).unwrap_or_else(|_| args.into()),
                        });
                    }
                }
            }
            Some("tool/result") => {
                let m = &d["message"];
                if let Some(id) = string(&m["toolCallId"]) {
                    let status = m["isError"].as_bool().map(|e| {
                        if e {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Succeeded
                        }
                    });
                    let result = ToolResult {
                        status,
                        content: crate::tool_activity::parts(&m["content"]),
                        ..Default::default()
                    };
                    out.push(Event::ToolResult {
                        id: id.clone(),
                        result,
                    });
                    out.push(Event::ToolEnded { id });
                }
            }
            Some("compaction/start") => out.push(Event::Compaction {
                state: "started".into(),
            }),
            Some("compaction/end") => out.push(Event::Compaction {
                state: if d["error"].is_string() {
                    "failed"
                } else {
                    "completed"
                }
                .into(),
            }),
            Some("goal/change") => out.extend(super::goals::event(d)),
            Some("job/registered") if d["owner"] == session => {
                if let Some(id) = string(&d["jobId"]) {
                    out.push(Event::Task {
                        id,
                        change: TaskChange::Started {
                            task_type: if matches!(d["kind"].as_str(), Some("bash" | "pwsh")) {
                                TaskType::Shell
                            } else {
                                TaskType::Other
                            },
                            details: TaskDetails {
                                description: string(&d["label"]),
                                native_type: string(&d["kind"]),
                                tool_call_id: string(&d["toolCall"]["callId"]),
                                ..Default::default()
                            },
                        },
                    });
                }
            }
            Some("job/settled") if d["owner"] == session => {
                let status = match d["status"].as_str() {
                    Some("completed") => Some(TaskStatus::Completed),
                    Some("killed") => Some(TaskStatus::Stopped),
                    Some("failed") => Some(TaskStatus::Failed),
                    _ => None,
                };
                if let (Some(id), Some(status)) = (string(&d["jobId"]), status) {
                    out.push(Event::Task {
                        id,
                        change: TaskChange::Ended {
                            status,
                            summary: string(&d["detail"]),
                            output: None,
                            output_file: None,
                        },
                    });
                }
            }
            Some("subagent/catalog") => {
                if let Some(child) = string(&d["childId"]).filter(|c| c != session) {
                    out.push(Event::SessionLinked {
                        parent: session.into(),
                        child,
                    });
                }
            }
            _ => {}
        }
        out
    }
}
fn failure(reason: &Value) -> TurnFailure {
    let e = &reason["error"];
    let status = e["status"].as_u64().and_then(|n| u16::try_from(n).ok());
    let class = match (reason["kind"].as_str(), e["code"].as_str()) {
        (Some("interrupted"), _) => FailureClass::RuntimeExited,
        (_, Some("AUTH" | "INVALID_CREDENTIAL")) => FailureClass::Auth,
        (_, Some("ACCOUNT_QUOTA" | "QUOTA")) => FailureClass::Quota,
        (_, Some("RATE_LIMIT")) => FailureClass::RateLimited,
        (_, Some("CONTEXT_WINDOW_EXCEEDED")) => FailureClass::InputTooLarge,
        (_, Some("UNKNOWN_MODEL" | "NO_ADAPTER")) => FailureClass::ModelUnavailable,
        (_, Some("INVALID_REQUEST" | "UNSUPPORTED_REASONING_EFFORT")) => {
            FailureClass::InvalidRequest
        }
        (_, Some("SERVER")) => FailureClass::Provider,
        _ => crate::wire::failure::from_status(status),
    };
    let mut f = TurnFailure::new(
        class,
        e["message"]
            .as_str()
            .or(reason["kind"].as_str())
            .unwrap_or("DSH turn failed"),
    );
    f.status = status;
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn child_descriptor_and_headers_track_each_sessions_confirmed_selection() {
        let descriptor = json!({"type":"subagent/descriptor","data":{
            "agentProvider":"vendor","agentModel":"child","agentReasoningEffort":"high","provider":"spawn"}});
        let header = |config| json!({"type":"request/header","data":{"header":{"config":config}}});
        let mut child = Projection::default();
        let mut sibling = Projection::default();
        let initial = vec![
            Event::Model {
                model: "vendor/child".into(),
            },
            Event::Effort {
                effort: "high".into(),
            },
        ];
        assert_eq!(child.event("child", &descriptor), initial);
        assert!(
            child
                .event(
                    "child",
                    &header(json!({"provider":"vendor","model":"child","reasoningEffort":"high"}))
                )
                .is_empty()
        );
        assert_eq!(
            child.event("child", &header(json!({"reasoningEffort":"low"}))),
            [Event::Effort {
                effort: "low".into()
            }]
        );
        assert_eq!(sibling.event("sibling", &descriptor), initial);
        assert_eq!(
            child.event(
                "child",
                &header(json!({"provider":"vendor","model":"later","reasoningEffort":"low"}))
            ),
            [Event::Model {
                model: "vendor/later".into()
            }]
        );
        assert!(child.event("child", &descriptor).is_empty());
        assert!(Projection::default().event("anonymous",&json!({"type":"subagent/descriptor","data":{"agentModel":"orphan","provider":"spawn"}})).is_empty());
    }
}
