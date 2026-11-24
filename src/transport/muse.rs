//! MSP identifies each turn by the UUIDv7 used for its turn/start command.
use crate::{Event, Outcome, Protocol, SessionOptions, input::Input};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
struct Item {
    revision: u64,
    owner: String,
    kind: String,
    complete: bool,
}
pub(super) struct Projection {
    id: String,
    autonomous: bool,
    joined: HashSet<String>,
    ended: HashSet<String>,
    decisions: HashMap<String, Value>,
    resolved: HashSet<String>,
    opening: Option<(String, bool)>,
    early: Vec<Value>,
    early_bytes: usize,
    started: bool,
    accepted: bool,
    settled: bool,
    roots: Vec<std::path::PathBuf>,
    effort: Option<String>,
    items: HashMap<String, Item>,
}
impl Projection {
    pub fn new(opts: &SessionOptions) -> Self {
        let cwd = opts.cwd.canonicalize().unwrap_or_else(|_| opts.cwd.clone());
        let mut roots = vec![cwd.clone()];
        if let Ok(dirs) = opts.settings.directories(&cwd) {
            roots.extend(dirs);
        }
        Self {
            id: String::new(),
            autonomous: false,
            joined: Default::default(),
            ended: Default::default(),
            decisions: Default::default(),
            resolved: Default::default(),
            opening: None,
            early: vec![],
            early_bytes: 0,
            started: false,
            accepted: false,
            settled: true,
            roots,
            effort: opts.settings.effort.clone(),
            items: HashMap::new(),
        }
    }
    pub fn opening(&mut self, id: &str, resumed: bool) {
        self.opening = Some((id.into(), resumed));
    }

    pub fn hold_before_open(&mut self, packet: &Value) -> crate::Result<bool> {
        if packet.get("id").is_some()
            || packet["method"].as_str().is_none()
            || !self
                .opening
                .as_ref()
                .is_some_and(|(id, _)| packet["params"]["sessionId"] == *id)
        {
            return Ok(false);
        }
        // A native notification can race its session/resume receipt. Wait for the
        // confirmed identity and the pre-existing turn before attributing it.
        let size = serde_json::to_vec(packet)?.len();
        if self.early.len() >= 4096 || self.early_bytes.saturating_add(size) > 16 * 1024 * 1024 {
            return Err(crate::Error::Closed(
                "Muse exceeded the pending-open event limit".into(),
            ));
        }
        self.early_bytes += size;
        self.early.push(packet.clone());
        Ok(true)
    }

    pub fn opened(&mut self, result: &Value) -> Option<(String, Vec<Value>)> {
        let (expected, resumed) = self.opening.take()?;
        let frames = std::mem::take(&mut self.early);
        self.early_bytes = 0;
        if result.pointer("/session/sessionId")?.as_str()? != expected
            || result
                .pointer("/history/noneReason")
                .is_some_and(|s| s == "projectionUnavailable")
        {
            return None;
        }
        // MSP exposes the currently running ID when reopening a session. A new
        // transport has no per-item history or admission receipt for that run.
        if resumed
            && let Some(id) = result
                .pointer("/session/activeTurnId")
                .and_then(Value::as_str)
        {
            self.ended.insert(id.into());
        }
        Some((expected, frames))
    }

    pub fn live_turn(&self) -> Option<&str> {
        (!self.settled && (self.started || self.accepted)).then_some(self.id.as_str())
    }
    pub fn admitted(&mut self, native: &str) {
        if self.id == native {
            self.accepted = true;
        }
    }
    pub fn refused_prompt(&mut self, id: &str) {
        if self.id == id && !self.started {
            self.settled = true;
            self.ended.insert(id.into());
            self.ended.extend(self.joined.drain());
        }
    }
    pub fn autonomous(&self) -> Option<&str> {
        (self.autonomous && !self.settled).then_some(self.id.as_str())
    }

    pub fn owns(&self, id: &str) -> bool {
        !self.settled && (self.id == id || self.joined.contains(id))
    }

