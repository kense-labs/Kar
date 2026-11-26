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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::Path,
    time::{Duration, SystemTime},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::workbench) struct WorktreeCleanupRules {
    pub enabled: bool,
    #[serde(alias = "after_days")]
    pub after_days: Option<u32>,
    #[serde(alias = "settled_after_days")]
    pub settled_after_days: Option<u32>,
    #[serde(alias = "on_merge")]
    pub on_merge: bool,
    #[serde(alias = "on_delete")]
    pub on_delete: bool,
    pub unchanged: bool,
}

impl Default for WorktreeCleanupRules {
    fn default() -> Self {
        Self {
            enabled: false,
            after_days: Some(30),
            settled_after_days: Some(14),
            on_merge: true,
            on_delete: true,
            unchanged: true,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Retention {
    days: u32,
    worktree_ids: Vec<Uuid>,
    #[serde(alias = "applyPolicy")]
    apply_policy: bool,
    #[serde(alias = "compactWal")]
    compact_wal: bool,
    rules: Option<WorktreeCleanupRules>,
}
impl Default for Retention {
    fn default() -> Self {
        Self {
            days: 30,
            worktree_ids: vec![],
            apply_policy: false,
            compact_wal: false,
            rules: None,
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
    let (policy_rules, archived_chats, settled_chats) = {
        let db = app.0.catalog.lock().unwrap();
        let configured = db
            .server
            .value("worktreeCleanup")
            .and_then(|v| serde_json::from_value::<WorktreeCleanupRules>(v.clone()).ok())
            .unwrap_or_default();
        let archived = db.archived.clone();
        let settled: HashSet<Uuid> = db
            .thread_state
            .rows
            .iter()
            .filter_map(|(id, m)| m.settled_ms.map(|_| *id))
            .collect();
        (q.rules.clone().unwrap_or(configured), archived, settled)
    };
    let settled_threshold = policy_rules
        .settled_after_days
        .and_then(|d| SystemTime::now().checked_sub(Duration::from_secs(u64::from(d) * 86_400)));
    let has_live_controller = |dir: &Path| {
        chats
            .iter()
            .filter(|c| c.workspace.starts_with(dir))
            .any(|c| {
                app.0
                    .controllers
                    .lock()
                    .unwrap()
                    .get(&c.id)
                    .and_then(|slot| slot.owner.try_lock().ok().map(|g| g.is_some()))
                    .unwrap_or(true)
            })
    };
    let bound = |dir: &Path| {
        projects.iter().any(|p| p.root.starts_with(dir))
            || chats.iter().any(|c| c.workspace.starts_with(dir))
    };
    let policy_bound_eligible = |tree: &crate::workbench_git::ManagedWorktree,
                                 updated: SystemTime,
                                 repo: &Repository|
     -> bool {
        if !(q.apply_policy || policy_rules.enabled) {
            return false;
        }
        if projects.iter().any(|p| p.root.starts_with(&tree.path))
            || has_live_controller(&tree.path)
        {
            return false;
        }
        let owning: Vec<_> = chats
            .iter()
            .filter(|c| c.workspace.starts_with(&tree.path))
            .collect();
        if owning.is_empty() {
            return policy_rules.on_delete
                || policy_rules
                    .after_days
                    .and_then(|d| {
                        SystemTime::now().checked_sub(Duration::from_secs(u64::from(d) * 86_400))
                    })
                    .is_some_and(|cut| updated < cut);
        }
        let all_inactive = owning
            .iter()
            .all(|c| archived_chats.contains(&c.id) || settled_chats.contains(&c.id));
        if !all_inactive {
            return false;
        }
        if settled_threshold.is_some_and(|cut| updated < cut) {
            return true;
        }
        if policy_rules.unchanged
            && repo
                .resolve(&format!("refs/heads/{}", tree.branch))
                .is_ok_and(|tip| tip == tree.start_commit)
        {
            return true;
        }
        if policy_rules.on_merge
            && let Ok(head_oid) = repo.resolve("HEAD")
            && let Ok(branch_oid) = repo.resolve(&format!("refs/heads/{}", tree.branch))
            && branch_oid != tree.start_commit
            && crate::workbench_git::git(
                repo.root(),
                &["merge-base", "--is-ancestor", &branch_oid, &head_oid],
            )
            .is_ok()
        {
            return true;
        }
        false
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
            let by_policy = policy_bound_eligible(&tree, updated, &repo);
            let guard = || -> Result<()> {
                app.workspace_at(path)?;
                if !by_policy {
                    if bound(path) {
                        return Err(Error::Refused(
                            "project or chat still uses this checkout".into(),
                        ));
                    }
                    if updated >= threshold {
                        return Err(Error::Refused("worktree is within retention".into()));
                    }
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
                if apply && (selected.contains(&tree.id) || (q.apply_policy && by_policy)) {
                    repo.remove_worktree_if(tree.id, |actual| {
                        if actual != path
                            || app.projects().iter().any(|p| p.root.starts_with(actual))
                            || (!by_policy
                                && app
                                    .0
                                    .chats
                                    .list()?
                                    .iter()
                                    .any(|c| c.workspace.starts_with(actual)))
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
    let wal_reclaimed = if apply || q.compact_wal {
        let cat_bytes = {
            let db = app.0.catalog.lock().unwrap();
            app.0.catalog_wal.compact(&*db)?
        };
        let wf_bytes = app.0.workflows.compact_wal()?;
        cat_bytes.saturating_add(wf_bytes)
    } else {
        0
    };
    let catalog_wal_stats = app.0.catalog_wal.stats();
    let after = if apply { bytes(root)? } else { before };
    Ok(json!({
        "applied": apply,
        "storage": {
            "format": "kar-jsonl",
            "bytes_before": before,
            "bytes_after": after,
            "catalog_bytes": bytes(&app.0.state.join("projects.json"))?,
            "wal": {
                "catalog": catalog_wal_stats,
                "reclaimed_bytes": wal_reclaimed,
            }
        },
        "expired_uploads": {"entries": uploads, "bytes": upload_bytes},
        "rotated_logs": {"files": logs, "bytes": log_bytes},
        "worktrees": worktrees
    }))
}

pub(in crate::workbench) fn start_cleanup_worker(app: Workbench) {
    let weak = std::sync::Arc::downgrade(&app.0);
    drop(app);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1800)).await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let live = Workbench(inner);
            let enabled = live
                .0
                .catalog
                .lock()
                .unwrap()
                .server
                .value("worktreeCleanup")
                .and_then(|v| serde_json::from_value::<WorktreeCleanupRules>(v.clone()).ok())
                .is_some_and(|r| r.enabled);
            if !enabled {
                continue;
            }
            let _ = tokio::task::spawn_blocking(move || {
                report(
                    &live,
                    Retention {
                        days: 30,
                        worktree_ids: Vec::new(),
                        apply_policy: true,
                        compact_wal: true,
                        rules: None,
                    },
                    true,
                )
            })
            .await;
        }
    });
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
