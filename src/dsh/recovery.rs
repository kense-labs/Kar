//! Read only the completed native turn belonging to a durable host request.
use super::{projection::Projection, web::Web};
use crate::{
    Direction, Error, Event, Record, RecordKind, Result,
    chat::{ChatEdit, ChatFrame, ChatTurn, Item, Role},
    observe::Agent,
    stream::Journal,
};
use serde_json::{Value, json};

/// Work backwards at one inclusive cut so later appends cannot shift page boundaries.
pub(super) async fn capture(
    web: &Web,
    root: &str,
    snapshot: &Value,
    local: &ChatTurn,
    journal: &Journal,
    secrets: &[String],
) -> Result<()> {
    let cut = snapshot["cursor"]
        .as_i64()
        .ok_or_else(|| Error::Protocol("DSH snapshot omitted its cursor".into()))?;
    let mut expected = cut;
    let mut current;
    let mut page = snapshot;
    let mut scanner = Scan {
        request: local.id.to_string(),
        native: local.native_turn_id.as_deref(),
        goal: local.native_continuation,
        turn: None,
        matched: false,
        events: Vec::new(),
    };
    loop {
        let rows = page["records"]
            .as_array()
            .ok_or_else(|| Error::Protocol("DSH omitted history records".into()))?;
        for row in rows.iter().rev() {
            let e = &row["event"];
            if row["type"] != "event" || e["seq"].as_i64() != Some(expected) || expected < 0 {
                return Err(Error::Protocol("gap in DSH recovery history".into()));
            }
            expected -= 1;
            if let Some(turn) = scanner.read(e)? {
                scanner.events.reverse();
                let mut body = json!({"type":"session/events","localTurnId":local.id,"turn":turn,"records":scanner.events});
                crate::transport::redact(&mut body, secrets);
                journal.push(
                    Direction::FromRuntime,
                    RecordKind::Response,
                    Some(root.into()),
                    vec![],
                    body,
                    vec![],
                );
                return Ok(());
            }
        }
        match page["hasMore"].as_bool() {
            Some(false) if expected == -1 => return Ok(()),
            Some(true) if !rows.is_empty() && expected >= 0 => {}
            _ => {
                return Err(Error::Protocol(
                    "inconsistent DSH history continuation".into(),
                ));
            }
        }
        current = web
            .request(
                "session/events",
                json!({"address":{"kind":"session","sessionId":root},
            "throughSeq":cut,"beforeSeq":expected+1,"maxMessages":50}),
            )
            .await?;
        if current["cursor"].as_i64() != Some(cut) {
            return Err(Error::Protocol(
                "DSH changed the history cut during recovery".into(),
            ));
        }
        page = &current;
    }
}

struct Scan<'a> {
    request: String,
    native: Option<&'a str>,
    goal: bool,
    turn: Option<u64>,
    matched: bool,
    events: Vec<Value>,
}

impl Scan<'_> {
    fn read(&mut self, event: &Value) -> Result<Option<u64>> {
        let kind = event["type"].as_str();
        let data = &event["data"];
        let turn = data["turn"].as_u64();
        if kind == Some("turn/end") {
            // A new candidate supersedes any incomplete or unrelated range.
            self.events.clear();
            self.matched = false;
            self.turn = turn.filter(|t| self.native.is_none_or(|n| n == t.to_string()));
        }
        let Some(owner) = self.turn else {
            return Ok(None);
        };
        if kind == Some("turn/start") {
            self.turn = None;
            if turn != Some(owner) {
                return Err(Error::Protocol("mismatched DSH turn boundaries".into()));
            }
            return Ok(self.matched.then_some(owner));
        }
        // UserMessage carries its durable RPC identity but no turn field. Its
        // enclosing turn/start and turn/end establish ownership on this log.
        if kind == Some("user/message") {
            if event["surfaceOp"] == "append" {
                let source = &data["source"];
                if source["kind"] == "user" {
                    self.matched |= !self.goal && source["rpcId"] == self.request;
                    self.events.push(event.clone());
                } else if source["kind"] == "goal" && self.goal && self.native.is_some() {
                    self.matched = true;
                }
            }
            return Ok(None);
        }
        if turn != Some(owner) {
            return Ok(None);
        }
        if matches!(
            kind,
            Some(
                "assistant/message"
                    | "assistant/attempt"
                    | "tool/call"
                    | "tool/result"
                    | "turn/end"
            )
        ) {
            self.events.push(event.clone());
        }
        Ok(None)
    }
}

