use crate::{
    ChangeKind, Event, FileChange, Protocol, ToolResult, ToolStatus,
    tool_activity::{mcp_raw_parts, parts},
};
use serde_json::Value;

fn input(events: &mut Vec<Event>, id: &str, value: Option<&Value>) {
    if let Some(value) = value {
        events.push(Event::ToolInput {
            id: id.into(),
            input: value.clone(),
        });
    }
}
fn status(native: &Value) -> Option<ToolStatus> {
    match native.as_str()? {
        "completed" => Some(ToolStatus::Succeeded),
        "failed" | "declined" => Some(ToolStatus::Failed),
        "cancelled" | "canceled" | "interrupted" => Some(ToolStatus::Cancelled),
        _ => None,
    }
}
fn boolean_status(failed: Option<bool>) -> Option<ToolStatus> {
    failed.map(|error| {
        if error {
            ToolStatus::Failed
        } else {
            ToolStatus::Succeeded
        }
    })
}
fn confirmed_changes(item: &Value) -> Vec<FileChange> {
    if item["type"] != "fileChange" || item["status"] != "completed" {
        return vec![];
    }
    item["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|change| {
            let path = change["path"].as_str().filter(|s| !s.is_empty())?;
            let kind = match change["kind"]["type"].as_str() {
                Some("add") => ChangeKind::Added,
                Some("update") => ChangeKind::Modified,
                Some("delete") => ChangeKind::Deleted,
                _ => ChangeKind::Unknown,
            };
            let move_to = (kind == ChangeKind::Modified)
                .then(|| {
                    change["kind"]["move_path"]
                        .as_str()
                        .or(change["kind"]["movePath"].as_str())
                        .map(str::to_owned)
                })
                .flatten();
            Some(FileChange {
                path: path.into(),
                kind,
                move_to,
            })
        })
        .collect()
}
fn codex(v: &Value, events: &mut Vec<Event>) {
    let p = &v["params"];
    match v["method"].as_str() {
        Some("turn/diff/updated") => {
            if let (Some(turn_id), Some(diff)) = (p["turnId"].as_str(), p["diff"].as_str()) {
                events.push(Event::TurnDiff {
                    turn_id: turn_id.into(),
                    diff: diff.into(),
                });
            }
        }
        Some("item/commandExecution/outputDelta") => {
            if let (Some(id), Some(delta)) = (p["itemId"].as_str(), p["delta"].as_str()) {
                events.push(Event::ToolProgress {
                    id: id.into(),
                    output: None,
                    delta: Some(delta.into()),
                });
            }
        }
        Some(method @ ("item/started" | "item/completed")) => {
            let item = &p["item"];
            let Some(id) = item["id"].as_str() else {
                return;
            };
            let arg = match item["type"].as_str() {
                Some("commandExecution") => "command",
                Some("fileChange") => "changes",
                Some("mcpToolCall" | "dynamicToolCall") => "arguments",
                Some("sleep") => "durationMs",
                Some("webSearch") => "query",
                Some("collabAgentToolCall" | "collabToolCall") => "prompt",
                _ => return,
            };
            if item["tool"].is_string() || item["server"].is_string() {
                let mcp_app_uri = (item["type"] == "mcpToolCall")
                    .then(|| {
                        item.pointer("/mcpAppUi/resourceUri")
                            .and_then(Value::as_str)
                            .or_else(|| item["mcpAppResourceUri"].as_str())
                            .or_else(|| {
                                item.pointer("/appContext/resourceUri")
                                    .and_then(Value::as_str)
                            })
                    })
                    .flatten()
                    .filter(|s| s.starts_with("ui://") && s.len() <= 4096)
                    .map(str::to_owned);
                events.push(Event::ToolIdentity {
                    mcp_app_uri,
                    id: id.into(),
                    native_type: item["type"].as_str().unwrap().into(),
                    server: item["server"].as_str().map(str::to_owned),
                    tool_name: item["tool"].as_str().map(str::to_owned),
                });
            }
            if arg == "durationMs" && item[arg].is_number() {
                input(
                    events,
                    id,
                    Some(&serde_json::json!({"durationMs":item[arg]})),
                );
            } else {
                input(events, id, item.get(arg));
            }
            if method == "item/completed" {
                let output = match item["type"].as_str() {
                    Some("commandExecution") => &item["aggregatedOutput"],
                    Some("mcpToolCall") if !item["error"].is_null() => &item["error"],
                    Some("mcpToolCall") => {
                        item.pointer("/result/content").unwrap_or(&item["result"])
                    }
                    Some("dynamicToolCall") => &item["contentItems"],
                    Some("webSearch") => &item["results"],
                    _ => &Value::Null,
                };
                events.push(Event::ToolResult {
                    id: id.into(),
                    result: ToolResult {
                        status: status(&item["status"]),
                        exit_code: item["exitCode"].as_i64(),
                        content: parts(output),
                        file_changes: confirmed_changes(item),
                    },
                });
            }
        }
        _ => {}
    }
}
fn acp(v: &Value, events: &mut Vec<Event>) {
    let u = &v["params"]["update"];
    if !matches!(
        u["sessionUpdate"].as_str(),
        Some("tool_call" | "tool_call_update")
    ) {
        return;
    }
    let Some(id) = u["toolCallId"].as_str() else {
        return;
    };
    input(events, id, u.get("rawInput"));
    let raw = &u["rawOutput"];
    if let Some(status) = status(&u["status"]) {
        let content = u["content"]
            .as_array()
            .filter(|list| !list.is_empty())
            .map(|list| {
                list.iter()
                    .flat_map(|block| {
                        let value = if block["type"] == "content" {
                            &block["content"]
                        } else {
                            block
                        };
                        parts(value).unwrap_or_default()
                    })
                    .collect()
            })
            .or_else(|| mcp_raw_parts(raw, Some(&status)))
            .or_else(|| parts(raw));
        events.push(Event::ToolResult {
            id: id.into(),
            result: ToolResult {
                status: Some(status),
                exit_code: raw["exitCode"].as_i64().or(raw["exit_code"].as_i64()),
                content,
                ..ToolResult::default()
            },
        });
    } else if u["sessionUpdate"] == "tool_call_update" && !raw.is_null() {
        let output = raw
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| raw.to_string());
        events.push(Event::ToolProgress {
            id: id.into(),
            output: Some(output),
            delta: None,
        });
    }
}
fn stream_json(v: &Value, events: &mut Vec<Event>) {
    if !matches!(v["type"].as_str(), Some("assistant" | "user")) {
        return;
    }
    for block in v["message"]["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("tool_use") if v["type"] == "assistant" => {
                if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) {
                    events.push(Event::ToolStarted {
                        id: id.into(),
                        name: name.into(),
                    });
                    input(events, id, block.get("input"));
                }
            }
            Some("tool_result") if v["type"] == "user" => {
                if let Some(id) = block["tool_use_id"].as_str() {
                    events.push(Event::ToolResult {
                        id: id.into(),
                        result: ToolResult {
                            status: boolean_status(Some(
                                block["is_error"].as_bool().unwrap_or(false),
                            )),
                            content: parts(&block["content"]),
                            ..ToolResult::default()
                        },
                    });
                }
            }
            _ => {}
        }
    }
}
fn pi(v: &Value, events: &mut Vec<Event>) {
    let Some(id) = v["toolCallId"].as_str() else {
        return;
    };
    match v["type"].as_str() {
        Some("tool_execution_start") => input(events, id, v.get("args")),
        Some("tool_execution_update") => {
            if let Some(list) = v
                .pointer("/partialResult/content")
                .and_then(Value::as_array)
            {
                let chunks: Vec<_> = list
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect();
                if !chunks.is_empty() || list.is_empty() {
                    events.push(Event::ToolProgress {
                        id: id.into(),
                        output: Some(chunks.join("\n")),
                        delta: None,
                    });
                }
            }
        }
        Some("tool_execution_end") => events.push(Event::ToolResult {
            id: id.into(),
            result: ToolResult {
                status: boolean_status(v["isError"].as_bool()),
                content: parts(&v["result"]["content"]),
                ..ToolResult::default()
            },
        }),
        _ => {}
    }
}
pub(super) fn project(protocol: Protocol, native: &Value, events: &mut Vec<Event>) {
    match protocol {
        Protocol::Codex => codex(native, events),
        Protocol::Claude | Protocol::Qoder | Protocol::Qwen => stream_json(native, events),
        Protocol::Acp => acp(native, events),
        Protocol::Pi => pi(native, events),
        _ => {}
    }
}

