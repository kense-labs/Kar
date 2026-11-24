//! Owner-initiated reports and cleanup of Kar's saved data.
use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{Error, Result, workbench_git::Repository};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::Path,
    time::{Duration, SystemTime},
};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Retention {
    days: u32,
    worktree_ids: Vec<Uuid>,
}
impl Default for Retention {
    fn default() -> Self {
        Self {
            days: 30,
            worktree_ids: vec![],
        }
    }
}
fn bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![path.to_owned()];
    while let Some(path) = pending.pop() {
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            for e in fs::read_dir(path)? {
                pending.push(e?.path());
            }
        }
    }
    Ok(total)
}
fn report(app: &Workbench, q: Retention, apply: bool) -> Result<Value> {
    let threshold = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(q.days) * 86_400))
        .ok_or_else(|| Error::Refused("retention exceeds host clock range".into()))?;
    let root = app.0.chats.directory();
    let before = bytes(root)?;
    let operation =
        |db: &mut super::Catalog| super::assets::expired(app, db, super::access::now(), apply);
    let (uploads, upload_bytes) = if apply {
        app.update(operation)?
    } else {
        operation(&mut app.0.catalog.lock().unwrap().clone())?
    };
    let (logs, log_bytes) = app.0.observability.traces.rotated(threshold, apply)?;
    let projects = app.projects();
    let chats = app.0.chats.list()?;
    let bound = |dir: &Path| {
        projects.iter().any(|p| p.root.starts_with(dir))
            || chats.iter().any(|c| c.workspace.starts_with(dir))
    };
    let selected: HashSet<_> = q.worktree_ids.into_iter().collect();
    let mut visited = HashSet::new();
    let mut worktrees = vec![];
    for p in &projects {
        let Ok(repo) = Repository::open(&p.root) else {
            continue;
        };
        for (tree, updated) in repo.managed_candidates()? {
            if !visited.insert(tree.id) {
                continue;
            }
            let path = &tree.path;
            let guard = || -> Result<()> {
                app.workspace_at(path)?;
                if bound(path) {
                    return Err(Error::Refused(
                        "project or chat still uses this checkout".into(),
                    ));
                }
                if updated >= threshold {
                    return Err(Error::Refused("worktree is within retention".into()));
                }
                let state = Repository::open(path)?.status()?;
                if !state.files.is_empty() {
                    return Err(Error::Refused("worktree has local changes".into()));
                }
                if state.branch.is_none() {
                    return Err(Error::Refused("worktree HEAD is detached".into()));
                }
                Ok(())
            };
            let size = if app.workspace_at(path).is_ok() {
                bytes(path)?
            } else {
                0
            };
            let result = guard().and_then(|()| {
                if apply && selected.contains(&tree.id) {
                    repo.remove_worktree_if(tree.id, |actual| {
                        if actual != path
                            || app
                                .0
                                .chats
                                .list()?
                                .iter()
                                .any(|c| c.workspace.starts_with(actual))
                            || app.projects().iter().any(|p| p.root.starts_with(actual))
                        {
                            return Err(Error::Refused("worktree ownership changed".into()));
                        }
                        Ok(())
                    })?;
                    Ok("removed")
                } else {
                    Ok("eligible")
                }
            });
            let (outcome, reason) = match result {
                Ok(s) => (s, None),
                Err(e) => ("kept", Some(e.to_string())),
            };
            worktrees.push(json!({"id":tree.id,"project_id":p.id,"path":path,"bytes":size,"outcome":outcome,"reason":reason}));
        }
    }
    for id in selected.difference(&visited) {
        worktrees.push(json!({"id":id,"outcome":"kept","reason":"unknown managed checkout"}));
    }
    let after = if apply { bytes(root)? } else { before };
    Ok(
        json!({"applied":apply,"storage":{"format":"kar-jsonl","bytes_before":before,"bytes_after":after,"catalog_bytes":bytes(&app.0.state.join("projects.json"))?},"expired_uploads":{"entries":uploads,"bytes":upload_bytes},"rotated_logs":{"files":logs,"bytes":log_bytes},"worktrees":worktrees}),
    )
}
async fn inspect(
    State(app): State<Workbench>,
    Query(q): Query<Retention>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || report(&app, q, false)).await
}
async fn clean(
    State(app): State<Workbench>,
    Json(q): Json<Retention>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || report(&app, q, true)).await
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new().route("/storage", get(inspect).post(clean))
}
