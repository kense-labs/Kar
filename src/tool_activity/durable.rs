use super::{ToolResult, ToolStatus, parts};
use crate::Event;
use serde_json::Value;
use std::collections::HashMap;

#[cfg(test)]
mod tests;

fn rows(v: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

type Key<'a> = (&'a str, Option<u64>);

fn slots(live: &Value) -> impl Iterator<Item = (Key<'_>, &Value)> {
    rows(&live["tools"])
        .chain(rows(&live["nestedTools"]))
        .filter_map(|slot| Some(((slot["callId"].as_str()?, slot["taskId"].as_u64()), slot)))
}

fn result_message<'a>(entry: &'a Value, id: &str) -> Option<&'a Value> {
    rows(&entry["model"]).find(|m| m["role"] == "toolResult" && m["toolCallId"] == id)
}

fn arguments<'a>(
    slot: &'a Value,
    view: &'a Value,
    appended: &[&'a Value],
    id: &str,
) -> Option<&'a Value> {
    if let Some(args) = slot.get("arguments") {
        return Some(args);
    }
    let first = view["docs"]["pi.live"]["run"]["first"].as_u64();
    appended
        .iter()
        .copied()
        .rev()
        .chain(rows(&view["entries"]).rev())
        .filter(|entry| first.is_none_or(|n| entry["id"].as_u64().is_some_and(|id| id >= n)))
        .flat_map(|entry| rows(&entry["model"]).rev())
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| rows(&m["content"]))
        .find(|block| block["type"] == "toolCall" && block["id"] == id)
        .and_then(|block| block.get("arguments"))
}

fn finish(events: &mut Vec<Event>, id: &str, message: Option<&Value>, summary: Option<&Value>) {
    let status = message
        .or(summary)
        .and_then(|v| v["isError"].as_bool())
        .map(|failed| {
            if failed {
                ToolStatus::Failed
            } else {
                ToolStatus::Succeeded
            }
        });
    let content = message.and_then(|m| parts(&m["content"]));
    if status.is_some() || content.is_some() {
        events.push(Event::ToolResult {
            id: id.into(),
            result: ToolResult {
                status,
                content,
                ..Default::default()
            },
        });
    }
    events.push(Event::ToolEnded { id: id.into() });
}

/// Removed slots finish in the outgoing turn. New slots and progress belong
/// after any turn boundary in the same Harness watch publication.
pub(crate) fn changes(
    old: &Value,
    view: &Value,
    appended: &[&Value],
    initial: bool,
) -> (Vec<Event>, Vec<Event>) {
    let before = &old["docs"]["pi.live"];
    let after = &view["docs"]["pi.live"];
    let previous: HashMap<_, _> = slots(before).collect();
    let current: HashMap<_, _> = slots(after).collect();
    let mut removed = Vec::new();
    let mut events = Vec::new();
    for (key @ (id, task), prior) in slots(before) {
        if initial || prior["status"] == "done" || current.contains_key(&key) {
            continue;
        }
        let result = appended.iter().find_map(|entry| {
            let owner = entry["byTaskId"].as_u64();
            if task.is_some() && owner.is_some() && task != owner {
                return None;
            }
            let message = result_message(entry, id)?;
            // An explicit entry reference also disambiguates records without byTaskId.
            if current.values().any(|s| {
                s["status"] == "done"
                    && s["callId"] == id
                    && s["entry"]
                        .as_u64()
                        .is_some_and(|n| Some(n) == entry["id"].as_u64())
            }) {
                return None;
            }
            Some(message)
        });
        if prior["status"] != "pending" || result.is_some() {
            finish(&mut removed, id, result, None);
        }
    }
    for (key @ (id, _), slot) in slots(after) {
        let prior = previous.get(&key).copied().unwrap_or(&Value::Null);
        let status = &slot["status"];
        if *status == "pending" || (initial && *status == "done") {
            continue;
        }
        let started = initial || prior.is_null() || prior["status"] == "pending";
        if started {
            events.push(Event::ToolStarted {
                id: id.into(),
                name: slot["name"].as_str().unwrap_or("tool").into(),
            });
        }
        if (started || slot.get("arguments") != prior.get("arguments"))
            && let Some(input) = arguments(slot, view, appended, id)
        {
            events.push(Event::ToolInput {
                id: id.into(),
                input: input.clone(),
            });
        }
        if *status == "running"
            && let Some(output) = slot["output"].as_str()
            && (started || slot["output"] != prior["output"])
        {
            events.push(Event::ToolUpdated { id: id.into() });
            events.push(Event::ToolProgress {
                id: id.into(),
                output: Some(output.into()),
                delta: None,
            });
        }
        if *status == "done" && prior["status"] != "done" {
            let result = slot["entry"].as_u64().and_then(|target| {
                appended
                    .iter()
                    .copied()
                    .chain(rows(&view["entries"]))
                    .filter(|entry| entry["id"].as_u64() == Some(target))
                    .find_map(|entry| result_message(entry, id))
            });
            finish(&mut events, id, result, slot.get("summary"));
        }
    }
    (removed, events)
}
