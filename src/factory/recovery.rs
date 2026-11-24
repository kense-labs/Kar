//! Restore one identified turn from Droid's local, append-only session transcript.
use super::projection::{Projection, terminal};
use crate::{
    Error, Result, SessionOptions,
    chat::{ChatEdit, ChatFrame, ChatTurn, Role},
    stream::Journal,
};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

pub(crate) fn directory(options: &SessionOptions, cwd: &Path) -> Option<PathBuf> {
    let home = crate::options::env_path(options, "FACTORY_HOME_OVERRIDE").or_else(|| {
        crate::options::env_path(options, if cfg!(windows) { "USERPROFILE" } else { "HOME" })
    })?;
    Some(cwd.join(home).join(".factory/sessions"))
}

fn header_matches(reader: &mut impl BufRead, session: &str, cwd: &Path) -> Result<bool> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let Ok(header) = serde_json::from_str::<Value>(&line) else {
        return Ok(false);
    };
    Ok(header["type"] == "session_start"
        && header["id"] == session
        && header["cwd"]
            .as_str()
            .and_then(|s| std::fs::canonicalize(s).ok())
            .is_some_and(|p| p == cwd))
}

fn locate(dir: &Path, session: &str, cwd: &Path) -> Result<Option<PathBuf>> {
    // Only the exact UUID filename is opened, regardless of the platform's workspace encoding.
    let Ok(uuid) = uuid::Uuid::parse_str(session) else {
        return Ok(None);
    };
    let mut found = None;
    for path in crate::native_file::project_transcripts(dir, uuid)? {
        if header_matches(
            &mut BufReader::new(crate::native_file::open_regular(&path)?),
            session,
            cwd,
        )? {
            if found.is_some() {
                return Err(Error::Protocol(
                    "ambiguous Factory transcript for this session and workspace".into(),
                ));
            }
            found = Some(path);
        }
    }
    Ok(found)
}

pub(crate) async fn file(dir: PathBuf, session: String, cwd: PathBuf) -> Result<Option<PathBuf>> {
    tokio::task::spawn_blocking(move || locate(&dir, &session, &cwd))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
}

fn message(row: &Value, edits: &mut Vec<ChatEdit>, projection: &mut Projection) -> Option<()> {
    let id = row["id"].as_str()?;
    let msg = &row["message"];
    let mut events = Vec::new();
    let blocks = msg["content"].as_array()?;
    if msg["role"] == "user" && msg.get("visibility").is_none() {
        let text = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<String>();
        if !text.is_empty() {
            edits.push(ChatEdit::UserInput {
                id: id.into(),
                text,
            });
        }
    }
    for (index, b) in blocks.iter().enumerate() {
        let text = match (msg["role"].as_str(), b["type"].as_str()) {
            (Some("assistant"), Some("text")) => Some((Role::Assistant, b["text"].as_str()?)),
            (Some("assistant"), Some("thinking")) => {
                b["thinking"].as_str().map(|s| (Role::Reasoning, s))
            }
            (Some("assistant"), Some("tool_use")) => {
                projection.tool(b, false, &mut events);
                None
            }
            (Some("user"), Some("tool_result")) => {
                projection.result(
                    &b["tool_use_id"],
                    &b["content"],
                    &b["is_error"],
                    &mut events,
                );
                None
            }
            _ => None,
        };
        if let Some((role, text)) = text {
            edits.push(ChatEdit::Text {
                role,
                text: text.into(),
                key: Some(format!("{id}#{index}")),
                final_text: true,
            });
        }
        edits.extend(events.drain(..).map(|event| ChatEdit::Activity { event }));
    }
    Some(())
}

enum Transcript {
    Open(Vec<ChatEdit>),
    Settled(Vec<ChatEdit>),
}

