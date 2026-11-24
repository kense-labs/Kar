use crate::{
    ContextUsage, Event, FailureClass, Outcome, TaskChange, TaskDetails, TaskStatus, TaskType,
    TokenReading, TokenTotals, ToolResult, ToolStatus, TurnFailure,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

fn string(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}
pub(super) struct Frame {
    pub raw: Value,
    pub path: Vec<String>,
    pub span: Option<String>,
    pub events: Vec<Event>,
}
#[derive(Default)]
struct Item {
    kind: String,
    text: String,
    partial_input: String,
    complete_input: Option<Value>,
    started: bool,
    ended: bool,
}
#[derive(Default)]
struct Agent {
    initial: bool,
    historical: HashSet<String>,
    system: HashSet<String>,
    started: HashSet<String>,
    ended: HashSet<String>,
    items: HashMap<String, Item>,
    active: Option<String>,
    steps: HashMap<String, HashMap<u64, TokenTotals>>,
    usage: TokenTotals,
}
#[derive(Default)]
pub(super) struct Projection {
    agents: HashMap<String, Agent>,
    paths: HashMap<String, Vec<String>>,
    pub task_agents: HashMap<String, String>,
    ended_tasks: HashSet<String>,
    pub subscriptions: Vec<String>,
}
impl Projection {
    pub fn agent_path(&self, agent: &str) -> Vec<String> {
        if agent == "main" {
            Vec::new()
        } else {
            self.paths
                .get(agent)
                .cloned()
                .unwrap_or_else(|| vec![agent.into()])
        }
    }
    pub fn active(&self) -> Option<&str> {
        self.agents.get("main")?.active.as_deref()
    }
    fn child(&mut self, path: &[String], call: &str, agent: &Value) {
        if let Some(id) = agent.as_str().filter(|id| !id.is_empty() && *id != "main")
            && !self.paths.contains_key(id)
        {
            let mut p = path.to_vec();
            p.push(call.into());
            self.paths.insert(id.into(), p);
            self.subscriptions.push(id.into());
        }
    }
    pub fn frames(&mut self, raw: Value) -> Vec<Frame> {
        let agent = raw["agent_id"].as_str().unwrap_or("main").to_owned();
        if raw["type"] != "turns" {
            return vec![self.one(raw, &agent)];
        }
        let a = self.agents.entry(agent.clone()).or_default();
        let initial = !a.initial;
        a.initial = true;
        let mut out = Vec::new();
        for turn in raw["turns"].as_array().into_iter().flatten() {
            let Some(id) = turn["turn_id"].as_str() else {
                continue;
            };
            let a = self.agents.get_mut(&agent).unwrap();
            if initial && agent == "main" && turn["status"] == "ended" && !a.started.contains(id) {
                a.historical.insert(id.into());
            }
            if a.historical.contains(id) {
                continue;
            }
            // A snapshot contains many spans and items. Give each its own durable message identity.
            let mut header = turn.clone();
            if let Some(obj) = header.as_object_mut() {
                obj.remove("items");
            }
            out.push(self.one(
                json!({"type":"turn.started","agent_id":agent,"turn":header}),
                &agent,
            ));
            for item in turn["items"].as_array().into_iter().flatten() {
                let kind = if item["status"] == "running" {
                    "item.started"
                } else {
                    "item.ended"
                };
                out.push(self.one(
                    json!({"type":kind,"turn_id":id,"agent_id":agent,"item":item}),
                    &agent,
                ));
            }
            if turn["status"] == "ended" {
                out.push(self.one(
                    json!({"type":"turn.ended","agent_id":agent,"turn":header}),
                    &agent,
                ));
            }
        }
        out
    }
    fn one(&mut self, v: Value, agent: &str) -> Frame {
        let path = self.agent_path(agent);
        let span = string(&v["turn_id"])
            .or_else(|| string(&v["turn"]["turn_id"]))
            .or_else(|| self.agents.get(agent)?.active.clone());
        let mut events = vec![];
        let item = &v["item"];
        let kind = v["type"].as_str().unwrap_or_default();
        let id = v["item_id"].as_str().or_else(|| item["item_id"].as_str());
        if let Some(id) = id {
            for child in item["agent_refs"]
                .as_array()
                .into_iter()
                .chain(v["set"]["agent_refs"].as_array())
                .flatten()
            {
                self.child(&path, id, &child["agent_id"]);
            }
        }
        match kind {
            "task" => self.task(&v, agent, &path, &mut events),
            "task.list" => {
                for t in v["tasks"].as_array().into_iter().flatten() {
                    self.task(
                        t,
                        t["agent_id"].as_str().unwrap_or(agent),
                        &path,
                        &mut events,
                    );
                }
            }
            _ => {
                let a = self.agents.entry(agent.into()).or_default();
                if !span.as_ref().is_some_and(|t| a.historical.contains(t)) {
                    match kind {
                        "agent" => {
                            if let Some(model) = string(&v["model"]) {
                                events.push(Event::Model { model })
                            }
                            if let Some(effort) = string(&v["thinking"]) {
                                events.push(Event::Effort { effort })
                            }
                        }
                        "context" if v["context_tokens"].is_u64() => {
                            let mut usage = ContextUsage::counts(
                                v["context_tokens"].as_u64(),
                                v["max_context_tokens"].as_u64(),
                                false,
                            );
                            usage.percent = usage.percent.map(|x| x.min(100.0));
                            events.push(Event::ContextUsage { usage });
                        }
                        "turn.started" | "turn.ended" => {
                            if let Some(t) = v["turn"]["turn_id"].as_str() {
                                if v["turn"]["kind"] == "system" {
                                    a.system.insert(t.into());
                                } else if kind == "turn.started"
                                    && !a.ended.contains(t)
                                    && a.started.insert(t.into())
                                {
                                    a.active = Some(t.into());
                                    events.push(Event::TurnStarted);
                                } else if kind == "turn.ended" && a.ended.insert(t.into()) {
                                    if a.active.as_deref() == Some(t) {
                                        a.active = None;
                                    }
                                    let base = a
                                        .steps
                                        .remove(t)
                                        .unwrap_or_default()
                                        .into_values()
                                        .fold(TokenTotals::default(), TokenTotals::plus);
                                    if let Some(total) = tokens(&v["turn"]["usage"]) {
                                        a.account(total, base, &mut events);
                                    }
                                    let outcome = turn_outcome(&v["turn"], &mut events);
                                    events.push(Event::TurnEnded { outcome });
                                }
                            }
                        }
                        "turn.updated" => {
                            if let (Some(t), Some(i), Some(u)) = (
                                &span,
                                v["step"]["index"].as_u64(),
                                tokens(&v["step"]["usage"]),
                            ) && !a.ended.contains(t)
                                && !a.system.contains(t)
                            {
                                let steps = a.steps.entry(t.into()).or_default();
                                let base = steps.get(&i).copied().unwrap_or_default();
                                steps.insert(i, high_water(u, base));
                                a.account(u, base, &mut events);
                            }
                            let retry = &v["retry"];
                            if retry["attempt"].is_number()
                                || retry
                                    .pointer("/error/message")
                                    .is_some_and(Value::is_string)
                            {
                                events.push(Event::Retry {
                                    attempt: retry["attempt"].as_u64(),
                                    max_attempts: retry["max_attempts"].as_u64(),
                                    delay_ms: retry["delay_ms"].as_u64(),
                                    reason: string(&retry["error"]["message"]),
                                });
                            }
                        }
                        "item.started" | "item.ended" | "item.updated" | "item.delta" => {
                            if let Some(id) = id
                                && (!span.as_ref().is_some_and(|t| a.system.contains(t))
                                    || item["kind"] == "compaction")
                                && !span.as_ref().is_some_and(|t| a.ended.contains(t))
                            {
                                a.items.entry(id.into()).or_default().update(
                                    kind,
                                    id,
                                    &v,
                                    &mut events,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Frame {
            raw: v,
            path,
            span,
            events,
        }
    }
    fn task(&mut self, v: &Value, agent: &str, path: &[String], events: &mut Vec<Event>) {
        let Some(id) = v["task_id"].as_str() else {
            return;
        };
        self.child(
            path,
            v["parent_tool_call_id"].as_str().unwrap_or(id),
            &v["child_agent_id"],
        );
        if self.ended_tasks.contains(id) {
            return;
        }
        let mut emit = |change| {
            events.push(Event::Task {
                id: id.into(),
                change,
            })
        };
        if self.task_agents.insert(id.into(), agent.into()).is_none() {
            let task_type = match v["kind"].as_str() {
                Some("subagent") => TaskType::Agent,
                Some("shell") => TaskType::Shell,
                _ => TaskType::Other,
            };
            emit(TaskChange::Started {
                task_type,
                details: TaskDetails {
                    native_type: string(&v["kind"]),
                    description: string(&v["description"]),
                    tool_call_id: string(&v["parent_tool_call_id"]),
                    background: v["detached"].as_bool(),
                    ..Default::default()
                },
            });
        } else if v["status"] == "running" {
            emit(TaskChange::Updated {
                status: Some(TaskStatus::Running),
                description: string(&v["description"]),
                background: v["detached"].as_bool(),
                error: None,
            });
        }
        if v["status"] == "ended" {
            self.ended_tasks.insert(id.into());
            let status = match v["outcome"].as_str() {
                Some("completed") => TaskStatus::Completed,
                Some("killed") => TaskStatus::Stopped,
                _ => TaskStatus::Failed,
            };
            emit(TaskChange::Ended {
                status,
                summary: string(&v["result_summary"]),
                output: None,
                output_file: None,
            });
        }
    }
}
fn tokens(v: &Value) -> Option<TokenTotals> {
    let n = |key: &str| v[key].as_i64().filter(|n| *n >= 0);
    let (i, o, r, w) = (
        n("input_other"),
        n("output"),
        n("input_cache_read"),
        n("input_cache_creation"),
    );
    if [i, o, r, w].iter().all(Option::is_none) {
        return None;
    }
    Some(TokenTotals {
        input: i
            .unwrap_or(0)
            .saturating_add(r.unwrap_or(0))
            .saturating_add(w.unwrap_or(0)),
        output: o.unwrap_or(0),
        cache_read: r,
        cache_write: w,
        reasoning: None,
    })
}
fn high_water(u: TokenTotals, b: TokenTotals) -> TokenTotals {
    TokenTotals {
        input: u.input.max(b.input),
        output: u.output.max(b.output),
        cache_read: u.cache_read.max(b.cache_read),
        cache_write: u.cache_write.max(b.cache_write),
        reasoning: None,
    }
}
impl Agent {
    fn account(&mut self, u: TokenTotals, base: TokenTotals, out: &mut Vec<Event>) {
        let delta = high_water(u, base).minus(base);
        if delta.input == 0
            && delta.output == 0
            && delta.cache_read.unwrap_or(0) == 0
            && delta.cache_write.unwrap_or(0) == 0
        {
            return;
        }
        self.usage = self.usage.plus(delta);
        out.push(Event::TokenUsage {
            reading: TokenReading {
                tokens: Some(self.usage),
                ..Default::default()
            },
        });
    }
}
pub(crate) fn turn_outcome(t: &Value, events: &mut Vec<Event>) -> Outcome {
    if t["outcome"] == "completed" {
        return Outcome::Completed;
    }
    let reason = t["end_reason"].as_str().unwrap_or_default();
    if t["outcome"] == "interrupted" || matches!(reason, "user_cancelled" | "aborted") {
        return Outcome::Aborted;
    }
    let e = &t["error"];
    let status = e["status_code"]
        .as_u64()
        .and_then(|n| u16::try_from(n).ok());
    let class = match (e["code"].as_str(), reason, status) {
        (Some("provider.auth_error"), _, _) => FailureClass::Auth,
        (Some("provider.rate_limit"), _, _) => FailureClass::RateLimited,
        (Some("provider.overloaded"), _, _) => FailureClass::Overloaded,
        (Some("provider.filtered"), _, _) | (_, "filtered" | "blocked", _) => {
            FailureClass::ModelRefused
        }
        (Some("context.overflow"), _, _) => FailureClass::InputTooLarge,
        (Some("model.not_configured" | "model.config_invalid"), _, _) => {
            FailureClass::ModelUnavailable
        }
        (Some("loop.max_steps_exceeded"), _, _) | (_, "max_steps", _) => FailureClass::LimitReached,
        (Some("provider.connection_error"), _, _) => FailureClass::Provider,
        _ => {
            let class = crate::wire::failure::from_status(status);
            if class == FailureClass::Unknown && e["code"] == "provider.api_error" {
                FailureClass::Provider
            } else {
                class
            }
        }
    };
    let mut failure = TurnFailure::new(
        class,
        e["message"]
            .as_str()
            .or(e["code"].as_str())
            .unwrap_or("Kimi turn failed"),
    );
    failure.status = status;
    if class == FailureClass::Auth && status == Some(401) {
        failure.credential = Some(crate::CredentialProblem::Rejected);
    }
    events.push(Event::TurnFailure { failure });
    Outcome::Failed
}
pub(crate) fn tool_snapshot(id: &str, item: &Value) -> Vec<Event> {
    let mut events = Vec::new();
    let kind = if item["status"] == "running" {
        "item.started"
    } else {
        "item.ended"
    };
    Item::default().update(kind, id, &json!({"item":item}), &mut events);
    events
}
impl Item {
    fn tool_input(&mut self, id: &str, item: &Value, events: &mut Vec<Event>) {
        if let Some(input) = item.get("input").filter(|v| !v.is_null()) {
            if self.complete_input.as_ref() != Some(input) {
                events.push(Event::ToolInput {
                    id: id.into(),
                    input: input.clone(),
                });
                self.complete_input = Some(input.clone());
                self.partial_input.clear();
            }
        } else if self.complete_input.is_none()
            && let Some(snapshot) = item["input_text"].as_str()
            && let Some(suffix) = snapshot
                .strip_prefix(&self.partial_input)
                .filter(|s| !s.is_empty())
        {
            events.push(Event::ToolInputDelta {
                id: id.into(),
                delta: suffix.into(),
            });
            self.partial_input = snapshot.into();
        }
    }

    fn update(&mut self, kind: &str, id: &str, v: &Value, events: &mut Vec<Event>) {
        if self.ended {
            return;
        }
        let item = &v["item"];
        if let Some(k) = item["type"].as_str() {
            self.kind = k.into();
        }
        let end = kind == "item.ended";
        let call = id.to_owned();
        if kind == "item.delta" {
            let Some(delta) = string(&v["delta"]).filter(|d| !d.is_empty()) else {
                return;
            };
            match v["field"].as_str() {
                Some("text") => {
                    self.text.push_str(&delta);
                    events.push(if self.kind == "reasoning" {
                        Event::ReasoningDelta { text: delta }
                    } else {
                        Event::TextDelta { text: delta }
                    });
                }
                Some("input_text") if self.complete_input.is_none() => {
                    self.partial_input.push_str(&delta);
                    events.push(Event::ToolInputDelta { id: call, delta });
                }
                Some("output") => events.push(Event::ToolProgress {
                    id: call,
                    output: None,
                    delta: Some(delta),
                }),
                _ => {}
            }
            return;
        }
        if kind == "item.updated" {
            self.tool_input(id, &v["set"], events);
            if let Some(output) = string(&v["set"]["output"]) {
                events.push(Event::ToolProgress {
                    id: call,
                    output: Some(output),
                    delta: None,
                });
            }
            return;
        }
        match self.kind.as_str() {
            "user_message" => {
                let text = item["text"].as_str().unwrap_or("");
                if !self.started {
                    if !end && text.is_empty() {
                        return;
                    }
                    events.push(Event::UserMessage { text: text.into() });
                }
            }
            "assistant_message" | "reasoning" => {
                if let Some(text) = item["text"].as_str() {
                    if let Some(suffix) = text.strip_prefix(&self.text).filter(|s| !s.is_empty()) {
                        events.push(if self.kind == "reasoning" {
                            Event::ReasoningDelta {
                                text: suffix.into(),
                            }
                        } else {
                            Event::TextDelta {
                                text: suffix.into(),
                            }
                        });
                        self.text = text.into();
                    }
                    if end && self.kind == "assistant_message" {
                        events.push(Event::Message { text: text.into() });
                    }
                }
            }
            "tool_call" => {
                if !self.started {
                    events.push(Event::ToolStarted {
                        id: call.clone(),
                        name: item["name"].as_str().unwrap_or("unknown").into(),
                    });
                }
                self.tool_input(id, item, events);
                if let Some(output) = string(&item["output"]) {
                    events.push(Event::ToolProgress {
                        id: call.clone(),
                        output: Some(output),
                        delta: None,
                    });
                }
                if end {
                    let status = if item["error"].is_string() || item["outcome"] == "failed" {
                        Some(ToolStatus::Failed)
                    } else if item["outcome"] == "completed" {
                        Some(ToolStatus::Succeeded)
                    } else {
                        None
                    };
                    let raw = item
                        .get("output")
                        .filter(|v| !v.is_null())
                        .or_else(|| item.get("error"));
                    let content = raw.and_then(crate::tool_activity::parts);
                    events.push(Event::ToolResult {
                        id: call.clone(),
                        result: ToolResult {
                            status,
                            content,
                            exit_code: item["fields"]["exit_code"].as_i64(),
                            ..Default::default()
                        },
                    });
                    events.push(Event::ToolEnded { id: call });
                }
            }
            "system" if item["kind"] == "compaction" => {
                if !self.started {
                    events.push(Event::Compaction {
                        state: "started".into(),
                    });
                }
                if end {
                    let state = match item["outcome"].as_str() {
                        Some("failed") => "failed",
                        Some("interrupted") => "aborted",
                        _ => "completed",
                    };
                    events.push(Event::Compaction {
                        state: state.into(),
                    });
                }
            }
            _ => {}
        }
        self.started = true;
        self.ended = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn events(p: &mut Projection, v: Value) -> Vec<Event> {
        p.frames(v).into_iter().flat_map(|f| f.events).collect()
    }
    #[test]
    fn tool_snapshots_extend_partial_input_and_settle_only_on_native_input() {
        let mut item = Item::default();
        let mut card = crate::observe::Tool::default();
        let mut step = |kind: &str, value: Value| {
            let mut projected = vec![];
            item.update(kind, "call", &value, &mut projected);
            for event in &projected {
                card.apply(event, false);
            }
            (card.clone(), projected)
        };
        let running = |s: &str| json!({"item":{"type":"tool_call","name":"Read","input_text":s}});
        let (partial, events) = step("item.started", running("{"));
        assert!(partial.input_partial);
        assert!(
            matches!(&events[..],[Event::ToolStarted{..},Event::ToolInputDelta{delta,..}] if delta=="{")
        );
        assert!(step("item.started", running("{")).1.is_empty());
        step(
            "item.delta",
            json!({"field":"input_text","delta":"\"path\":"}),
        );
        let (expanded, events) = step("item.started", running("{\"path\":\"a\"}"));
        assert!(expanded.input_partial);
        assert!(matches!(&events[..],[Event::ToolInputDelta{delta,..}] if delta=="\"a\"}"));
        assert!(step("item.started", running("{")).1.is_empty());
        let args = json!({"path":"a"});
        let (ready, events) = step("item.updated", json!({"set":{"input":args}}));
        assert!(matches!(&events[..], [Event::ToolInput { .. }]));
        assert_eq!(ready.input, Some(args.clone()));
        assert!(!ready.input_partial);
        assert!(
            step("item.delta", json!({"field":"input_text","delta":"stale"}))
                .1
                .is_empty()
        );
        let (finished, events) = step(
            "item.ended",
            json!({"item":{"type":"tool_call","input":args,"output":"ok"}}),
        );
        assert!(finished.finished && finished.result.is_some());
        assert!(!events.iter().any(|e| matches!(e, Event::ToolInput { .. })));
        let recovered = tool_snapshot(
            "call",
            &json!({"type":"tool_call","status":"ended","input_text":"{}"}),
        );
        assert!(
            recovered
                .iter()
                .any(|e| matches!(e,Event::ToolInputDelta{delta,..} if delta=="{}"))
        );
        assert!(
            !recovered
                .iter()
                .any(|e| matches!(e, Event::ToolInput { .. }))
        );
    }
    #[test]
    fn ending_tool_supplies_arguments_without_an_update() {
        let args = json!({"path":"example.rs"});
        for start in [
            Value::Null,
            json!({"input_text":"{"}),
            json!({"input":args}),
        ] {
            let mut first = json!({"type":"tool_call","name":"Read"});
            if let Some(fields) = start.as_object() {
                first.as_object_mut().unwrap().extend(fields.clone());
            }
            let mut item = Item::default();
            let mut events = vec![];
            item.update("item.started", "call", &json!({"item":first}), &mut events);
            let end = json!({"item":{"type":"tool_call","input":args,"output":"ok"}});
            item.update("item.ended", "call", &end, &mut events);
            item.update("item.ended", "call", &end, &mut events);
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e,Event::ToolInput{input,..} if input==&args))
                    .count(),
                1
            );
            let mut card = crate::observe::Tool::default();
            for event in &events {
                card.apply(event, false);
            }
            assert_eq!(card.input.as_ref(), Some(&args));
            assert!(!card.input_partial && card.finished);
        }
    }
    #[test]
    fn root_history_and_repeated_snapshots_do_not_duplicate_turns_or_items() {
        let mut p = Projection::default();
        let old = json!({"turn_id":"old","status":"ended","outcome":"completed","items":[{"type":"assistant_message","item_id":"old-text","text":"history"}]});
        assert!(events(&mut p, json!({"type":"turns","turns":[old]})).is_empty());
        assert!(events(&mut p,json!({"type":"item.ended","turn_id":"old","item":{"type":"user_message","item_id":"old-user","text":"stale"}})).is_empty());
        let live = json!({"turn_id":"new","status":"running","items":[{"type":"assistant_message","item_id":"m","status":"running","text":"你"}]});
        let snap = json!({"type":"turns","turns":[old,live]});
        assert_eq!(
            events(&mut p, snap.clone()),
            [Event::TurnStarted, Event::TextDelta { text: "你".into() }]
        );
        assert!(events(&mut p, snap).is_empty());
        assert_eq!(
            events(
                &mut p,
                json!({"type":"item.delta","turn_id":"new","item_id":"m","field":"text","delta":"好"})
            ),
            [Event::TextDelta { text: "好".into() }]
        );
        let tip = json!({"type":"turns","turns":[{"turn_id":"new","status":"ended","outcome":"completed","items":[{"type":"assistant_message","item_id":"m","text":"你好！"}]}]});
        assert_eq!(
            events(&mut p, tip.clone()),
            [
                Event::TextDelta { text: "！".into() },
                Event::Message {
                    text: "你好！".into()
                },
                Event::TurnEnded {
                    outcome: Outcome::Completed
                }
            ]
        );
        assert!(events(&mut p, tip).is_empty());
    }
    #[test]
    fn nested_agent_usage_excludes_replayed_steps_and_root_history() {
        let mut p = Projection::default();
        let parent = json!({"type":"item.started","item":{"type":"tool_call","name":"Agent","item_id":"task-call","agent_refs":[{"agent_id":"helper"}]}});
        events(&mut p, parent.clone());
        events(&mut p, parent);
        assert_eq!(p.subscriptions, ["helper"]);
        let step = json!({"type":"turn.updated","agent_id":"helper","turn_id":"t","step":{"index":1,"usage":{"input_other":10,"input_cache_read":4,"output":3}}});
        let f = p.frames(step.clone()).pop().unwrap();
        assert_eq!(f.path, ["task-call"]);
        let usage = |events: Vec<Event>| {
            events.into_iter().find_map(|e| {
                if let Event::TokenUsage { reading } = e {
                    reading.tokens
                } else {
                    None
                }
            })
        };
        assert_eq!(usage(f.events).unwrap().input, 14);
        assert!(events(&mut p, step).is_empty());
        let ended = json!({"type":"turn.ended","agent_id":"helper","turn":{"turn_id":"t","outcome":"failed","error":{"status_code":503,"message":"busy"},"usage":{"input_other":12,"input_cache_read":4,"output":5}}});
        let es = events(&mut p, ended.clone());
        assert_eq!(usage(es.clone()).unwrap().input, 16);
        assert!(es.iter().any(
            |e| matches!(e,Event::TurnFailure{failure} if failure.class==FailureClass::Overloaded)
        ));
        assert!(events(&mut p, ended).is_empty());
        let root = json!({"type":"turn.updated","turn_id":"r","step":{"index":1,"usage":{"input_other":2,"output":1}}});
        assert_eq!(usage(events(&mut p, root)).unwrap().input, 2);
    }
}