/// A single replayable edit replaces partial messages by their native snapshot
/// and updates tools by call ID. It does not re-publish past token observations.
pub(crate) fn frame(last: &ChatTurn, root: &str, record: &Record) -> Option<ChatFrame> {
    let n = &record.native;
    if record.session_id.as_deref() != Some(root)
        || !record.agent_path.is_empty()
        || n["type"] != "session/events"
        || n["localTurnId"] != last.id.to_string()
    {
        return None;
    }
    let id = n["turn"].as_u64()?.to_string();
    if last.native_turn_id.as_ref().is_some_and(|k| *k != id) {
        return None;
    }
    let rows = n["records"].as_array()?;
    let end = rows.last()?;
    if end["type"] != "turn/end" || end["data"]["turn"] != n["turn"] {
        return None;
    }
    let source = Agent {
        session_id: Some(root.into()),
        path: vec![],
    };
    // Early DSH recordings used process-local keys. Their local prefix still
    // identifies the corresponding native messages in that role's order.
    let source_ref = &source;
    let prior = |role: Role| {
        last.items.iter().filter_map(move |i| match &i.item {
            Item::Message {
                role: r,
                provider_id,
                ..
            } if &i.source == source_ref && *r == role => Some(provider_id.as_deref()),
            _ => None,
        })
    };
    let mut assistant = prior(Role::Assistant);
    let mut reasoning = prior(Role::Reasoning);
    let mut finished_calls = std::collections::HashMap::<String, usize>::new();
    let mut p = Projection::default();
    let mut edits = Vec::new();
    for e in rows {
        let d = &e["data"];
        if matches!(
            e["type"].as_str(),
            Some("assistant/message" | "assistant/attempt")
        ) {
            let key = p.message_key(d)?;
            for (part, text) in parts(d) {
                match part {
                    Part::Message(role) => {
                        let existing = if role == Role::Reasoning {
                            reasoning.next()
                        } else {
                            assistant.next()
                        }
                        .flatten();
                        let key = existing
                            .filter(|k| {
                                k.strip_prefix(root)
                                    .is_some_and(|suffix| suffix.starts_with(':'))
                            })
                            .unwrap_or(&key);
                        edits.push(ChatEdit::Text {
                            role,
                            key: Some(key.into()),
                            text,
                            final_text: true,
                        });
                    }
                    Part::Tool(id) => {
                        // A local delta may already be durable. Restore only its missing
                        // suffix; tool/call, if dispatched, follows with authoritative JSON.
                        let local = last
                            .items
                            .iter()
                            .filter_map(|item| match &item.item {
                                Item::Tool { id: seen, activity }
                                    if item.source == source && *seen == id =>
                                {
                                    Some(activity)
                                }
                                _ => None,
                            })
                            .nth(finished_calls.get(&id).copied().unwrap_or_default());
                        let delta = match local {
                            Some(tool) if !tool.input_partial => continue,
                            Some(tool) => {
                                let prefix =
                                    tool.input.as_ref().and_then(Value::as_str).unwrap_or("");
                                if let Some(suffix) = text.strip_prefix(prefix) {
                                    suffix.to_owned()
                                } else {
                                    edits.push(ChatEdit::Activity {
                                        event: Event::ToolInput {
                                            id: id.clone(),
                                            input: Value::Null,
                                        },
                                    });
                                    text
                                }
                            }
                            None => text,
                        };
                        edits.push(ChatEdit::Activity {
                            event: Event::ToolInputDelta { id, delta },
                        });
                    }
                }
            }
        }
        let events = p.event(root, e);
        for event in events {
            if let Event::ToolEnded { id } = &event {
                *finished_calls.entry(id.clone()).or_default() += 1;
            }
            match event {
                Event::UserMessage { text } if d["source"]["rpcId"] != last.id.to_string() => {
                    edits.push(ChatEdit::Text {
                        role: Role::User,
                        key: d["source"]["rpcId"].as_str().map(str::to_owned),
                        text,
                        final_text: true,
                    });
                }
                Event::ToolStarted { .. }
                | Event::ToolInput { .. }
                | Event::ToolResult { .. }
                | Event::ToolEnded { .. }
                | Event::TurnFailure { .. }
                | Event::TurnEnded { .. }
                | Event::ForkAnchor { .. } => edits.push(ChatEdit::Activity { event }),
                _ => {}
            }
        }
    }
    Some(ChatFrame {
        morph_progress: None,
        cursor: record.cursor(),
        source,
        elided: record.elided,
        native_turn: Some(id),
        edits,
    })
}

#[derive(PartialEq)]
enum Part {
    Message(Role),
    Tool(String),
}

/// The packed stream preserves the first appearance of interleaved text,
/// reasoning and tool arguments. A settled message supplies the final text.
fn parts(data: &Value) -> Vec<(Part, String)> {
    let mut parts: Vec<(Part, String)> = Vec::new();
    for run in data["stream"].as_array().into_iter().flatten() {
        let (part, fragments) = match run["type"].as_str() {
            Some("reasoning-chunks") => (Part::Message(Role::Reasoning), &run["texts"]),
            Some("text-chunks") => (Part::Message(Role::Assistant), &run["texts"]),
            Some("tool-call-chunks") if run["id"].is_string() => {
                (Part::Tool(run["id"].as_str().unwrap().into()), &run["args"])
            }
            _ => continue,
        };
        let Some(fragments) = fragments.as_array() else {
            continue;
        };
        let text = fragments
            .iter()
            .filter_map(Value::as_str)
            .collect::<String>();
        if let Some((_, prior)) = parts.iter_mut().find(|(k, _)| *k == part) {
            prior.push_str(&text);
        } else {
            parts.push((part, text));
        }
    }
    for (role, kind) in [(Role::Reasoning, "reasoning"), (Role::Assistant, "text")] {
        let text = super::projection::text(&data["message"]["content"], kind);
        if text.is_empty() {
            continue;
        }
        let part = Part::Message(role);
        if let Some((_, prior)) = parts.iter_mut().find(|(k, _)| *k == part) {
            *prior = text;
        } else {
            parts.push((part, text));
        }
    }
    parts
}
