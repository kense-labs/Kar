//! Recover a lost host-side send receipt from Claude's persisted SDK input UUID.
use crate::{
    Error, Result, SessionOptions,
    chat::{ChatInfo, ChatTurn, TurnStatus},
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

pub(super) async fn received(
    chat: &ChatInfo,
    options: &SessionOptions,
    turns: &[ChatTurn],
) -> Result<Vec<Uuid>> {
    let inputs: HashMap<_, _> = turns
        .iter()
        .filter(|t| {
            !t.input_sent
                && !t.native_continuation
                && t.native_turn_id.is_none()
                && t.status != TurnStatus::Completed
        })
        .filter_map(|t| Some((t.id, t.prompt()?.to_owned())))
        .collect();
    let Ok(session) = Uuid::parse_str(&chat.native_id) else {
        return Ok(vec![]);
    };
    if inputs.is_empty() {
        return Ok(vec![]);
    }
    let config = env_path(options, "CLAUDE_CONFIG_DIR")
        .or_else(|| options.session_home().map(|home| home.join(".claude")));
    let Some(config) = config else {
        return Ok(vec![]);
    };
    let cwd = chat.workspace.clone();
    tokio::task::spawn_blocking(move || {
        let projects = cwd.join(config).join("projects");
        // An optional native transcript may have moved or be unreadable. Retain
        // the uncertain receipt in that case and let other queued inputs proceed.
        let Ok(files) = native_file::project_transcripts(&projects, session) else {
            return vec![];
        };
        let [path] = files.as_slice() else {
            return vec![];
        };
        confirmed(path, &cwd, session, &inputs).unwrap_or_default()
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))
}

#[derive(Deserialize)]
struct Identity<'a> {
    uuid: Option<&'a str>,
}

fn confirmed(
    path: &Path,
    cwd: &Path,
    session: Uuid,
    inputs: &HashMap<Uuid, String>,
) -> io::Result<Vec<Uuid>> {
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
            return Ok(vec![]);
        }
        // Skip request payloads, attachments and other messages without allocating
        // their contents. Only inspect the body for an exact candidate UUID.
        let Ok(row) = serde_json::from_slice::<Identity>(&line) else {
            return Ok(vec![]);
        };
        let Some(id) = row.uuid.and_then(|s| Uuid::parse_str(s).ok()) else {
            continue;
        };
        let Some(prompt) = inputs.get(&id) else {
            continue;
        };
        if !seen.insert(id) {
            confirmed.remove(&id);
            continue;
        }
        let row: Value = serde_json::from_slice(&line)?;
        if row["type"] == "user"
            && row["message"]["role"] == "user"
            && row["promptSource"] == "sdk"
            && row["isSidechain"] == false
            && row["isMeta"] != true
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
            && row["message"]["content"].as_str() == Some(prompt.as_str())
        {
            confirmed.insert(id);
        }
    }
    let current = std::fs::symlink_metadata(path)?;
    if !current.is_file()
        || before.len() != current.len()
        || before.modified()? != current.modified()?
    {
        return Ok(vec![]);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != current.dev() || before.ino() != current.ino() {
            return Ok(vec![]);
        }
    }
    let mut ids: Vec<_> = confirmed.into_iter().collect();
    ids.sort_unstable();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            confirmed(&path, &cwd, session, &inputs).unwrap()
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
