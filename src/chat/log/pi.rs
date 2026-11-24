//! Reconcile a Pi RPC user echo with the active branch of its v3 session file.
use crate::{
    Event, Outcome, Protocol, Result,
    chat::{
        ChatEdit, Role,
        timeline::{pi_reply_key, snapshot_tools},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::Path,
};

struct Entry {
    parent: Option<usize>,
    children: usize,
    offset: u64,
}

fn matches_input(message: &Value, text: &str) -> bool {
    match &message["content"] {
        Value::String(s) => s == text,
        Value::Array(parts) => {
            parts
                .iter()
                .all(|b| b["type"] == "text" && b["text"].is_string())
                && parts
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<String>()
                    == text
        }
        _ => false,
    }
}

pub(crate) fn recover(
    path: &Path,
    root: &str,
    cwd: &Path,
    timestamp: u64,
    prompt: &str,
) -> Result<Option<(String, Vec<ChatEdit>)>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let before = file.metadata()?;
    let cut = before.len();
    let mut reader = BufReader::new(file);
    let mut offset = 0;
    let mut buf = Vec::new();
    let mut ids = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut input = None;
    // Keep offsets and ancestry, not the contents of other conversations or branches.
    while offset < cut {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)? as u64;
        if n > cut - offset || buf.last() != Some(&b'\n') {
            return Ok(None);
        }
        let Ok(row) = serde_json::from_slice::<Value>(&buf) else {
            return Ok(None);
        };
        let at = offset;
        offset += n;
        if at == 0 {
            if row["type"] != "session"
                || row["version"] != 3
                || row["id"] != root
                || !row["cwd"]
                    .as_str()
                    .and_then(|p| std::fs::canonicalize(p).ok())
                    .is_some_and(|p| p == cwd)
            {
                return Ok(None);
            }
            continue;
        }
        let Some(id) = row["id"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let parent = match row.get("parentId") {
            Some(Value::Null) => None,
            Some(Value::String(p)) => match ids.get(p).copied() {
                Some(index) => Some(index),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        let index = entries.len();
        if ids.insert(id.to_owned(), index).is_some() {
            return Ok(None);
        }
        if let Some(parent) = parent {
            entries[parent].children += 1
        }
        entries.push(Entry {
            parent,
            children: 0,
            offset: at,
        });
        let message = &row["message"];
        if row["type"] == "message"
            && message["role"] == "user"
            && message["timestamp"].as_u64() == Some(timestamp)
        {
            // Milliseconds alone are not a unique message identity.
            if input.is_some() || !matches_input(message, prompt) {
                return Ok(None);
            }
            input = Some((index, id.to_owned()));
        }
    }
    let Some((index, id)) = input else {
        return Ok(None);
    };
    // Pi rebuilds its leaf from the last persisted entry on open. Following
    // parentId excludes records appended on a different branch.
    let mut cursor = entries.len() - 1;
    let mut branch = Vec::new();
    while cursor != index {
        branch.push(cursor);
        let Some(parent) = entries[cursor].parent else {
            return Ok(None);
        };
        cursor = parent;
    }
    let mut turn = Turn::default();
    let mut next_user = false;
    let mut failed_entry = None;
    let mut anchor = None;
    for child in branch.into_iter().rev() {
        let entry = &entries[child];
        reader.seek(SeekFrom::Start(entry.offset))?;
        buf.clear();
        reader.read_until(b'\n', &mut buf)?;
        let Ok(row) = serde_json::from_slice::<Value>(&buf) else {
            return Ok(None);
        };
        let parent = entry.parent.expect("descendant of input");
        if row["id"].as_str().and_then(|id| ids.get(id)) != Some(&child)
            || row["parentId"].as_str().and_then(|id| ids.get(id)) != Some(&parent)
            || entries[parent].children != 1
        {
            return Ok(None);
        }
        if row["type"] == "message" && row["message"]["role"] == "user" {
            next_user = true;
            break;
        }
        // Later configuration or context changes may have happened after the
        // host stopped. Only the assistant's own terminal entry proves a cut.
        if row["type"] == "message" && row["message"]["role"] == "assistant" {
            anchor = row["id"].as_str().map(str::to_owned);
        }
        match row["type"].as_str() {
            // Pi records a context omission when retrying a failed assistant
            // attempt. Its streamed timeline remains visible, but the omitted
            // failure cannot by itself prove the outcome of the logical turn.
            Some("context_edit")
                if row.get("replacement") == Some(&Value::Null)
                    && failed_entry.is_some_and(|i| {
                        row["targetId"].as_str().and_then(|id| ids.get(id)) == Some(&i)
                    })
                    && turn.outstanding.is_empty() =>
            {
                failed_entry = None;
                turn.last_assistant = None;
            }
            Some("message") => {
                if turn.message(&row["message"]).is_none() {
                    return Ok(None);
                }
                failed_entry = (row["message"]["role"] == "assistant"
                    && row["message"]["stopReason"] == "error")
                    .then_some(child);
            }
            Some(
                "usage"
                | "thinking_level_change"
                | "model_change"
                | "session_info"
                | "label"
                | "custom",
            ) => {}
            // A summary or extension-generated conversation boundary can change
            // what constituted a turn. Do not attribute its successor to this input.
            _ => return Ok(None),
        }
    }
    let after = reader.get_ref().metadata()?;
    if cut != after.len() || before.modified()? != after.modified()? {
        return Ok(None);
    }
    Ok(turn.finish(next_user).map(|mut edits| {
        if let Some(anchor) = anchor
            && matches!(
                edits.last(),
                Some(ChatEdit::Activity {
                    event: Event::TurnEnded {
                        outcome: Outcome::Completed
                    }
                })
            )
        {
            edits.insert(
                edits.len() - 1,
                ChatEdit::Activity {
                    event: Event::ForkAnchor { anchor },
                },
            );
        }
        snapshot_tools(&mut edits, false);
        (id, edits)
    }))
}

pub(crate) fn saved_anchor(
    local: &crate::chat::ChatTurn,
    root: &str,
    options: &crate::SessionOptions,
) -> Result<Option<String>> {
    let (Some(timestamp), Some(prompt)) = (local.pi_input_timestamp_ms, local.prompt()) else {
        return Ok(None);
    };
    let path = crate::options::pi_file(&options.cwd, options, root)?;
    let Some((_, edits)) = recover(&path, root, &options.cwd, timestamp, prompt)? else {
        return Ok(None);
    };
    let mut raw = serde_json::to_value(edits)?;
    crate::transport::redact(&mut raw, &crate::options::session_secrets(options));
    let edits = serde_json::from_value::<Vec<ChatEdit>>(raw)?;
    if local.transcript_adds_content(root, &edits).is_none() {
        return Ok(None);
    }
    Ok(edits.into_iter().find_map(|e| match e {
        ChatEdit::Activity {
            event: Event::ForkAnchor { anchor },
        } => Some(anchor),
        _ => None,
    }))
}

#[derive(Default)]
struct Turn {
    edits: Vec<ChatEdit>,
    ordinal: u64,
    calls: HashSet<String>,
    outstanding: HashMap<String, String>,
    last_assistant: Option<Value>,
}
impl Turn {
    fn activity(&mut self, packet: Value) {
        self.edits.extend(
            crate::wire::project(Protocol::Pi, &packet, false)
                .into_iter()
                .map(|event| ChatEdit::Activity { event }),
        );
    }
    fn message(&mut self, m: &Value) -> Option<()> {
        match m["role"].as_str()? {
            "assistant" => {
                if !self.outstanding.is_empty()
                    || self
                        .last_assistant
                        .as_ref()
                        .is_some_and(|last| last["stopReason"] != "error")
                    || !matches!(
                        m["stopReason"].as_str(),
                        Some("stop" | "length" | "aborted" | "error" | "toolUse")
                    )
                {
                    return None;
                }
                self.ordinal += 1;
                let key = pi_reply_key(self.ordinal);
                let mut tools = Vec::new();
                for (i, b) in m["content"].as_array()?.iter().enumerate() {
                    let (role, text) = match b["type"].as_str()? {
                        "text" => (Role::Assistant, b["text"].as_str()?),
                        "thinking" => (Role::Reasoning, b["thinking"].as_str()?),
                        "toolCall" => {
                            let id = b["id"].as_str().filter(|s| !s.is_empty())?;
                            let name = b["name"].as_str().filter(|s| !s.is_empty())?;
                            if !self.calls.insert(id.into()) || !b["arguments"].is_object() {
                                return None;
                            }
                            self.outstanding.insert(id.into(), name.into());
                            tools.push(json!({"type":"tool_execution_start","toolCallId":id,"toolName":name,"args":b["arguments"]}));
                            continue;
                        }
                        _ => return None,
                    };
                    self.edits.push(ChatEdit::Text {
                        role,
                        text: text.into(),
                        key: Some(format!("{key}#{i}")),
                        final_text: true,
                    });
                }
                for tool in tools {
                    self.activity(tool)
                }
                self.last_assistant = Some(m.clone());
            }
            "toolResult" => {
                let id = m["toolCallId"].as_str()?;
                let name = self.outstanding.remove(id)?;
                if m["toolName"] != name || !m["content"].is_array() || !m["isError"].is_boolean() {
                    return None;
                }
                self.activity(json!({"type":"tool_execution_end","toolCallId":id,"toolName":name,"result":{"content":m["content"]},"isError":m["isError"]}));
                self.last_assistant = None;
            }
            _ => return None,
        }
        Some(())
    }
    fn finish(mut self, next_user: bool) -> Option<Vec<ChatEdit>> {
        if !self.outstanding.is_empty() {
            return None;
        }
        let last = self.last_assistant.take()?;
        let outcome = match last["stopReason"].as_str()? {
            "stop" | "length" => Outcome::Completed,
            "aborted" => Outcome::Aborted,
            // Pi can retry an error after persisting the assistant attempt.
            "error" if next_user => {
                self.activity(json!({"type":"agent_end","messages":[last]}));
                return Some(self.edits);
            }
            _ => return None,
        };
        self.edits.push(ChatEdit::Activity {
            event: Event::TurnEnded { outcome },
        });
        Some(self.edits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn row(id: &str, parent: Option<&str>, message: Value) -> Value {
        json!({"type":"message","id":id,"parentId":parent,"message":message})
    }

    fn user(timestamp: u64) -> Value {
        json!({"role":"user","timestamp":timestamp,"content":[{"type":"text","text":"same prompt"}]})
    }

    fn assistant(reason: &str) -> Value {
        json!({"role":"assistant","stopReason":reason,"content":[{"type":"text","text":"answer"}]})
    }

    fn transcript(rows: &[Value], tail: &[u8]) -> Option<Vec<ChatEdit>> {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let path = cwd.join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"session","version":3,"id":"root","cwd":cwd})
        )
        .unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
        file.write_all(tail).unwrap();
        recover(&path, "root", &cwd, 1, "same prompt")
            .unwrap()
            .map(|(_, edits)| edits)
    }

    #[test]
    fn requires_unique_input_on_an_unambiguous_active_branch() {
        let u = row("u", None, user(1));
        let end = row("a", Some("u"), assistant("stop"));
        let clean = [u.clone(), end.clone()];
        assert!(transcript(&clean, b"").is_some());
        assert!(transcript(&clean, br#"{"type":"message""#).is_none());
        for extra in [
            row("other", Some("a"), user(1)),
            row("other", None, user(2)),
            row("other", Some("u"), assistant("stop")),
            row("other", Some("a"), assistant("stop")),
            row("other", Some("missing"), user(2)),
            row("a", Some("a"), user(2)),
        ] {
            assert!(
                transcript(&[u.clone(), end.clone(), extra.clone()], b"").is_none(),
                "{extra}"
            );
        }
        let mut wrong = u;
        wrong["message"]["content"] = json!("different");
        assert!(transcript(&[wrong, end], b"").is_none());
    }

    #[test]
    fn requires_matching_tool_results_and_a_terminal_assistant() {
        let u = row("u", None, user(1));
        let call = row(
            "c",
            Some("u"),
            json!({"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"t","name":"read","arguments":{}}]}),
        );
        let result = row(
            "r",
            Some("c"),
            json!({"role":"toolResult","toolCallId":"t","toolName":"read","content":[],"isError":false}),
        );
        let end = row("a", Some("r"), assistant("length"));
        let mut rows = vec![u, call, result, end];
        let edits = transcript(&rows, b"").unwrap();
        let cards: Vec<_> = edits
            .iter()
            .filter_map(|edit| match edit {
                ChatEdit::ToolSnapshot { id, activity } => Some((id, activity)),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].0, "t");
        assert!(cards[0].1.finished && cards[0].1.result.is_some());
        // A pre-snapshot journal encoded the same verified call as individual events.
        let mut old = Turn::default();
        old.activity(
            json!({"type":"tool_execution_start","toolCallId":"t","toolName":"read","args":{}}),
        );
        old.activity(json!({"type":"tool_execution_end","toolCallId":"t","toolName":"read","result":{"content":[]},"isError":false}));
        snapshot_tools(&mut old.edits, false);
        let [ChatEdit::ToolSnapshot { id, activity }] = old.edits.as_slice() else {
            panic!("expected one recovered tool")
        };
        assert_eq!((id, activity), cards[0]);
        assert!(transcript(&rows[..2], b"").is_none());
        assert!(transcript(&rows[..3], b"").is_none());
        rows[2]["message"]["toolCallId"] = json!("another");
        assert!(transcript(&rows, b"").is_none());
        rows[2]["message"]["toolCallId"] = json!("t");
        rows[2]["message"]["toolName"] = json!("write");
        assert!(transcript(&rows, b"").is_none());
    }

    #[test]
    fn a_failed_attempt_can_retry_or_end_before_the_next_user() {
        let u = row("u", None, user(1));
        let error = row("e", Some("u"), assistant("error"));
        let rows = [u.clone(), error.clone()];
        assert!(transcript(&rows, b"").is_none());
        let omission = json!({"type":"context_edit","id":"omit","parentId":"e","targetId":"e","replacement":null});
        let mut retried = vec![
            u.clone(),
            error.clone(),
            omission,
            row("next", Some("omit"), user(2)),
        ];
        assert!(transcript(&retried, b"").is_none());
        retried[3]["message"] = assistant("stop");
        assert!(transcript(&retried, b"").is_some());
        retried[2]["targetId"] = json!("u");
        assert!(transcript(&retried, b"").is_none());
        for (next, outcome) in [
            (user(2), Outcome::Failed),
            (assistant("stop"), Outcome::Completed),
        ] {
            let edits = transcript(
                &[u.clone(), error.clone(), row("next", Some("e"), next)],
                b"",
            )
            .unwrap();
            assert!(
                matches!(edits.last(), Some(ChatEdit::Activity { event: Event::TurnEnded { outcome: observed } }) if *observed == outcome)
            );
        }
        let abort = row(
            "a",
            Some("u"),
            json!({"role":"assistant","stopReason":"aborted","content":[]}),
        );
        assert!(matches!(
            transcript(&[u, abort], b"").unwrap().last(),
            Some(ChatEdit::Activity {
                event: Event::TurnEnded {
                    outcome: Outcome::Aborted
                }
            })
        ));
    }

    #[test]
    fn recovered_reply_must_extend_the_recorded_stream() {
        use crate::chat::{ChatTurn, Item, TimelineItem};
        let mut local = ChatTurn::new(uuid::Uuid::new_v4(), 0, "root", Some("same prompt".into()));
        let key = format!("{}#0", pi_reply_key(1));
        local.items.push(TimelineItem {
            source: crate::observe::Agent {
                session_id: Some("root".into()),
                path: vec![],
            },
            item: Item::Message {
                role: Role::Assistant,
                text: "ans".into(),
                provider_id: Some(key),
                complete: false,
            },
        });
        let history = transcript(
            &[
                row("u", None, user(1)),
                row("a", Some("u"), assistant("stop")),
            ],
            b"",
        )
        .unwrap();
        assert!(local.transcript_adds_content("root", &history).is_some());
        let Item::Message { complete, .. } = &mut local.items.last_mut().unwrap().item else {
            unreachable!()
        };
        *complete = true;
        assert!(local.transcript_adds_content("root", &history).is_none());
        let Item::Message { text, complete, .. } = &mut local.items.last_mut().unwrap().item else {
            unreachable!()
        };
        *complete = false;
        *text = "other".into();
        assert!(local.transcript_adds_content("root", &history).is_none());
    }
}
