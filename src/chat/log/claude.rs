//! Reconcile host inputs with Claude's persistent root conversation.
mod blocks;
use crate::{
    Error, Result, SessionOptions,
    chat::{ChatEdit, ChatInfo, ChatTurn, TurnStatus},
    native_file,
    options::env_path,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader, Read},
    path::Path,
};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct History {
    pub received: Vec<Uuid>,
    pub edits: Vec<ChatEdit>,
}

pub(super) async fn history(
    chat: &ChatInfo,
    options: &SessionOptions,
    turns: &[ChatTurn],
) -> Result<History> {
    let mut inputs: HashMap<_, _> = turns
        .iter()
        .filter(|t| {
            !t.input_sent
                && !t.native_continuation
                && t.native_turn_id.is_none()
                && t.status != TurnStatus::Completed
        })
        .filter_map(|t| Some((t.id, t.prompt()?.to_owned())))
        .collect();
    let recovery = turns
        .last()
        .filter(|t| {
            matches!(t.status, TurnStatus::Interrupted | TurnStatus::Running)
                && !t.native_continuation
                && t.steered_from.is_none()
                && t.steered_inputs.is_empty()
                && t.native_result_id.is_none()
                && t.native_input_receipt
                    .is_none_or(|r| r.dropped_outcome().is_none())
        })
        .and_then(|t| {
            inputs.insert(t.id, t.prompt()?.to_owned());
            Some(blocks::Transcript::new(t, &chat.native_id))
        });
    let Ok(session) = Uuid::parse_str(&chat.native_id) else {
        return Ok(History::default());
    };
    if inputs.is_empty() {
        return Ok(History::default());
    }
    let config = env_path(options, "CLAUDE_CONFIG_DIR")
        .or_else(|| options.session_home().map(|home| home.join(".claude")));
    let Some(config) = config else {
        return Ok(History::default());
    };
    let cwd = chat.workspace.clone();
    tokio::task::spawn_blocking(move || {
        let projects = cwd.join(config).join("projects");
        // An optional native transcript may have moved or be unreadable. Retain
        // the uncertain receipt in that case and let other queued inputs proceed.
        let Ok(files) = native_file::project_transcripts(&projects, session) else {
            return History::default();
        };
        let [path] = files.as_slice() else {
            return History::default();
        };
        read(path, &cwd, session, &inputs, recovery).unwrap_or_default()
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))
}

#[derive(Deserialize)]
struct Identity<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    uuid: Option<&'a str>,
}