#[cfg(test)]
mod grok_mcp_tests {
    use super::*;
    use serde_json::json;

    fn acp_result(update: Value) -> ToolResult {
        let mut events = Vec::new();
        project(
            Protocol::Acp,
            &json!({"method": "session/update", "params": {"update": update}}),
            &mut events,
        );
        events
            .into_iter()
            .find_map(|e| match e {
                Event::ToolResult { result, .. } => Some(result),
                _ => None,
            })
            .expect("expected ToolResult event")
    }

    #[test]
    fn grok_mcp_raw_output_projects_text_when_content_is_absent_or_empty() {
        let completed = acp_result(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "status": "completed",
            "rawOutput": {"type": "MCP", "output": {"OkayOutput": "42 rows"}}
        }));
        assert_eq!(completed.status, Some(ToolStatus::Succeeded));
        assert_eq!(completed.text().as_deref(), Some("42 rows"));

        let empty_content = acp_result(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-2",
            "status": "failed",
            "content": [],
            "rawOutput": {"type": "MCP", "output": {"Error": "connection refused"}}
        }));
        assert_eq!(empty_content.status, Some(ToolStatus::Failed));
        assert_eq!(empty_content.text().as_deref(), Some("connection refused"));

        let explicit_content = acp_result(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-3",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "explicit"}}],
            "rawOutput": {"type": "MCP", "output": {"OkayOutput": "ignored"}}
        }));
        assert_eq!(explicit_content.text().as_deref(), Some("explicit"));
    }
}
