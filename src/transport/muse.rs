//! MSP identifies each turn by the UUIDv7 used for its turn/start command.
use crate::{Event, Outcome, Protocol, SessionOptions, input::Input};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Default)]
struct Item {
    revision: u64,
    kind: String,
    complete: bool,
}
pub(super) struct Projection {
    id: String,
    started: bool,
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
            started: false,
            settled: true,
            roots,
            effort: opts.settings.effort.clone(),
            items: HashMap::new(),
        }
    }
    pub fn prompt(&mut self, session: Option<&str>, input: &Input) -> (String, Value) {
        self.id = uuid::Uuid::now_v7().to_string();
        self.items.clear();
        self.started = false;
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
        if self.settled || session.is_none() || p["sessionId"].as_str() != session {
            return vec![];
        }
        if let Some(turn) = p["turnId"]
            .as_str()
            .or_else(|| p.pointer("/item/turnId").and_then(Value::as_str))
            && turn != self.id
        {
            return vec![];
        }
        match method {
            "turn/started" if p["turnId"] == self.id && !self.started => {
                self.started = true;
                vec![Event::TurnStarted]
            }
            "turn/completed" if p["turnId"] == self.id => {
                self.settled = true;
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
                if v["turnId"].as_str() != Some(self.id.as_str())
                    && p["turnId"].as_str() != Some(self.id.as_str())
                    && !self.items.contains_key(id)
                {
                    return vec![];
                }
                let item = self.items.entry(id.into()).or_default();
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
                if item.complete {
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

pub(super) fn accepts(expected: &str, result: &Value) -> bool {
    result["turnId"] == expected && result.get("commandId").is_none_or(|id| id == expected)
}

/// Native approvals have a notification identity distinct from the receipt of
/// the server's JSON-RPC request. Only the later decision command settles one.
pub(super) fn control(
    frame: &Value,
    session: Option<&str>,
    turn: Option<&str>,
) -> Option<(Value, String)> {
    let method = frame["method"].as_str()?;
    let p = &frame["params"];
    if session.is_none() || p["sessionId"].as_str() != session {
        return None;
    }
    if !matches!(method, "approval/resolved" | "userInput/settled")
        && (turn.is_none() || p["turnId"].as_str() != turn || !p["subagentOrigin"].is_null())
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
