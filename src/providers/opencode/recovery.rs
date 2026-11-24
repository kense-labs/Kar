//! ACP load replays the provider's own tool cards. The native archive supplies the
//! user/assistant identities and the execution boundary that ACP omits on replay.
use super::history::{self, Witness};
use crate::{
    Error, Event, Outcome, Result, Session, SessionOptions,
    chat::{ChatEdit, ChatFrame, ChatInfo, ChatTurn, Item, Role},
    observe::Tool,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub(crate) fn has_witness(turn: &ChatTurn, root: &str) -> bool {
    turn.prompt().is_some() && Witness::for_turn(turn, root).is_ok()
}

enum Part {
    Text(Role, String, String),
    Tool(String),
}

struct Turn {
    user: String,
    steered_users: Vec<String>,
    failure: Option<crate::TurnFailure>,
    parts: Vec<Part>,
    tools: HashSet<String>,
}

impl Turn {
    fn assistant(&mut self, row: &Value) -> Option<()> {
        let message = row["id"].as_str()?;
        let mut ordinal = 0;
        let mut text_slot = None;
        for part in row["content"].as_array()? {
            match part["type"].as_str()? {
                "text" => {
                    let text = part["text"].as_str()?;
                    // ACP uses the assistant message ID for all text spans in the step.
                    if let Some(Part::Text(_, _, existing)) =
                        text_slot.and_then(|i: usize| self.parts.get_mut(i))
                    {
                        existing.push_str(text);
                    } else {
                        text_slot = Some(self.parts.len());
                        self.parts
                            .push(Part::Text(Role::Assistant, message.into(), text.into()));
                    }
                }
                "reasoning" => {
                    let key = format!("{message}:reasoning:{ordinal}");
                    self.parts.push(Part::Text(
                        Role::Reasoning,
                        key,
                        part["text"].as_str()?.into(),
                    ));
                    ordinal += 1;
                }
                "tool" => {
                    let id = part["id"].as_str()?;
                    if !matches!(
                        part["state"]["status"].as_str(),
                        Some("completed" | "error")
                    ) || !self.tools.insert(id.into())
                    {
                        return None;
                    }
                    self.parts.push(Part::Tool(id.into()));
                }
                _ => return None,
            }
        }
        Some(())
    }
}

struct Archive<'a> {
    witness: Witness<'a>,
    prompt: &'a str,
    accepted: Vec<&'a str>,
    in_turn: bool,
    matched: bool,
    hits: usize,
    current: Option<Turn>,
    settled: Option<(Turn, String, Outcome)>,
}

impl<'a> Archive<'a> {
    fn new(witness: Witness<'a>, prompt: &'a str, accepted: Vec<&'a str>) -> Self {
        Self {
            witness,
            prompt,
            accepted,
            in_turn: false,
            matched: false,
            hits: 0,
            current: None,
            settled: None,
        }
    }

    fn finish(self) -> Option<(Turn, String, Outcome)> {
        if self.hits == 1 { self.settled } else { None }
    }

