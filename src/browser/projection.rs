use crate::Event;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn array(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}
fn live(v: &Value) -> &Value {
    &v["docs"]["pi.live"]
}
fn totals(v: &Value) -> crate::TokenTotals {
    let usage = &v["docs"]["pi.usage"];
    let mut total = crate::TokenTotals {
        cache_read: Some(0),
        cache_write: Some(0),
        ..Default::default()
    };
    for ledger in [&usage["models"], &usage["tools"]]
        .iter()
        .filter_map(|v| v.as_object())
    {
        for item in ledger.values() {
            let read = item["cacheRead"].as_i64().unwrap_or(0);
            let write = item["cacheWrite"].as_i64().unwrap_or(0);
            total = total.plus(crate::TokenTotals {
                input: item["input"]
                    .as_i64()
                    .unwrap_or(0)
                    .saturating_add(read)
                    .saturating_add(write),
                output: item["output"].as_i64().unwrap_or(0),
                cache_read: Some(read),
                cache_write: Some(write),
                reasoning: item["reasoning"].as_i64(),
            });
        }
    }
    total
}
fn appended<'a>(old: &Value, view: &'a Value, ops: &'a Value) -> Vec<&'a Value> {
    let original: Vec<_> = array(ops)
        .filter(|op| op[0] == "p" && op[1] == json!(["entries"]))
        .flat_map(|op| array(&op[4]))
        .filter(|v| v["id"].is_u64())
        .collect();
    if !original.is_empty() {
        return original;
    }
    let seen = old["entries"]
        .as_array()
        .and_then(|a| a.last())
        .and_then(|v| v["id"].as_u64())
        .or(old["before"].as_u64())
        .unwrap_or(0);
    array(&view["entries"])
        .filter(|v| v["id"].as_u64().is_some_and(|n| n > seen))
        .collect()
}
pub(super) fn ended(old: &Value, call: &Value) -> bool {
    let before = &live(old)["run"];
    let new = live(&call["value"]);
    let after = &new["run"];
    if before.is_null() {
        return false;
    }
    if after.is_null() {
        return true;
    }
    let path = json!(["docs", "pi.live", "run"]);
    let mut replaced = call["retired"] == true;
    for op in array(&call["ops"]) {
        if op[0] == "d" && op[1] == path {
            return true;
        }
        if op[0] == "r"
            || (op[0] == "s"
                && matches!(op[1].as_array(),Some(p) if !p.is_empty() && p.len()<=3 && path.as_array().unwrap().starts_with(p)))
        {
            replaced = true;
        }
    }
    let left: Vec<_> = array(&before["inputs"]).collect();
    let right: Vec<_> = array(&after["inputs"]).collect();
    replaced
        && before["taskId"] != after["taskId"]
        && ((!before["first"].is_null()
            && !after["first"].is_null()
            && before["first"] != after["first"])
            || (!left.is_empty() && !right.is_empty() && !left.iter().any(|id| right.contains(id)))
            || (!before["afterRun"].is_null() && after["afterRun"].is_null())
            || new["runFailure"]["taskId"] == before["taskId"])
}
#[derive(Default)]
struct Blocks(BTreeMap<usize, (String, String)>);
impl Blocks {
    fn read(&mut self, message: &Value, events: &mut Vec<Event>) {
        for (i, b) in array(&message["content"]).enumerate() {
            let kind = match b["type"].as_str() {
                Some(k @ ("text" | "thinking")) => k,
                _ => continue,
            };
            let Some(text) = b[kind].as_str() else {
                continue;
            };
            let old = self.0.insert(i, (kind.into(), text.into()));
            let prefix = old
                .as_ref()
                .filter(|(k, _)| k == kind)
                .map(|(_, s)| s.as_str())
                .unwrap_or("");
            if let Some(delta) = text.strip_prefix(prefix)
                && (!delta.is_empty() || (kind == "thinking" && old.is_none()))
            {
                events.push(if kind == "text" {
                    Event::TextDelta { text: delta.into() }
                } else {
                    Event::ReasoningDelta { text: delta.into() }
                });
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Projection {
    pub view: Value,
    pub inputs: BTreeSet<u64>,
    blocks: Blocks,
    baseline: crate::TokenTotals,
}
impl Projection {
    pub fn fold(&mut self, call: &Value, initial: bool, terminal: Vec<Event>) -> Vec<Event> {
        let old = &self.view;
        let view = &call["value"];
        let previous = live(old);
        let new = live(view);
        let mut events = vec![];
        let agent = &view["docs"]["pi.agent"];
        let prior = &old["docs"]["pi.agent"];
        if (initial || agent["model"] != prior["model"])
            && let (Some(p), Some(id)) = (
                agent["model"]["provider"].as_str(),
                agent["model"]["modelId"].as_str(),
            )
        {
            events.push(Event::Model {
                model: format!("{p}/{id}"),
            });
        }
        if (initial || agent["thinkingLevel"] != prior["thinkingLevel"])
            && let Some(effort) = agent["thinkingLevel"].as_str()
        {
            events.push(Event::Effort {
                effort: effort.into(),
            });
        }
        let total = totals(view);
        if initial {
            self.baseline = total;
        }
        if initial || total != totals(old) {
            let t = total.minus(self.baseline);
            let mut tokens = json!({"input":t.input,"output":t.output,"cacheRead":t.cache_read,"cacheWrite":t.cache_write});
            if let Some(n) = t.reasoning {
                tokens["reasoning"] = n.into();
            }
            events.push(Event::Usage {
                usage: json!({"tokens":tokens}),
            });
        }
        let entries = if initial {
            vec![]
        } else {
            appended(old, view, &call["ops"])
        };
        for entry in &entries {
            for message in array(&entry["model"]) {
                match message["role"].as_str() {
                    Some("assistant") => {
                        if entry["byTaskId"] == previous["run"]["taskId"] {
                            self.blocks.read(message, &mut events);
                            self.blocks = Blocks::default();
                        } else {
                            Blocks::default().read(message, &mut events);
                        }
                    }
                    Some("user") => {
                        let content = &message["content"];
                        let text = content.as_str().map(str::to_owned).unwrap_or_else(|| {
                            array(content)
                                .filter(|b| b["type"] == "text")
                                .filter_map(|b| b["text"].as_str())
                                .collect()
                        });
                        events.push(Event::UserMessage { text });
                    }
                    _ => {}
                }
            }
        }
        let (removed, tool_events) =
            crate::tool_activity::durable::changes(old, view, &entries, initial);
        events.extend(removed);
        let boundary = !initial && ended(old, call);
        events.extend(terminal);
        if boundary || new["run"].is_null() {
            self.inputs.clear();
        }
        if !new["run"].is_null() && (initial || previous["run"].is_null() || boundary) {
            events.push(Event::TurnStarted);
        }
        self.inputs
            .extend(array(&new["run"]["inputs"]).filter_map(Value::as_u64));
        if previous["run"]["taskId"] != new["run"]["taskId"]
            || previous["generation"]["attempt"] != new["generation"]["attempt"]
            || new["generation"]["message"].is_null()
        {
            self.blocks = Blocks::default();
        }
        self.blocks.read(&new["generation"]["message"], &mut events);
        events.extend(tool_events);
        if let Some(retry) = new["generation"]["retry"].as_object()
            && (initial
                || new["generation"]["retry"]["at"] != previous["generation"]["retry"]["at"])
        {
            events.push(Event::Retry {
                attempt: new["generation"]["attempt"].as_u64(),
                max_attempts: None,
                delay_ms: None,
                reason: Some(
                    retry
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                ),
            });
        }
        for c in array(&new["compactions"]) {
            if c["blocking"] == true
                && !array(&previous["compactions"]).any(|old| old["taskId"] == c["taskId"])
            {
                events.push(Event::Compaction {
                    state: "started".into(),
                });
            }
        }
        self.view = view.clone();
        events
    }
}
