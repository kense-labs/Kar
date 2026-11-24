//! Import Kar voyage v3 session recordings.
use crate::{Direction, Error, Event, Outcome, Record, RecordKind, Result};
use serde::Serialize;
use serde_json::Value;
use std::io::BufRead;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct VoyageSummary {
    pub stream_id: Uuid,
    pub header: Value,
    pub end: Option<Value>,
    pub records: u64,
}
/// A missing end marker describes an interrupted capture. Invalid records are errors.
pub fn import(
    reader: impl BufRead,
    mut accept: impl FnMut(Record) -> Result<()>,
) -> Result<VoyageSummary> {
    let mut lines = reader.lines();
    let first = lines
        .next()
        .transpose()?
        .ok_or_else(|| Error::Protocol("empty voyage".into()))?;
    let header: Value = serde_json::from_str(&first)?;
    if header["kind"] != "header" || header["format"] != "kar-voyage/3" {
        return Err(Error::Protocol(
            "expected a kar-voyage/3 recording header".into(),
        ));
    }
    let mut summary = VoyageSummary {
        header,
        stream_id: Uuid::new_v4(),
        end: None,
        records: 0,
    };
    for line in lines {
        let value: Value = serde_json::from_str(&line?)?;
        if summary.end.is_some() {
            return Err(Error::Protocol("data after voyage end".into()));
        }
        match value["kind"].as_str() {
            Some("record") => {
                let rec = &value["record"];
                if rec["seq"].as_u64() != Some(summary.records) {
                    return Err(Error::Protocol(format!(
                        "expected upstream record {}",
                        summary.records
                    )));
                }
                accept(convert(rec, summary.stream_id, summary.records)?)?;
                summary.records += 1;
            }
            Some("end") => summary.end = Some(value),
            _ => return Err(Error::Protocol("unknown voyage line".into())),
        }
    }
    Ok(summary)
}
fn convert(v: &Value, stream_id: Uuid, seq: u64) -> Result<Record> {
    let kind: RecordKind = serde_json::from_value(v["kind"].clone())?;
    let body = &v["body"];
    let session_id = v["sessionId"]
        .as_str()
        .ok_or_else(|| Error::Protocol("missing upstream sessionId".into()))?;
    let direction = match (kind, v["direction"].as_str(), body["kind"].as_str()) {
        (RecordKind::Frame, _, _) | (_, Some("toApp"), _) => Direction::FromRuntime,
        (_, Some("toRuntime"), _) | (_, _, Some("answered")) => Direction::ToRuntime,
        _ => Direction::Host,
    };
    let mut events = vec![];
    for event in body["events"].as_array().into_iter().flatten() {
        if event["kind"] == "turn_ended"
            && event["outcome"]["kind"] == "completed"
            && let Some(value) = event["outcome"].get("structuredOutput")
        {
            events.push(Event::StructuredOutput {
                value: value.clone(),
            });
        }
        if let Some(e) = project(event) {
            events.push(e)
        }
        if event["kind"] == "usage"
            && let Some(value) = event.pointer("/usage/context").filter(|v| v.is_object())
            && let Ok(usage) = serde_json::from_value(value.clone())
        {
            events.push(Event::ContextUsage { usage });
        }
        project_tool_detail(event, &mut events);
        if event["kind"] == "turn_ended"
            && let Some(anchor) = event["forkAnchor"].as_str()
        {
            events.push(Event::ForkAnchor {
                anchor: anchor.into(),
            });
        }
    }
    if kind == RecordKind::Request
        && v["direction"] == "toApp"
        && let Some(id) = v.get("id")
    {
        events.push(Event::ApprovalRequested {
            id: id.clone(),
            method: body["type"].as_str().unwrap_or("native").into(),
        });
    }
    if kind == RecordKind::Response && body["kind"] == "exited" {
        events.push(Event::Exited {
            reason: body["reason"].as_str().unwrap_or("runtime exited").into(),
        });
    }
    if kind == RecordKind::Response
        && body["kind"] == "answered"
        && let Some(id) = v.get("requestId")
    {
        events.push(Event::RequestResolved { id: id.clone() });
    }
    Ok(Record {
        stream_id,
        seq,
        timestamp_ms: v["receivedAt"]
            .as_u64()
            .ok_or_else(|| Error::Protocol("missing upstream receivedAt".into()))?,
        direction,
        kind,
        session_id: Some(session_id.into()),
        span_id: v["spanId"].as_str().map(str::to_owned),
        agent_path: serde_json::from_value(v["agentPath"].clone())?,
        elided: body["elided"] == true,
        native: body.get("native").unwrap_or(body).clone(),
        upstream: Some(v.clone()),
        events,
    })
}
fn project_tool_detail(v: &Value, events: &mut Vec<Event>) {
    let Some(id) = v["callId"].as_str() else {
        return;
    };
    match v["kind"].as_str() {
        Some("tool_call_started" | "tool_call_input") => {
            if v["kind"] == "tool_call_started"
                && v["tool"].is_string()
                && (v["server"].is_string() || v["toolName"].is_string())
            {
                events.push(Event::ToolIdentity {
                    id: id.into(),
                    native_type: v["tool"].as_str().unwrap().into(),
                    server: v["server"].as_str().map(str::to_owned),
                    tool_name: v["toolName"].as_str().map(str::to_owned),
                });
            }
            if let Some(input) = v.get("arguments").or_else(|| v.get("input")) {
                events.push(Event::ToolInput {
                    id: id.into(),
                    input: input.clone(),
                });
            }
        }
        Some("tool_call_input_delta") => {
            if let Some(delta) = v["delta"].as_str() {
                events.push(Event::ToolInputDelta {
                    id: id.into(),
                    delta: delta.into(),
                });
            }
        }
        Some("tool_call_progress") => {
            let output = v["output"].as_str().map(str::to_owned);
            let delta = v["outputDelta"].as_str().map(str::to_owned);
            if output.is_some() || delta.is_some() {
                events.push(Event::ToolProgress {
                    id: id.into(),
                    output,
                    delta,
                });
            }
        }
        Some("tool_call_ended") => {
            let status = match v["result"].as_str() {
                Some("ok") => Some(crate::ToolStatus::Succeeded),
                Some("failed") => Some(crate::ToolStatus::Failed),
                Some("cancelled") => Some(crate::ToolStatus::Cancelled),
                _ => None,
            };
            let content = v["content"].as_array().map(|list| {
                list.iter()
                    .map(|part| {
                        serde_json::from_value(part.clone()).unwrap_or_else(|_| {
                            crate::OutputPart::Other {
                                value: part.clone(),
                            }
                        })
                    })
                    .collect()
            });
            let file_changes = v["fileChanges"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| serde_json::from_value(f.clone()).ok())
                .collect();
            let result = crate::ToolResult {
                status,
                exit_code: v["exitCode"].as_i64(),
                content,
                file_changes,
            };
            if result != crate::ToolResult::default() {
                events.push(Event::ToolResult {
                    id: id.into(),
                    result,
                });
            }
        }
        _ => {}
    }
}
fn project(v: &Value) -> Option<Event> {
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    Some(match v["kind"].as_str()? {
        "app_answered" => Event::RequestResolved {
            id: v.get("requestId")?.clone(),
        },
        "input_queued" => Event::InputQueued {
            input_id: text("inputId")?,
        },
        "session_linked" if v["via"] == "tool_call" => Event::SessionLinked {
            parent: text("parent")?,
            child: text("child")?,
        },
        "service_tier" => Event::ServiceTier {
            tier: text("serviceTier")?,
        },
        "mcp_server_startup" => Event::McpServerStartup {
            server: text("server")?,
            status: serde_json::from_value(v.get("status")?.clone()).ok()?,
        },
        "goal_changed" => Event::GoalChanged {
            goal: serde_json::from_value(v.get("goal")?.clone()).ok()?,
        },
        label @ ("task_started" | "task_updated" | "task_ended") => {
            let mut fields = v.clone();
            fields["action"] = label.strip_prefix("task_")?.into();
            let change = serde_json::from_value(fields).ok()?;
            Event::Task {
                id: text("taskId")?,
                change,
            }
        }
        "turn_active" => Event::TurnStarted,
        "model" => Event::Model {
            model: text("model")?,
        },
        "effort" => Event::Effort {
            effort: text("effort")?,
        },
        "input_dropped" => Event::InputDropped {
            input_id: text("inputId")?,
        },
        "retry" => Event::Retry {
            reason: text("reason")?,
        },
        "compaction_started" => Event::Compaction {
            state: "started".into(),
        },
        "compaction_ended" => Event::Compaction {
            state: text("outcome")?,
        },
        "user_message" => Event::UserMessage {
            text: text("input")?,
        },
        "text_delta" => Event::TextDelta {
            text: text("text")?,
        },
        "tool_call_started" => Event::ToolStarted {
            id: text("callId")?,
            name: text("tool")?,
        },
        "tool_call_progress" => Event::ToolUpdated {
            id: text("callId")?,
        },
        "tool_call_ended" => Event::ToolEnded {
            id: text("callId")?,
        },
        "usage" => Event::Usage {
            usage: v.get("usage")?.clone(),
        },
        "turn_started" => Event::TurnStarted,
        "turn_ended" => Event::TurnEnded {
            outcome: match v.pointer("/outcome/kind")?.as_str()? {
                "completed" => Outcome::Completed,
                "aborted" => Outcome::Aborted,
                "failed" => Outcome::Failed,
                _ => return None,
            },
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn imported_tool_result_and_pending_request_replay() {
        let header =
            json!({"kind":"header","format":"kar-voyage/3","sessionId":"s","runtime":"codex"});
        let bodies = [
            json!({"kind":"request","id":"ask","direction":"toApp","body":{"kind":"native","type":"item/tool/requestUserInput","native":{"questions":[{"id":"q"}]}}}),
            json!({"kind":"response","requestId":"ask","body":{"kind":"answered","native":{}}}),
            json!({"kind":"frame","body":{"events":[
                {"kind":"tool_call_started","callId":"c","tool":"shell","input":"exit 7"},
                {"kind":"tool_call_progress","callId":"c","output":"x","outputDelta":"y"},
                {"kind":"tool_call_started","callId":"nested","tool":"leaf","arguments":{"line":4}},
                {"kind":"tool_call_ended","callId":"c","result":"failed","exitCode":7,"content":[{"type":"text","text":"done"}],
                 "fileChanges":[{"path":"a","kind":"modified","moveTo":"b"}]},
                {"kind":"turn_ended","outcome":{"kind":"completed","structuredOutput":null}}
            ]}}),
        ];
        let mut source = format!("{header}\n");
        for (seq, mut r) in bodies.into_iter().enumerate() {
            r["seq"] = seq.into();
            r["sessionId"] = "s".into();
            r["agentPath"] = json!([]);
            r["receivedAt"] = 1.into();
            source.push_str(&format!("{}\n", json!({"kind":"record","record":r})));
        }
        let mut view = crate::observe::SessionView::default();
        let mut state = crate::observe::SessionReducer::new("s");
        import(source.as_bytes(), |r| {
            state.apply(&r)?;
            if r.seq == 0 {
                assert_eq!(
                    state.snapshot().pending_requests[0].kind,
                    crate::RequestKind::Question
                );
            }
            view.apply(&r)
        })
        .unwrap();
        assert!(state.snapshot().pending_requests.is_empty());
        let agent = view.agents.values().next().unwrap();
        assert_eq!(agent.last_structured_output, Some(Value::Null));
        assert_eq!(state.snapshot().last_structured_output, Some(Value::Null));
        assert_eq!(agent.tools["nested"].input, Some(json!({"line":4})));
        let tool = &agent.tools["c"];
        assert_eq!(tool.preview, "xy");
        let result = tool.result.as_ref().unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.file_changes[0].move_to.as_deref(), Some("b"));
    }
    #[test]
    fn preserves_attribution_unknown_native_data_and_interrupted_captures() {
        let header =
            json!({"kind":"header","format":"kar-voyage/3","sessionId":"root","runtime":"codex"});
        let native = json!({"kind":"frame","seq":0,"receivedAt":812,"sessionId":"child","agentPath":["tool-1"],"spanId":"turn-9","body":{"type":"custom","native":{"future":"field"},"events":[{"kind":"text_delta","text":"hello"},{"kind":"future_event"}]}});
        let row = json!({"kind":"record","record":native});
        let data = format!("{header}\n{row}\n");
        let mut converted = vec![];
        let summary = import(data.as_bytes(), |r| {
            converted.push(r);
            Ok(())
        })
        .unwrap();
        assert!(summary.end.is_none());
        let record = &converted[0];
        assert_eq!(record.upstream.as_ref(), Some(&native));
        assert_eq!(record.agent_path, ["tool-1"]);
        assert_eq!(record.session_id.as_deref(), Some("child"));
        assert_eq!(
            record.events,
            [Event::TextDelta {
                text: "hello".into()
            }]
        );
        assert!(import(format!("{data}{row}").as_bytes(), |_| Ok(())).is_err());
    }
}
