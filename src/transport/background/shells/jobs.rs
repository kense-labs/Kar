//! OpenCode's persisted background jobs and synthetic completion receipts share the native session ID.
use crate::{TaskStatus, workbench::native_sqlite::store};
use serde_json::Value;
use std::{collections::BTreeMap, io, path::Path};

#[derive(Default)]
pub(super) struct Job {
    pub(super) command: Option<String>,
    pub(super) alias: Option<String>,
    pub(super) running: bool,
    pub(super) ended: Option<TaskStatus>,
    pub(super) killed_by_signal: bool,
}

pub(super) fn terminal(meta: &Value) -> Option<TaskStatus> {
    match meta["state"].as_str()? {
        "cancelled" => Some(TaskStatus::Stopped),
        "error" => Some(TaskStatus::Failed),
        "completed"
            if meta["timeout"].is_number()
                || meta["timeout"] == true
                || !meta["signal"].is_null()
                || meta["exit"].as_i64().is_some_and(|i| i != 0) =>
        {
            Some(TaskStatus::Failed)
        }
        "completed" if meta["killed"] == true => Some(TaskStatus::Stopped),
        "completed" => Some(TaskStatus::Completed),
        _ => None,
    }
}
pub(super) fn read(
    path: &Path,
    root: &str,
    watching: &[String],
) -> io::Result<BTreeMap<String, Job>> {
    let db = store::open(path)?;
    let mut jobs = BTreeMap::<String, Job>::new();
    db.records("job", |r| {
        if r.bounded_text("session_id", 512)?.as_deref() != Some(root) {
            return Ok(());
        }
        let recovery: Value = r
            .bounded_text("recovery", 1024 * 1024)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        if recovery["kind"] != "shell" || recovery["sessionID"] != root {
            return Ok(());
        }
        let Some(id) = recovery["shellID"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        let j = jobs.entry(id.into()).or_default();
        j.alias = r.bounded_text("id", 512)?;
        j.command = recovery["command"].as_str().map(str::to_owned);
        match r.bounded_text("status", 64)?.as_deref() {
            Some("running") => j.running = true,
            Some("cancelled") => j.ended = Some(TaskStatus::Stopped),
            Some("error") => j.ended = Some(TaskStatus::Failed),
            _ => {}
        }
        Ok(())
    })?;
    if watching.is_empty() && !jobs.values().any(|j| j.running) {
        return Ok(jobs);
    }
    for (table, col) in [("session_inbox", "payload"), ("session_message", "data")] {
        if !db.has_table(table) {
            continue;
        }
        db.records(table, |r| {
            if r.bounded_text("session_id", 512)?.as_deref() != Some(root)
                || r.bounded_text("type", 64)?.as_deref() != Some("synthetic")
            {
                return Ok(());
            }
            let Some(data) = r
                .bounded_text(col, 8 * 1024 * 1024)?
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            else {
                return Ok(());
            };
            let meta = &data["metadata"];
            if meta["source"] != "shell" {
                return Ok(());
            }
            let Some(id) = meta["shellID"].as_str() else {
                return Ok(());
            };
            if !jobs.contains_key(id) && !watching.iter().any(|s| s == id) {
                return Ok(());
            }
            let j = jobs.entry(id.into()).or_default();
            if let Some(alias) = meta["jobID"].as_str() {
                if j.alias.as_deref().is_some_and(|known| known != alias) {
                    return Ok(());
                }
                j.alias = Some(alias.into());
            }
            if let Some(end) = terminal(meta) {
                j.ended = Some(end);
                j.killed_by_signal = matches!(
                    meta["signal"].as_str(),
                    Some("SIGTERM" | "SIGKILL" | "TERM" | "KILL")
                ) || meta["killed"] == true
                    || matches!(meta["exit"].as_i64(), Some(137 | 143));
            }
            Ok(())
        })?;
    }
    Ok(jobs)
}
