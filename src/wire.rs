use crate::{Event, Outcome, Protocol, RecordKind};
pub(crate) mod acp_prompt;
pub(crate) mod argument_stream;
pub(crate) mod claude_input;
pub(crate) mod compaction;
pub(crate) mod cursor_children;
pub(crate) mod failure;
pub(crate) mod opencode;
pub(crate) mod qwen_content;
pub(crate) mod tasks;
mod tool_activity;
#[cfg(test)]
mod tool_activity_tests;
pub(crate) mod usage;
use serde_json::{Value, json};

pub(crate) fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}
fn string(v: &Value, p: &str) -> Option<String> {
    v.pointer(p)?.as_str().map(str::to_owned)
}
fn text(events: &mut Vec<Event>, v: Option<&str>) {
    if let Some(v) = v {
        events.push(Event::TextDelta { text: v.into() })
    }
}
fn reasoning(events: &mut Vec<Event>, v: Option<&str>) {
    if let Some(v) = v {
        events.push(Event::ReasoningDelta { text: v.into() })
    }
}
fn done(events: &mut Vec<Event>, outcome: Outcome) {
    events.push(Event::TurnEnded { outcome })
}

pub(crate) fn response_id(p: Protocol, v: &Value) -> Option<Value> {
    match p {
        Protocol::Codex | Protocol::Acp
            if v.get("result").is_some() || v.get("error").is_some() =>
        {
            v.get("id").cloned()
        }
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen
            if s(v, "type") == "control_response" =>
        {
            v.pointer("/response/request_id").cloned()
        }
        Protocol::Pi | Protocol::Factory if s(v, "type") == "response" => v.get("id").cloned(),
        _ => None,
    }
}
pub(crate) fn result(p: Protocol, v: &Value) -> crate::Result<Value> {
    let error = match p {
        Protocol::Codex | Protocol::Acp | Protocol::Factory => v.get("error"),
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen
            if v.pointer("/response/subtype").and_then(Value::as_str) == Some("error") =>
        {
            Some(&v["response"])
        }
        Protocol::Pi if v["success"] == false => Some(v),
        _ => None,
    };
    if let Some(e) = error {
        return Err(crate::Error::Refused(e.to_string()));
    }
    Ok(match p {
        Protocol::Codex | Protocol::Acp | Protocol::Factory => v["result"].clone(),
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen => v["response"]["response"].clone(),
        Protocol::Pi => v["data"].clone(),
        Protocol::Cybion | Protocol::Morph | Protocol::KimiKap | Protocol::Dsh => {
            return Err(crate::Error::Protocol("expected a stdio response".into()));
        }
    })
}
pub(crate) fn request(p: Protocol, id: u64, method: &str, params: Value) -> Value {
    match p {
        Protocol::Cybion | Protocol::Morph | Protocol::KimiKap | Protocol::Dsh => {
            unreachable!("HTTP runtimes own a separate transport")
        }
        Protocol::Factory => crate::factory::request(&id.to_string(), method, params),
        Protocol::Codex | Protocol::Acp => {
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        }
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen => {
            let mut body = params;
            body["subtype"] = method.into();
            json!({"type":"control_request","request_id":id.to_string(),"request":body})
        }
        Protocol::Pi => {
            let mut body = params;
            body["id"] = id.to_string().into();
            body["type"] = method.into();
            body
        }
    }
}
pub(crate) fn host_request(p: Protocol, v: &Value) -> Option<(Value, String)> {
    match p {
        Protocol::Factory if v["type"] == "request" => {
            Some((v.get("id")?.clone(), v["method"].as_str()?.into()))
        }
        Protocol::Codex | Protocol::Acp if v.get("method").is_some() => {
            Some((v.get("id")?.clone(), s(v, "method").into()))
        }
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen
            if s(v, "type") == "control_request" =>
        {
            Some((
                v.get("request_id")?.clone(),
                s(&v["request"], "subtype").into(),
            ))
        }
        Protocol::Pi
            if v["type"] == "extension_ui_request"
                && matches!(s(v, "method"), "select" | "confirm" | "input" | "editor") =>
        {
            Some((v.get("id")?.clone(), s(v, "method").into()))
        }
        _ => None,
    }
}
pub(crate) fn answer(p: Protocol, id: &Value, method: &str, answer: Option<Value>) -> Value {
    match p {
        Protocol::Factory => crate::factory::answer(id, method, answer),
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen => {
            let value = answer.or_else(|| match method {
                "can_use_tool" => Some(json!({"behavior":"deny","message":"Denied by host"})),
                "elicitation" => Some(json!({"action":"decline"})),
                _ => None,
            });
            match value {
                Some(result) => {
                    json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":result}})
                }
                None => {
                    json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"Unsupported host request"}})
                }
            }
        }
        Protocol::Pi => {
            let mut value = answer
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({"cancelled":true}));
            value["type"] = "extension_ui_response".into();
            value["id"] = id.clone();
            value
        }
        _ => {
            let value = answer.or_else(|| {
                Some(match method {
                    "session/request_permission" | "cursor/ask_question" | "cursor/create_plan" => {
                        json!({"outcome":{"outcome":"cancelled"}})
                    }
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        json!({"decision":"decline"})
                    }
                    "execCommandApproval" | "applyPatchApproval" => json!({"decision":"denied"}),
                    "item/permissions/requestApproval" => json!({"permissions":{}}),
                    "item/tool/requestUserInput" => json!({"answers":{}}),
                    "mcpServer/elicitation/request" | "elicitation/create" => json!({"action":"decline"}),
                    "item/tool/call" if p == Protocol::Codex => json!({
                        "success":false,
                        "contentItems":[{"type":"inputText","text":"Host did not execute this tool"}]
                    }),
                    _ => return None,
                })
            });
            match value {
                Some(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                None => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Host does not implement this method"}})
                }
            }
        }
    }
}
pub(crate) fn classify(p: Protocol, v: &Value) -> RecordKind {
    if response_id(p, v).is_some() {
        RecordKind::Response
    } else if host_request(p, v).is_some() {
        RecordKind::Request
    } else {
        RecordKind::Frame
    }
}

