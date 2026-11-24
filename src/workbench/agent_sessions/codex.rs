use super::discover::{MIB, bounded_file};
use sqlite_core::{Database, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

// Read only a consistent snapshot of committed, live tables, including Codex's WAL.
// Corrupt, changing or oversized optional indexes leave the original rollout cwd intact.
pub(super) fn project_roots(home: &Path) -> Option<HashMap<String, PathBuf>> {
    let bytes = bounded_file(&home.join("state_5.sqlite"), 128 * MIB)?;
    let wal = home.join("state_5.sqlite-wal");
    let db = if wal.try_exists().ok()? {
        Database::open_with_wal(bytes, &bounded_file(&wal, 64 * MIB)?).ok()?
    } else {
        Database::open(bytes).ok()?
    };
    let tables = db.live_tables();
    let rows = |name: &str, cols: &[&str]| {
        let table = tables.iter().find(|t| t.name == name)?;
        let names = table.column_names.as_ref()?;
        let indices = cols
            .iter()
            .map(|c| names.iter().position(|s| s == c))
            .collect::<Option<Vec<_>>>()?;
        let rows = db.read_table(table.rootpage, names.len()).ok()?;
        Some((rows, indices))
    };
    fn key(v: &Value) -> Option<String> {
        match v {
            Value::Text(s) => Some(s.clone()),
            Value::Integer(n) => Some(n.to_string()),
            _ => None,
        }
    }
    let (projects, cols) = rows("project_roots", &["project_id", "path", "position"])?;
    let mut first = HashMap::new();
    for row in projects {
        let (Some(id), Some(Value::Text(path)), Some(Value::Integer(pos))) = (
            row.values.get(cols[0]).and_then(key),
            row.values.get(cols[1]),
            row.values.get(cols[2]),
        ) else {
            continue;
        };
        let path = Path::new(path);
        if path.is_absolute() && first.get(&id).is_none_or(|(p, _)| p > pos) {
            first.insert(id, (*pos, path.to_path_buf()));
        }
    }
    let (threads, cols) = rows("threads", &["id", "project_id"])?;
    let mut roots = HashMap::new();
    for row in threads.iter().rev().take(100_000) {
        let (Some(Value::Text(id)), Some(proj)) = (
            row.values.get(cols[0]),
            row.values.get(cols[1]).and_then(key),
        ) else {
            continue;
        };
        if let Some((_, path)) = first.get(&proj) {
            roots.insert(id.clone(), path.clone());
        }
    }
    Some(roots)
}

pub(super) fn titles(home: &Path) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    if let Some(data) = bounded_file(&home.join("session_index.jsonl"), MIB) {
        for line in data.split(|c| *c == b'\n') {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line)
                && let (Some(id), Some(title)) = (
                    v["id"].as_str(),
                    v["thread_name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty()),
                )
            {
                titles.insert(id.into(), title.into());
            }
        }
    }
    titles
}