fn read_turn(
    path: &Path,
    session: &str,
    cwd: &Path,
    turn: &str,
    prompt: &str,
) -> Result<Option<Transcript>> {
    let file = File::open(path)?;
    let cut = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(cut));
    if !header_matches(&mut reader, session, cwd)? {
        return Ok(None);
    }
    let mut previous: Option<(String, u64)> = None;
    let mut edits = Vec::new();
    let mut projection = Projection::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 || !line.ends_with(b"\n") {
            // Only complete, chained rows before the next visible user input
            // are attributable without a matching turn outcome.
            if let Some(next) = edits
                .iter()
                .position(|edit| matches!(edit, ChatEdit::UserInput { .. }))
            {
                edits.truncate(next);
            }
            return Ok(previous.is_some().then_some(Transcript::Open(edits)));
        }
        let Ok(row) = serde_json::from_slice::<Value>(&line) else {
            return Ok(None);
        };
        if row["type"] == "agent_turn_outcome" && previous.is_some() {
            let Some(reason) = row["reason"].as_str().filter(|s| !s.is_empty()) else {
                return Ok(None);
            };
            if row["turnId"] != turn {
                return Ok(None);
            }
            let mut events = Vec::new();
            terminal(reason, None, &mut events);
            edits.extend(events.into_iter().map(|event| ChatEdit::Activity { event }));
            return Ok(Some(Transcript::Settled(edits)));
        }
        if row["type"] != "message" {
            continue;
        }
        let msg = &row["message"];
        if previous.is_none() && row["id"] != turn {
            continue;
        }
        let (Some(id), Some(seq)) = (row["id"].as_str(), row["seq"].as_u64()) else {
            return Ok(None);
        };
        if let Some((prev, prev_seq)) = &previous {
            if row["parentId"] != *prev || prev_seq.checked_add(1) != Some(seq) || id == turn {
                return Ok(None);
            }
            if message(&row, &mut edits, &mut projection).is_none() {
                return Ok(None);
            }
        } else {
            let Some(blocks) = msg["content"].as_array() else {
                return Ok(None);
            };
            if msg["role"] != "user"
                || msg.get("visibility").is_some()
                || blocks.is_empty()
                || !blocks
                    .iter()
                    .all(|b| b["type"] == "text" && b["text"].is_string())
                || blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<String>()
                    != prompt
            {
                return Ok(None);
            }
        }
        previous = Some((id.into(), seq));
    }
}