fn settings(model: Option<&str>, effort: Option<&str>, tier: Option<&Value>) -> Vec<Event> {
    let mut events = vec![];
    if let Some(model) = model.filter(|s| !s.is_empty()) {
        events.push(Event::Model {
            model: model.into(),
        });
    }
    if let Some(effort) = effort.filter(|s| !s.is_empty()) {
        events.push(Event::Effort {
            effort: effort.into(),
        });
    }
    if let Some(tier) = tier
        .and_then(|v| {
            if v.is_null() {
                Some("default")
            } else {
                v.as_str()
            }
        })
        .filter(|s| !s.is_empty())
    {
        events.push(Event::ServiceTier { tier: tier.into() });
    }
    events
}
fn acp_settings(body: &Value, cursor: bool) -> Vec<Event> {
    let model = crate::options::acp_model_option(body)
        .and_then(|v| v["currentValue"].as_str())
        .or_else(|| {
            body.pointer("/models/currentModelId")
                .and_then(Value::as_str)
        });
    let effort =
        crate::options::acp_effort_option(body, cursor).and_then(|v| v["currentValue"].as_str());
    settings(model, effort, None)
}
pub(crate) fn reply_settings(p: Protocol, method: &str, body: &Value, cursor: bool) -> Vec<Event> {
    match (p, method) {
        (Protocol::Codex, "thread/start" | "thread/resume" | "thread/fork") => settings(
            body["model"].as_str(),
            body["reasoningEffort"].as_str(),
            body.get("serviceTier"),
        ),
        (
            Protocol::Acp,
            "session/new" | "session/load" | "session/fork" | "session/set_config_option",
        ) => acp_settings(body, cursor),
        (Protocol::Factory, "droid.initialize_session" | "droid.load_session") => {
            crate::factory::settings(&body["settings"])
        }
        (Protocol::Pi, "get_state") => {
            let m = &body["model"];
            let model = m["id"].as_str().map(|id| {
                m["provider"]
                    .as_str()
                    .map_or_else(|| id.to_owned(), |p| format!("{p}/{id}"))
            });
            settings(model.as_deref(), body["thinkingLevel"].as_str(), None)
        }
        (Protocol::Qwen, "initialize") => {
            let status = &body["effort_status"];
            let effort =
                if status["applied"] == true && status.get("override") == Some(&Value::Null) {
                    status["effort"].as_str()
                } else {
                    None
                };
            settings(None, effort, None)
        }
        _ => vec![],
    }
}
pub(crate) fn project(p: Protocol, v: &Value, cursor: bool) -> Vec<Event> {
    let mut events = Vec::new();
    match p {
        Protocol::Factory => {} // Stateful notifications are projected by the stdio actor.
        Protocol::Cybion | Protocol::Morph | Protocol::KimiKap | Protocol::Dsh => {
            unreachable!("HTTP runtimes project their own frames")
        }
        Protocol::Codex => {
            let args = &v["params"];
            let item = &args["item"];
            let item_frame = matches!(s(v, "method"), "item/started" | "item/completed");
            let collaboration = matches!(s(item, "type"), "collabAgentToolCall" | "collabToolCall")
                || (s(item, "type") == "subAgentActivity" && s(item, "kind") == "started");
            if item_frame
                && collaboration
                && let Some(parent) = item["senderThreadId"]
                    .as_str()
                    .or_else(|| args["threadId"].as_str())
            {
                for child in item["receiverThreadIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(item.get("agentThreadId"))
                    .filter_map(Value::as_str)
                    .filter(|id| !id.is_empty() && *id != parent)
                {
                    events.push(Event::SessionLinked {
                        parent: parent.into(),
                        child: child.into(),
                    });
                }
            }
            match s(v, "method") {
                "mcpServer/startupStatus/updated" => events.extend(Event::codex_mcp_startup(args)),
                method @ ("thread/goal/updated" | "thread/goal/cleared") => {
                    if let Some(thread) = args["threadId"].as_str()
                        && let Ok(goal) = crate::goal::observation(args, method, Some(thread))
                    {
                        events.push(Event::GoalChanged { goal });
                    }
                }
                "thread/settings/updated" => {
                    let current = &args["threadSettings"];
                    events.extend(settings(
                        current["model"].as_str(),
                        current["effort"].as_str(),
                        current.get("serviceTier"),
                    ));
                }
                "turn/started" => events.push(Event::TurnStarted),
                "turn/completed" => {
                    let outcome = match s(&args["turn"], "status") {
                        "completed" => Outcome::Completed,
                        "interrupted" => Outcome::Aborted,
                        _ => {
                            events.push(Event::TurnFailure {
                                failure: failure::codex(&args["turn"]["error"]),
                            });
                            Outcome::Failed
                        }
                    };
                    done(&mut events, outcome);
                }
                "item/agentMessage/delta" => text(&mut events, args["delta"].as_str()),
                "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                    reasoning(&mut events, args["delta"].as_str())
                }
                "item/started" | "item/completed"
                    if matches!(
                        s(item, "type"),
                        "commandExecution"
                            | "fileChange"
                            | "mcpToolCall"
                            | "dynamicToolCall"
                            | "collabAgentToolCall"
                            | "collabToolCall"
                            | "sleep"
                            | "webSearch"
                    ) =>
                {
                    let id = s(item, "id").to_owned();
                    events.push(if s(v, "method") == "item/started" {
                        Event::ToolStarted {
                            id,
                            name: item["tool"].as_str().unwrap_or(s(item, "type")).into(),
                        }
                    } else {
                        Event::ToolEnded { id }
                    })
                }
                "item/completed" if s(item, "type") == "agentMessage" => {
                    if let Some(content) = item["text"].as_str() {
                        events.push(Event::Message {
                            text: content.into(),
                        })
                    }
                }
                "thread/tokenUsage/updated" => {
                    let u = &args["tokenUsage"];
                    events.push(Event::Usage { usage: u.clone() });
                    if u["total"].is_object() {
                        let (tokens, window) = if u["last"].is_object() {
                            (
                                u["last"]["totalTokens"].as_u64(),
                                u["modelContextWindow"].as_u64(),
                            )
                        } else {
                            (u["total"]["inputTokens"].as_u64(), None)
                        };
                        events.push(Event::ContextUsage {
                            usage: crate::ContextUsage::counts(tokens, window, true),
                        });
                    }
                }
                _ => {}
            }
        }
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen => {
            let e = &v["event"];
            match s(v, "type") {
                "system" if v["subtype"] == "init" => {
                    events.extend(settings(v["model"].as_str(), None, None))
                }
                "stream_event" => match s(e, "type") {
                    "content_block_delta" if p != Protocol::Qwen => {
                        let d = &e["delta"];
                        text(&mut events, d["text"].as_str());
                        reasoning(&mut events, d["thinking"].as_str())
                    }
                    "content_block_start" if s(&e["content_block"], "type") == "tool_use" => {
                        let b = &e["content_block"];
                        events.push(Event::ToolStarted {
                            id: s(b, "id").into(),
                            name: s(b, "name").into(),
                        })
                    }
                    _ => {}
                },
                "assistant" | "user" => {
                    if v["type"] == "assistant"
                        && (p != Protocol::Claude
                            || v["parent_tool_use_id"]
                                .as_str()
                                .is_some_and(|id| !id.is_empty()))
                    {
                        events.extend(settings(
                            v.pointer("/message/model")
                                .and_then(Value::as_str)
                                .filter(|m| *m != "<synthetic>"),
                            None,
                            None,
                        ));
                    }
                    if let Some(parts) = v.pointer("/message/content").and_then(Value::as_array) {
                        for part in parts {
                            match s(part, "type") {
                                "text" if p != Protocol::Qwen && s(v, "type") == "assistant" => {
                                    events.push(Event::Message {
                                        text: s(part, "text").into(),
                                    })
                                }
                                "tool_result" => events.push(Event::ToolEnded {
                                    id: s(part, "tool_use_id").into(),
                                }),
                                _ => {}
                            }
                        }
                    }
                }
                "result" => {
                    if let Some(usage) = v.get("usage") {
                        events.push(Event::Usage {
                            usage: usage.clone(),
                        })
                    }
                    if p == Protocol::Qoder {
                        let aborted = matches!(
                            v["terminal_reason"].as_str(),
                            Some("aborted_streaming" | "aborted_tools")
                        );
                        if v["is_error"] == true && !aborted {
                            events.push(Event::TurnFailure {
                                failure: crate::qoder::failure(v),
                            });
                        }
                        done(
                            &mut events,
                            if aborted {
                                Outcome::Aborted
                            } else if v["is_error"] == true {
                                Outcome::Failed
                            } else {
                                Outcome::Completed
                            },
                        );
                        return events;
                    }
                    let failed = v["is_error"] == true
                        || (p != Protocol::Qwen && s(v, "subtype") != "success");
                    if p == Protocol::Qwen && failed {
                        events.push(Event::TurnFailure {
                            failure: failure::qwen(v),
                        });
                    }
                    done(
                        &mut events,
                        if failed {
                            Outcome::Failed
                        } else {
                            Outcome::Completed
                        },
                    )
                }
                _ => {}
            }
        }
        Protocol::Acp => {
            let update = &v["params"]["update"];
            match s(update, "sessionUpdate") {
                "config_option_update" => events.extend(acp_settings(update, cursor)),
                "agent_message_chunk" => text(&mut events, update["content"]["text"].as_str()),
                "agent_thought_chunk" => reasoning(&mut events, update["content"]["text"].as_str()),
                "tool_call" => {
                    events.push(Event::ToolStarted {
                        id: s(update, "toolCallId").into(),
                        name: s(update, "title").into(),
                    });
                    if matches!(s(update, "status"), "completed" | "failed" | "cancelled") {
                        events.push(Event::ToolEnded {
                            id: s(update, "toolCallId").into(),
                        });
                    }
                }
                "tool_call_update" => {
                    let id = s(update, "toolCallId").into();
                    events.push(
                        if matches!(s(update, "status"), "completed" | "failed" | "cancelled") {
                            Event::ToolEnded { id }
                        } else {
                            Event::ToolUpdated { id }
                        },
                    )
                }
                "usage_update" => {
                    events.push(Event::Usage {
                        usage: update.clone(),
                    });
                    let (tokens, window) = (update["used"].as_u64(), update["size"].as_u64());
                    if tokens.is_some() || window.is_some() {
                        events.push(Event::ContextUsage {
                            usage: crate::ContextUsage::counts(tokens, window, true),
                        });
                    }
                }
                _ => {}
            }
        }
        Protocol::Pi => match s(v, "type") {
            "agent_start" => events.push(Event::TurnStarted),
            "agent_end" => {
                let last = v["messages"]
                    .as_array()
                    .and_then(|m| m.iter().rev().find(|v| s(v, "role") == "assistant"));
                if let Some(msg) = last.filter(|m| m["stopReason"] == "error") {
                    events.push(Event::TurnFailure {
                        failure: failure::pi(msg),
                    });
                }
                done(
                    &mut events,
                    match last.map(|v| s(v, "stopReason")) {
                        Some("aborted") => Outcome::Aborted,
                        Some("error") => Outcome::Failed,
                        _ => Outcome::Completed,
                    },
                )
            }
            "message_update" => {
                let e = &v["assistantMessageEvent"];
                match s(e, "type") {
                    "text_delta" => text(&mut events, e["delta"].as_str()),
                    "thinking_delta" => reasoning(&mut events, e["delta"].as_str()),
                    _ => {}
                }
            }
            "message_end" => {
                if let Some(usage) = v.pointer("/message/usage") {
                    events.push(Event::Usage {
                        usage: usage.clone(),
                    })
                }
            }
            "tool_execution_start" => events.push(Event::ToolStarted {
                id: s(v, "toolCallId").into(),
                name: s(v, "toolName").into(),
            }),
            "tool_execution_end" => events.push(Event::ToolEnded {
                id: s(v, "toolCallId").into(),
            }),
            _ => {}
        },
    }
    events.extend(tasks::reported(p, v));
    tool_activity::project(p, v, &mut events);
    events
}
pub(crate) fn session_id(p: Protocol, v: &Value) -> Option<String> {
    match p {
        Protocol::Codex if v["method"] == "thread/started" => string(v, "/params/thread/id")
            .filter(|id| !id.is_empty())
            .or_else(|| string(v, "/params/threadId")),
        Protocol::Codex => string(v, "/params/threadId").or_else(|| string(v, "/params/thread/id")),
        Protocol::Acp | Protocol::Factory => string(v, "/params/sessionId"),
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen => string(v, "/session_id"),
        Protocol::Pi | Protocol::Cybion | Protocol::Morph | Protocol::KimiKap | Protocol::Dsh => {
            None
        }
    }
}

#[cfg(test)]
mod host_refusals {
    use super::*;
    #[test]
    fn recognized_host_requests_receive_their_native_negative_reply() {
        let id = json!(9);
        let tool = answer(Protocol::Codex, &id, "item/tool/call", None);
        assert_eq!(tool["result"]["success"], false);
        assert_eq!(tool["result"]["contentItems"][0]["type"], "inputText");
        let form = answer(Protocol::Acp, &id, "elicitation/create", None);
        assert_eq!(form["result"], json!({"action":"decline"}));
        assert_eq!(
            answer(Protocol::Acp, &id, "item/tool/call", None)["error"]["code"],
            -32601
        );
        assert_eq!(
            answer(Protocol::Factory, &id, "droid.request_permission", None)["error"]["code"],
            -32602
        );
    }
}