fn read(
    path: &Path,
    cwd: &Path,
    session: Uuid,
    inputs: &HashMap<Uuid, String>,
    mut recovery: Option<blocks::Transcript>,
) -> io::Result<History> {
    let file = native_file::open_regular(path)?;
    let before = file.metadata()?;
    let mut reader = BufReader::new((&file).take(before.len()));
    let mut seen = HashSet::new();
    let mut confirmed = HashSet::new();
    let mut line = Vec::new();
    const MAX_ROW: u64 = 16 * 1024 * 1024;
    loop {
        line.clear();
        if (&mut reader)
            .take(MAX_ROW + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            break;
        }
        if line.last() != Some(&b'\n') || line.len() as u64 > MAX_ROW {
            return Ok(History::default());
        }
        // Skip request payloads, attachments and other messages without allocating
        // their contents. Only inspect the body for an exact candidate UUID.
        let Ok(row) = serde_json::from_slice::<Identity>(&line) else {
            return Ok(History::default());
        };
        let Some(id) = row.uuid.and_then(|s| Uuid::parse_str(s).ok()) else {
            if matches!(row.kind, Some("assistant" | "user"))
                && recovery.as_ref().is_some_and(|t| t.started())
            {
                recovery = None;
            }
            continue;
        };
        let prompt = inputs.get(&id);
        if prompt.is_none() && !recovery.as_ref().is_some_and(|t| t.started()) {
            continue;
        }
        let row: Value = serde_json::from_slice(&line)?;
        let same_root = || {
            row["isSidechain"] == false
                && row["parent_tool_use_id"].is_null()
                && row["sessionId"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    == Some(session)
                && row["cwd"]
                    .as_str()
                    .and_then(|s| Path::new(s).canonicalize().ok())
                    .as_deref()
                    == Some(cwd)
        };
        if let Some(prompt) = prompt {
            let first = seen.insert(id);
            let matches = first
                && row["type"] == "user"
                && row["message"]["role"] == "user"
                && row["promptSource"] == "sdk"
                && row["isMeta"] != true
                && row["message"]["content"].as_str() == Some(prompt.as_str())
                && same_root();
            if matches {
                confirmed.insert(id);
            } else {
                confirmed.remove(&id);
            }
            if let Some(t) = &mut recovery
                && id == t.input
            {
                if matches {
                    t.start();
                } else {
                    recovery = None;
                }
                continue;
            }
        }
        if let Some(t) = &mut recovery
            && t.started()
            && (!same_root() || t.push(id, &row).is_none())
        {
            recovery = None;
        }
    }
    let current = std::fs::symlink_metadata(path)?;
    if !current.is_file()
        || before.len() != current.len()
        || before.modified()? != current.modified()?
    {
        return Ok(History::default());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != current.dev() || before.ino() != current.ino() {
            return Ok(History::default());
        }
    }
    let mut ids: Vec<_> = confirmed.into_iter().collect();
    ids.sort_unstable();
    Ok(History {
        received: ids,
        edits: recovery.map_or_else(Vec::new, |t| t.finish()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transcript_requires_a_single_root_chain_and_stays_with_its_input() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        let path = cwd.join("session.jsonl");
        let (session, input, reply, next) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let root = session.to_string();
        let local = ChatTurn::new(input, 1, &root, Some("same".into()));
        let first = json!({"uuid":input,"parentUuid":null,"type":"user","sessionId":session,"cwd":cwd,
            "promptSource":"sdk","isSidechain":false,"message":{"role":"user","content":"same"}});
        let answer = json!({"uuid":reply,"parentUuid":input,"type":"assistant","sessionId":session,"cwd":cwd,
            "isSidechain":false,"apiBlockIndex":2,"message":{"role":"assistant","model":"fixture","id":"msg","stop_reason":"end_turn",
                "content":[{"type":"text","text":"visible"}]}});
        let check = |rows: &[Value], tail: &str| {
            let data = rows.iter().map(|r| format!("{r}\n")).collect::<String>() + tail;
            std::fs::write(&path, data).unwrap();
            read(
                &path,
                &cwd,
                session,
                &HashMap::from([(input, "same".into())]),
                Some(blocks::Transcript::new(&local, &root)),
            )
            .unwrap()
            .edits
        };
        let visible = check(&[first.clone(), answer.clone()], "");
        assert!(
            matches!(&visible[..],[ChatEdit::Text {key:Some(key),text,final_text:true,..}] if key=="msg#2" && text=="visible")
        );
        assert!(check(&[first.clone(), answer.clone()], "{").is_empty());
        for (key, value) in [
            ("parentUuid", json!(next)),
            ("uuid", json!(input)),
            ("sessionId", json!(next)),
            ("cwd", json!(cwd.join("other"))),
            ("isSidechain", json!(true)),
            ("isMeta", json!(true)),
            ("parent_tool_use_id", json!("child")),
        ] {
            let mut bad = answer.clone();
            bad[key] = value;
            assert!(check(&[first.clone(), bad], "").is_empty(), "{key}");
        }
        let mut fork = answer.clone();
        fork["uuid"] = json!(next);
        assert!(check(&[first.clone(), answer.clone(), fork], "").is_empty());
        let mut other = first.clone();
        other["uuid"] = json!(next);
        other["parentUuid"] = json!(reply);
        let mut unrelated = answer.clone();
        unrelated["uuid"] = json!(Uuid::new_v4());
        unrelated["parentUuid"] = json!(next);
        unrelated["message"]["content"][0]["text"] = json!("belongs to another prompt");
        let chain = [
            first.clone(),
            answer.clone(),
            other.clone(),
            unrelated.clone(),
        ];
        assert_eq!(json!(check(&chain, "")), json!(visible));
        other["type"] = json!("attachment");
        other["attachment"] = json!({"type":"task_notification"});
        assert_eq!(
            json!(check(
                &[first.clone(), answer.clone(), other, unrelated],
                ""
            )),
            json!(visible)
        );
        let mut request = answer.clone();
        request["message"]["content"] =
            json!([{"type":"tool_use","id":"call","name":"Read","input":{"file":"one"}}]);
        let mut result = first.clone();
        result.as_object_mut().unwrap().remove("promptSource");
        result["uuid"] = json!(next);
        result["parentUuid"] = json!(reply);
        result["sourceToolAssistantUUID"] = json!(input);
        result["message"]["content"] = json!([{"type":"tool_result","tool_use_id":"call","content":"belongs to another call"}]);
        assert!(check(&[first, request, result], "").is_empty());
    }

    #[test]
    fn receipt_requires_unique_root_input_in_the_same_session_and_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        let path = cwd.join("session.jsonl");
        let (session, id, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let row = json!({"type":"user","message":{"role":"user","content":"same"},
            "uuid":id,"sessionId":session,"isSidechain":false,"promptSource":"sdk","cwd":cwd});
        let inputs = HashMap::from([(id, "same".into()), (other, "same".into())]);
        let check = |rows: &[Value], tail: &str| {
            let mut data = String::new();
            for row in rows {
                data.push_str(&row.to_string());
                data.push('\n');
            }
            data.push_str(tail);
            std::fs::write(&path, data).unwrap();
            read(&path, &cwd, session, &inputs, None).unwrap().received
        };
        assert_eq!(check(std::slice::from_ref(&row), ""), [id]);
        assert!(check(&[row.clone(), row.clone()], "").is_empty());
        assert!(check(std::slice::from_ref(&row), "{").is_empty());
        for (key, value) in [
            ("uuid", json!(Uuid::new_v4())),
            ("sessionId", json!(Uuid::new_v4())),
            ("cwd", json!(cwd.join("elsewhere"))),
            ("isSidechain", json!(true)),
            ("isMeta", json!(true)),
            ("type", json!("assistant")),
            ("promptSource", json!("repl")),
            ("parent_tool_use_id", json!("tool")),
            ("message", json!({"role":"user","content":"different"})),
        ] {
            let mut changed = row.clone();
            changed[key] = value;
            assert!(check(&[changed], "").is_empty(), "{key}");
        }
        let mut second = row.clone();
        second["uuid"] = json!(other);
        assert_eq!(check(&[row.clone(), second, row], ""), [other]);
    }
}