pub(crate) async fn recover(
    journal: &Journal,
    root: &str,
    cwd: &Path,
    options: &SessionOptions,
    local: &ChatTurn,
    prompt: &str,
) -> Result<Option<ChatFrame>> {
    let turn = local.id.to_string();
    if local.native_continuation || local.native_turn_id.as_ref().is_some_and(|id| *id != turn) {
        return Ok(None);
    }
    let Some(dir) = directory(options, cwd) else {
        return Ok(None);
    };
    let (root_id, workspace, request, input) = (
        root.to_owned(),
        cwd.to_owned(),
        turn.clone(),
        prompt.to_owned(),
    );
    let captured = tokio::task::spawn_blocking(move || {
        let Some(path) = locate(&dir, &root_id, &workspace)? else {
            return Ok(None);
        };
        read_turn(&path, &root_id, &workspace, &request, &input)
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    let Some(transcript) = captured else {
        return Ok(None);
    };
    let (edits, settled) = match transcript {
        Transcript::Open(edits) => (edits, false),
        Transcript::Settled(edits) => (edits, true),
    };
    let missing_receipt = !local.input_sent || local.native_turn_id.is_none();
    let frame = |edits, accept: &dyn Fn(&[ChatEdit]) -> bool| {
        ChatFrame::transcript(
            journal,
            root,
            turn.clone(),
            "factory/transcript",
            edits,
            options,
            accept,
        )
    };
    let content = frame(edits, &|edits| {
        settled || local.transcript_adds_content(root, edits) == Some(true)
    })?;
    if content.is_none() && missing_receipt {
        // The opener proves delivery even if the local stream contains later
        // or unidentifiable content that a partial transcript cannot replace.
        return frame(Vec::new(), &|_| true);
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn distinguish_a_matching_native_opener_from_a_completed_turn() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let root = uuid::Uuid::new_v4().to_string();
        let turn = uuid::Uuid::new_v4().to_string();
        let file = dir.path().join("fixture.jsonl");
        let header = json!({"type":"session_start","cwd":cwd,"id":root});
        let input = json!({"type":"message","seq":10,"id":turn,"message":{"role":"user","content":[{"type":"text","text":"hello"}]}});
        let reply = json!({"type":"message","seq":11,"parentId":turn,"id":"reply","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}});
        let end = json!({"type":"agent_turn_outcome","turnId":turn,"reason":"completed"});
        let rows = [header, input, reply, end];
        let read = |rows: &[Value], session: &str, prompt: &str| {
            let mut f = File::create(&file).unwrap();
            for row in rows {
                writeln!(f, "{row}").unwrap();
            }
            read_turn(&file, session, &cwd, &turn, prompt).unwrap()
        };
        assert!(matches!(
            read(&rows, &root, "hello"),
            Some(Transcript::Settled(_))
        ));
        assert!(matches!(
            read(&rows[..3], &root, "hello"),
            Some(Transcript::Open(ref edits)) if matches!(edits.as_slice(), [ChatEdit::Text { text, .. }] if text == "answer")
        ));
        assert!(matches!(
            read(&rows[..2], &root, "hello"),
            Some(Transcript::Open(ref edits)) if edits.is_empty()
        ));
        assert!(read(&rows[..1], &root, "hello").is_none());
        let mut multiple = rows[..3].to_vec();
        multiple.push(
            json!({"type":"message","id":"later-input","seq":12,"parentId":"reply",
            "message":{"role":"user","content":[{"type":"text","text":"another prompt"}]}}),
        );
        multiple.push(
            json!({"type":"message","id":"later-answer","seq":13,"parentId":"later-input",
            "message":{"role":"assistant","content":[{"type":"text","text":"another answer"}]}}),
        );
        assert!(
            matches!(read(&multiple, &root, "hello"), Some(Transcript::Open(ref edits))
            if matches!(edits.as_slice(), [ChatEdit::Text {text, ..}] if text == "answer"))
        );
        let mut partial = rows[..2].to_vec();
        partial[1]["id"] = json!(uuid::Uuid::new_v4());
        assert!(read(&partial, &root, "hello").is_none());
        partial[1] = rows[1].clone();
        partial[1]["message"]["visibility"] = json!("hidden");
        assert!(read(&partial, &root, "hello").is_none());
        partial[1] = rows[1].clone();
        partial[1]["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"tool_result","content":"other"}));
        assert!(read(&partial, &root, "hello").is_none());
        assert!(matches!(
            read(&rows[..2], &root, "hello"),
            Some(Transcript::Open(ref edits)) if edits.is_empty()
        ));
        // Even an interrupted UTF-8 code point in the next row cannot erase
        // the already persisted input or provide a terminal outcome.
        File::options()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"{\"text\":\"\xe4")
            .unwrap();
        assert!(matches!(
            read_turn(&file, &root, &cwd, &turn, "hello").unwrap(),
            Some(Transcript::Open(ref edits)) if edits.is_empty()
        ));
        assert!(read(&rows, &root, "different").is_none());
        assert!(read(&rows, "unrelated", "hello").is_none());
        for (index, key, value) in [
            (2, "parentId", json!("other")),
            (2, "seq", json!(12)),
            (3, "turnId", json!("other")),
        ] {
            let mut broken = rows.clone();
            broken[index][key] = value;
            assert!(read(&broken, &root, "hello").is_none());
        }
        assert!(
            read_turn(&file, &root, &cwd.join("other-workspace"), &turn, "hello")
                .unwrap()
                .is_none()
        );
    }
}
