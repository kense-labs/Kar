//! Classify only native Antigravity launch calls; ACP tools can share their human title.
use crate::Event;
use serde_json::Value;

pub(crate) const LAUNCH: &str = "antigravity_subagent";

pub(crate) fn project(root: Option<&str>, v: &Value, events: &mut Vec<Event>) {
    let params = &v["params"];
    if root.is_none() || params["sessionId"].as_str() != root {
        return;
    }
    let (tool, permission) = match v["method"].as_str() {
        Some("session/request_permission") => (&params["toolCall"], true),
        Some("session/update")
            if matches!(
                params["update"]["sessionUpdate"].as_str(),
                Some("tool_call" | "tool_call_update")
            ) =>
        {
            (&params["update"], false)
        }
        _ => return,
    };
    let Some(id) = tool["toolCallId"].as_str().filter(|s| !s.is_empty()) else {
        return;
    };
    let title = tool["title"].as_str().unwrap_or_default();
    let native_type = if tool.pointer("/_meta/is_mcp_tool_call") == Some(&Value::Bool(true)) {
        "mcp"
    } else if tool
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| k != "other")
    {
        "acp_tool"
    } else if matches!(
        title,
        "Running start_subagent"
            | "Run start_subagent?"
            | "Running invoke_subagent"
            | "Run invoke_subagent?"
            | "Run invoke_subagent"
    ) {
        LAUNCH
    } else {
        return;
    };
    if permission && native_type == LAUNCH {
        events.push(Event::ToolStarted {
            id: id.into(),
            name: title.into(),
        });
        if let Some(input) = tool.get("rawInput") {
            events.push(Event::ToolInput {
                id: id.into(),
                input: input.clone(),
            });
        }
    }
    events.push(Event::ToolIdentity {
        id: id.into(),
        native_type: native_type.into(),
        server: None,
        tool_name: None,
    });
}