    pub fn can_publish(&self, key: &str) -> bool {
        !self.resolved.contains(key)
    }
    pub fn resolve(&mut self, key: &str) {
        self.decisions.remove(key);
        self.resolved.insert(key.into());
    }
    pub fn reserve_decision(&mut self, key: &str, body: &Value) -> crate::Result<Value> {
        let revision = body
            .get("currentRequirementId")
            .unwrap_or(&body["userInputId"]);
        if self.decisions.get(key) == Some(revision) {
            return Err(crate::Error::Refused(
                "Muse decision already submitted for this requirement".into(),
            ));
        }
        self.decisions.insert(key.into(), revision.clone());
        Ok(revision.clone())
    }
    pub fn refused_decision(&mut self, key: &str, revision: &Value) {
        if self.decisions.get(key) == Some(revision) {
            self.decisions.remove(key);
        }
    }
    pub fn interrupt_targets(&self) -> Vec<String> {
        if self.settled {
            return vec![];
        }
        let mut ids: Vec<_> = self.joined.iter().cloned().collect();
        ids.sort();
        ids.push(self.id.clone());
        ids
    }

    pub fn prompt(&mut self, session: Option<&str>, input: &Input) -> (String, Value) {
        self.id = uuid::Uuid::now_v7().to_string();
        self.autonomous = false;
        self.items.clear();
        self.started = false;
        self.accepted = false;
        self.settled = false;
        let mut args = json!({"sessionId":session,"commandId":self.id,"workspaceRoots":self.roots,
            "input":input.blocks(Protocol::Muse),"displayText":input.text,"ifBusy":"queue"});
        if let Some(effort) = &self.effort {
            args["reasoningEffort"] = effort.as_str().into();
        }
        (self.id.clone(), args)
    }