    fn row(&mut self, row: &Value) -> Result<()> {
        let id = row["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("missing OpenCode message ID".into()))?;
        let kind = row["type"]
            .as_str()
            .ok_or_else(|| Error::Protocol("missing OpenCode message type".into()))?;
        if self.witness.matches(row) {
            self.hits += 1;
            self.matched = true;
        }
        match kind {
            "user" => {
                if !self.in_turn && row["text"] == self.prompt {
                    self.current = Some(Turn {
                        user: id.into(),
                        steered_users: Vec::new(),
                        failure: None,
                        parts: Vec::new(),
                        tools: HashSet::new(),
                    });
                } else if let Some(turn) = &mut self.current {
                    if self
                        .accepted
                        .get(turn.steered_users.len())
                        .copied()
                        .is_some_and(|expected| row["text"] == expected)
                    {
                        turn.steered_users.push(id.into());
                    } else {
                        self.current = None;
                    }
                }
                self.in_turn = true;
            }
            "assistant" => {
                if let Some(turn) = &mut self.current
                    && turn.assistant(row).is_none()
                {
                    self.current = None;
                }
            }
            "idle" => {
                let outcome = match row["outcome"].as_str() {
                    Some("succeeded") => Some(Outcome::Completed),
                    Some("failed") => Some(Outcome::Failed),
                    Some("interrupted") => Some(Outcome::Aborted),
                    _ => None,
                };
                if let Some(mut turn) = self.current.take()
                    && turn.steered_users.len() == self.accepted.len()
                    && self.matched
                    && let Some(outcome) = outcome
                {
                    if matches!(outcome, Outcome::Failed) {
                        turn.failure = crate::wire::failure::opencode_history(&row["error"]);
                    }
                    self.settled = Some((turn, id.into(), outcome));
                }
                self.matched = false;
                self.in_turn = false;
            }
            // Settings and system messages have no corresponding chat timeline item.
            "agent-switched" | "model-switched" | "location-switched" | "system" => {}
            _ => self.current = None,
        }
        Ok(())
    }
}

fn replayed_tools(
    records: &[std::sync::Arc<crate::Record>],
    root: &str,
    turn: &Turn,
) -> Option<HashMap<String, Tool>> {
    let mut request = None;
    let mut inside = false;
    let mut found_user = false;
    let mut tools = HashMap::<String, Tool>::new();
    for r in records {
        let packet = &r.native;
        if r.direction == crate::Direction::ToRuntime
            && packet["method"] == "session/load"
            && packet["params"]["sessionId"] == root
        {
            request = packet.get("id");
            continue;
        }
        if request.is_none() || r.direction != crate::Direction::FromRuntime {
            continue;
        }
        if packet.get("id") == request && packet.get("method").is_none() {
            return (found_user
                && packet.get("result").is_some()
                && tools.len() == turn.tools.len()
                && tools.values().all(|tool| tool.result.is_some()))
            .then_some(tools);
        }
        if packet["method"] != "session/update"
            || packet["params"]["sessionId"] != root
            || crate::wire::opencode::notice(packet, Some(root)).is_some()
        {
            continue;
        }
        let update = &packet["params"]["update"];
        match update["sessionUpdate"].as_str() {
            Some("user_message_chunk") => {
                let first = update["messageId"] == turn.user;
                found_user |= first;
                inside = first
                    || turn
                        .steered_users
                        .iter()
                        .any(|id| update["messageId"] == *id);
            }
            Some("tool_call" | "tool_call_update") if inside => {
                let id = update["toolCallId"].as_str()?;
                if !turn.tools.contains(id) {
                    continue;
                }
                let tool = tools.entry(id.into()).or_default();
                if tool.finished {
                    return None;
                }
                for event in &r.events {
                    if Tool::id(event) == Some(id) {
                        tool.apply(event, r.elided);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn consistent(local: &ChatTurn, root: &str, edits: &[ChatEdit]) -> bool {
    // A unique native ID identifies the turn. Also check every visible root item,
    // so a truncated or contradictory provider snapshot cannot overwrite the UI.
    let mut next = 0;
    for item in &local.items {
        if !item.source.path.is_empty() || item.source.session_id.as_deref() != Some(root) {
            continue;
        }
        if matches!(
            item.item,
            Item::Message {
                role: Role::User,
                ..
            }
        ) {
            continue;
        }
        let Some(offset) = edits[next..]
            .iter()
            .position(|edit| match (&item.item, edit) {
                (
                    Item::Message {
                        role,
                        provider_id: Some(id),
                        text,
                        complete,
                    },
                    ChatEdit::Text {
                        role: r,
                        key: Some(k),
                        text: whole,
                        ..
                    },
                ) => {
                    role == r
                        && id == k
                        && (if *complete {
                            whole == text
                        } else {
                            whole.starts_with(text)
                        })
                }
                (
                    Item::Tool { id, activity },
                    ChatEdit::ToolSnapshot {
                        id: k,
                        activity: whole,
                    },
                ) => {
                    id == k
                        && (activity.input_partial
                            || activity.input.as_ref().is_none_or(|input| {
                                input.as_object().is_some_and(serde_json::Map::is_empty)
                                    || whole.input.as_ref() == Some(input)
                            }))
                        && activity
                            .result
                            .as_ref()
                            .is_none_or(|result| whole.result.as_ref() == Some(result))
                }
                _ => false,
            })
        else {
            return false;
        };
        next += offset + 1;
    }
    true
}

pub(crate) async fn recover(
    info: &ChatInfo,
    session: &Session,
    opts: &SessionOptions,
    local: &ChatTurn,
) -> Result<Option<ChatFrame>> {
    let root = session.id();
    let (Some(prompt), Ok(witness)) = (local.prompt(), Witness::for_turn(local, root)) else {
        return Ok(None);
    };
    let records = session.records();
    if !records.iter().any(|r| {
        r.direction == crate::Direction::ToRuntime
            && r.native["method"] == "session/load"
            && r.native["params"]["sessionId"] == root
    }) {
        return Ok(None);
    }
    let rt = info.launch_runtime(opts).await?;
    if crate::account::release(&rt.launch_version(opts).await?).is_none_or(|v| v.0 != 2) {
        return Ok(None);
    }
    let read = async {
        let (server, _) = history::open(&rt.program, opts, root).await?;
        let accepted = local
            .items
            .iter()
            .filter_map(|item| {
                if item.source.session_id.as_deref() != Some(root) || !item.source.path.is_empty() {
                    return None;
                }
                let Item::Message {
                    role: Role::User,
                    text,
                    provider_id: Some(id),
                    ..
                } = &item.item
                else {
                    return None;
                };
                let key = uuid::Uuid::parse_str(id).ok()?;
                (local.steered_inputs.get(&key) == Some(&crate::SteerStatus::Accepted))
                    .then_some(text.as_str())
            })
            .collect();
        let mut archive = Archive::new(witness, prompt, accepted);
        history::visit(&server, root, |row| archive.row(row)).await?;
        server.stop().await;
        Ok::<_, Error>(archive.finish())
    };
    let Some((turn, idle, outcome)) =
        tokio::time::timeout(opts.handshake_timeout, read)
            .await
            .map_err(|_| Error::Timeout("OpenCode transcript recovery".into()))??
    else {
        return Ok(None);
    };
    if local
        .native_turn_id
        .as_ref()
        .is_some_and(|id| id != &turn.user)
    {
        return Ok(None);
    }
    let Some(mut tools) = replayed_tools(&records, root, &turn) else {
        return Ok(None);
    };
    let mut edits = Vec::new();
    for part in turn.parts {
        let edit = match part {
            Part::Text(role, key, text) => ChatEdit::Text {
                role,
                key: Some(key),
                text,
                final_text: true,
            },
            Part::Tool(id) => {
                let Some(activity) = tools.remove(&id) else {
                    return Ok(None);
                };
                ChatEdit::ToolSnapshot { id, activity }
            }
        };
        edits.push(edit);
    }
    if let Some(failure) = turn.failure {
        edits.push(ChatEdit::Activity {
            event: Event::TurnFailure { failure },
        });
    }
    edits.push(ChatEdit::Activity {
        event: Event::ForkAnchor { anchor: idle },
    });
    edits.push(ChatEdit::Activity {
        event: Event::TurnEnded { outcome },
    });
    session.opencode_transcript(turn.user, edits, opts, |edits| {
        consistent(local, root, edits)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repeated_tool_id_or_an_unfinished_turn_cannot_confirm_delivery() {
        let user = json!({"id":"question","type":"user","text":"same"});
        let tool = json!({"id":"answer","type":"assistant","content":[{"type":"tool","id":"call","state":{"status":"completed"}}]});
        let idle = json!({"id":"end","type":"idle","outcome":"succeeded"});
        let mut archived = Archive::new(Witness::Tool("call"), "same", vec![]);
        for row in [&user, &tool, &idle, &user, &tool, &idle] {
            archived.row(row).unwrap();
        }
        assert!(archived.finish().is_none());
        let mut open = Archive::new(Witness::Message("answer"), "same", vec![]);
        for row in [&user, &tool] {
            open.row(row).unwrap();
        }
        assert!(open.finish().is_none());
    }
}