    pub fn events(&mut self, frame: &Value, session: Option<&str>) -> Vec<Event> {
        let method = frame["method"].as_str().unwrap_or("");
        let p = &frame["params"];
        if session.is_none() || p["sessionId"].as_str() != session {
            return vec![];
        }
        let reported = p["turnId"]
            .as_str()
            .or_else(|| p.pointer("/item/turnId").and_then(Value::as_str));
        if let Some(turn) = reported
            && method == "turn/started"
            && self.ended.contains(turn)
        {
            return vec![];
        }
        if let (Some(turn), "turn/started") = (reported, method) {
            if turn.is_empty() {
                return vec![];
            }
            if self.settled {
                self.id = turn.into();
                self.autonomous = true;
                self.started = true;
                self.settled = false;
                self.items.clear();
                let id = continuation_id(session.expect("matched session"), turn);
                return vec![
                    Event::NativeTurnStarted {
                        id,
                        native_turn: turn.into(),
                    },
                    Event::TurnStarted,
                ];
            }
            if self.id != turn {
                self.joined.insert(turn.into());
                return vec![];
            }
        }
        if self.settled || reported.is_some_and(|turn| !self.owns(turn)) {
            return vec![];
        }
        if method == "turn/completed"
            && let Some(turn) = reported
            && self.joined.remove(turn)
        {
            self.ended.insert(turn.into());
            return vec![];
        }
        match method {
            "turn/started" if p["turnId"] == self.id && !self.started => {
                self.started = true;
                vec![Event::TurnStarted]
            }
            "turn/completed" if p["turnId"] == self.id => {
                self.settled = true;
                self.ended.insert(self.id.clone());
                self.ended.extend(self.joined.drain());
                let outcome = match p["terminal"].as_str() {
                    Some("completed") => Outcome::Completed,
                    Some("cancelled") => Outcome::Aborted,
                    _ => Outcome::Failed,
                };
                let mut events = vec![];
                if outcome == Outcome::Failed {
                    let reason = p
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .or_else(|| p["reason"].as_str())
                        .unwrap_or("Muse turn failed");
                    events.push(Event::TurnFailure {
                        failure: crate::TurnFailure::new(
                            crate::FailureClass::Unknown,
                            reason.to_owned(),
                        ),
                    });
                }
                events.push(Event::TurnEnded { outcome });
                events
            }
            "item/started" | "item/updated" | "item/completed" => {
                let v = &p["item"];
                let (Some(id), Some(kind), Some(rev)) = (
                    v["itemId"].as_str(),
                    v["kind"].as_str(),
                    v["revision"].as_u64(),
                ) else {
                    return vec![];
                };
                if v["turnId"].as_str().is_some_and(|t| !self.owns(t))
                    || (reported.is_none() && !self.items.contains_key(id))
                {
                    return vec![];
                }
                let item = self.items.entry(id.into()).or_default();
                if let Some(turn) = reported {
                    if !item.owner.is_empty() && item.owner != turn {
                        return vec![];
                    }
                    item.owner = turn.into();
                }
                if item.revision >= rev || item.complete {
                    return vec![];
                }
                item.revision = rev;
                item.kind = kind.to_owned();
                item.complete = method == "item/completed"
                    || v["status"].as_str().is_some_and(|s| s != "inProgress");
                if kind == "agentMessage"
                    && item.complete
                    && let Some(text) = v["text"].as_str()
                {
                    return vec![Event::Message { text: text.into() }];
                }
                if let Some(tool) = v["tool"].as_str() {
                    return vec![if method == "item/started" {
                        Event::ToolStarted {
                            id: id.into(),
                            name: tool.into(),
                        }
                    } else if item.complete {
                        Event::ToolEnded { id: id.into() }
                    } else {
                        Event::ToolUpdated { id: id.into() }
                    }];
                }
                vec![]
            }
            "item/delta" => {
                let Some(item) = p["itemId"].as_str().and_then(|id| self.items.get(id)) else {
                    return vec![];
                };
                let Some(text) = p["delta"].as_str() else {
                    return vec![];
                };
                if item.complete
                    || !self.owns(&item.owner)
                    || reported.is_some_and(|turn| turn != item.owner)
                {
                    return vec![];
                }
                if item.kind == "agentMessage" && p["field"].as_str().unwrap_or("text") == "text" {
                    vec![Event::TextDelta { text: text.into() }]
                } else if item.kind == "reasoning" {
                    vec![Event::ReasoningDelta { text: text.into() }]
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }
}

pub(super) fn continuation_id(session: &str, turn: &str) -> uuid::Uuid {
    crate::chat::stable_id(["kar-muse-continuation", session, turn])
}

pub(super) fn accepts(expected: &str, result: &Value) -> bool {
    result["turnId"] == expected && result.get("commandId").is_none_or(|id| id == expected)
}

/// Native approvals have a notification identity distinct from the receipt of
/// the server's JSON-RPC request. Only the later decision command settles one.
pub(super) fn control(
    frame: &Value,
    session: Option<&str>,
    owns_turn: bool,
) -> Option<(Value, String)> {
    let method = frame["method"].as_str()?;
    let p = &frame["params"];
    if session.is_none() || p["sessionId"].as_str() != session {
        return None;
    }
    let child = method.starts_with("approval/")
        && p.pointer("/subagentOrigin/subagentId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
    if !matches!(method, "approval/resolved" | "userInput/settled")
        && !(child || (owns_turn && p["subagentOrigin"].is_null()))
    {
        return None;
    }
    let field = match method {
        "approval/requested" | "approval/updated" | "approval/resolved" => "approvalId",
        "userInput/requested" | "userInput/settled" => "userInputId",
        _ => return None,
    };
    Some((
        Value::String(p[field].as_str()?.to_owned()),
        method.to_owned(),
    ))
}

pub(super) fn same_request(previous: &Value, next: &Value) -> bool {
    ["approvalId", "userInputId", "sessionId", "turnId"]
        .into_iter()
        .all(|k| next.get(k).is_none() || previous[k] == next[k])
        && next
            .pointer("/subagentOrigin/subagentId")
            .is_none_or(|id| previous.pointer("/subagentOrigin/subagentId") == Some(id))
}

pub(super) fn answer_command(
    method: &str,
    p: &Value,
    answer: &Value,
) -> crate::Result<(&'static str, Value)> {
    let mut params = json!({"sessionId":p["sessionId"],"commandId":uuid::Uuid::now_v7()});
    if answer["cancelled"] == true {
        let turn = p["turnId"].as_str().ok_or_else(|| {
            crate::Error::Refused("Muse did not identify a turn to interrupt".into())
        })?;
        params["turnId"] = turn.into();
        return Ok(("turn/interrupt", params));
    }
    match method {
        "approval/requested" | "approval/updated" => {
            let choice = answer["choiceId"]
                .as_str()
                .filter(|choice| {
                    p["availableChoices"]
                        .as_array()
                        .is_some_and(|all| all.iter().any(|v| v["choiceId"] == *choice))
                })
                .ok_or_else(|| {
                    crate::Error::Refused("Muse did not offer this approval choice".into())
                })?;
            params["approvalId"] = p["approvalId"].clone();
            params["requirementId"] = p["currentRequirementId"].clone();
            params["choiceId"] = choice.into();
            Ok(("approval/decide", params))
        }
        "userInput/requested" if answer["answers"].is_array() => {
            params["userInputId"] = p["userInputId"].clone();
            params["answers"] = answer["answers"].clone();
            Ok(("userInput/answer", params))
        }
        _ => Err(crate::Error::Refused(
            "Muse requires answers to the current native questions".into(),
        )),
    }
}
