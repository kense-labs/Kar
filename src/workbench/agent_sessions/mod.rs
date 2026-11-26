//! Read local provider history into the existing chat journal and resume its native binding.
mod codex;
mod discover;
pub(super) use discover::input as transcript_input;
mod jsonl;
pub(super) use jsonl::usage_record;
mod transcript;
use super::{
    Workbench,
    http::{ApiError, blocking},
    missing,
};
use crate::{Error, Result, chat::import::native::ImportStatus};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use discover::{MIB, Source};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
};
use uuid::Uuid;

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/agent-sessions/scan", get(scan))
        .route("/projects/{project}/agent-sessions/import", post(import))
}
#[derive(Serialize)]
struct Candidate {
    path: PathBuf,
    title: String,
    project_id: Option<Uuid>,
    sources: BTreeSet<&'static str>,
    thread_count: usize,
    last_active_at_ms: u64,
    already_imported: bool,
    git: Option<discover::Git>,
}
#[derive(Serialize)]
struct Scan {
    candidates: Vec<Candidate>,
    scanned_at_ms: u64,
    truncated: bool,
}
fn now() -> u64 {
    jiff::Timestamp::now().as_millisecond() as u64
}
async fn scan(State(app): State<Workbench>) -> std::result::Result<Json<Scan>, ApiError> {
    blocking(move || {
        let (files, truncated) = discover::discover(&app)?;
        let projects: BTreeMap<_, _> = app
            .projects()
            .into_iter()
            .filter(|p| !p.metadata.deleted)
            .map(|p| (p.root, p.id))
            .collect();
        let mut candidates = BTreeMap::new();
        for file in files {
            let c = candidates.entry(file.project.clone()).or_insert_with(|| {
                let root = file.project;
                let project_id = projects.get(&root).copied();
                Candidate {
                    git: discover::git(&root),
                    title: root
                        .file_name()
                        .unwrap_or(root.as_os_str())
                        .to_string_lossy()
                        .into_owned(),
                    path: root,
                    already_imported: project_id.is_some(),
                    project_id,
                    sources: Default::default(),
                    thread_count: 0,
                    last_active_at_ms: file.modified,
                }
            });
            c.sources.insert(file.source.runtime());
            c.thread_count += 1;
        }
        let mut candidates = candidates.into_values().collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            b.last_active_at_ms
                .cmp(&a.last_active_at_ms)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(Scan {
            candidates,
            scanned_at_ms: now(),
            truncated,
        })
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    expected_workspace_root: Option<PathBuf>,
}
#[derive(Default, Serialize)]
struct Outcome {
    imported_count: usize,
    skipped_count: usize,
    already_present_count: usize,
    chat_ids: Vec<Uuid>,
    truncated: bool,
}
async fn import(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(request): Json<Request>,
) -> std::result::Result<Json<Outcome>, ApiError> {
    blocking(move || adopt(&app, project, request)).await
}
fn adopt(app: &Workbench, project: Uuid, request: Request) -> Result<Outcome> {
    let p = app.project(project)?;
    let dest = app.workspace(&p)?;
    if p.metadata.deleted {
        return Err(missing());
    }
    if let Some(expected) = request.expected_workspace_root
        && expected.canonicalize().ok().as_ref() != Some(&p.root)
    {
        return Err(Error::Refused(
            "project directory changed since scan".into(),
        ));
    }
    let (files, truncated) = discover::discover(app)?;
    let bases = files
        .iter()
        .filter(|f| f.source == Source::Codex)
        .filter_map(|f| {
            Some((
                (
                    f.instance.id.clone(),
                    transcript::rollout(&f.path)?.to_owned(),
                ),
                f,
            ))
        })
        .collect::<HashMap<_, _>>();
    let mut owned = HashSet::new();
    for chat in app.0.chats.list()? {
        app.0.chats.visit_native_bindings(chat.id, |info| {
            let instance = info
                .active_instance()
                .map_or(info.runtime.as_str(), |i| i.id.as_str());
            owned.insert((instance.to_owned(), info.native_id.clone(), chat.id));
        })?;
    }
    let receipts = app
        .0
        .catalog
        .lock()
        .unwrap()
        .imported_transcripts
        .get(&project)
        .cloned()
        .unwrap_or_default();
    let mut result = Outcome {
        truncated,
        ..Default::default()
    };
    let start = now();
    let cutoff = start.saturating_sub(30 * 24 * 60 * 60 * 1000);
    let mut bytes = 4096 * MIB;
    let mut remaining = 100;
    let mut visited = BTreeSet::new();
    for f in files.iter().filter(|f| {
        (f.project == p.root || f.cwd == p.root) && (cutoff..=start).contains(&f.modified)
    }) {
        if let Some(id) = receipts
            .get(&f.path)
            .filter(|(s, _)| s == &f.instance.receipt(&f.stamp))
            .map(|(_, id)| *id)
            && app.binding(p.id, id).is_ok()
        {
            if visited.insert(id) {
                result.already_present_count += 1;
                result.chat_ids.push(id)
            }
            continue;
        }
        if remaining == 0 || bytes == 0 {
            result.truncated = true;
            result.skipped_count += 1;
            continue;
        }
        if f.size > 4096 * MIB {
            result.skipped_count += 1;
            continue;
        }
        remaining -= 1;
        let mut history = match transcript::parse(f, &bases, &mut bytes) {
            Some(transcript::Parsed::Empty) => {
                remaining += 1;
                continue;
            }
            Some(transcript::Parsed::Ready(history)) => *history,
            None => {
                result.skipped_count += 1;
                continue;
            }
        };
        let id = history.id();
        history.origin.project = Some(p.id);
        if owned.iter().any(|(r, n, owner)| {
            r == history.origin.instance_id() && n == &history.origin.native_id && *owner != id
        }) {
            result.skipped_count += 1;
            continue;
        }
        let cwd = history.workspace.clone();
        let title = history.origin.title.clone();
        let status = app.update(|catalog| {
            let p_now = catalog
                .projects
                .get(&project)
                .filter(|p| !p.metadata.deleted)
                .ok_or_else(missing)?;
            if p_now.root != dest.root {
                return Err(Error::Refused(
                    "project moved while importing native history".into(),
                ));
            }
            app.workspace_at(&cwd)?;
            if catalog.family.deleted.contains_key(&id)
                || catalog
                    .thread_state
                    .rows
                    .get(&id)
                    .is_some_and(|m| m.side_chat_parent.is_some())
                || !catalog.lifecycle.can_bind_import(project, id, &cwd)
            {
                return Ok(ImportStatus::Changed);
            }
            if !discover::still_belongs(&cwd, &p.root) {
                return Ok(ImportStatus::Changed);
            }
            if f.file().is_err() {
                return Ok(ImportStatus::Changed);
            }
            let status = match app.0.chats.adopt_native(history) {
                Ok(status) => status,
                Err(Error::Busy) => return Ok(ImportStatus::Changed),
                Err(e) => return Err(e),
            };
            if !matches!(status, ImportStatus::Changed) {
                catalog.lifecycle.bind_import(project, id, cwd);
                catalog
                    .thread_state
                    .rows
                    .entry(id)
                    .or_default()
                    .title
                    .get_or_insert(title);
                catalog
                    .imported_transcripts
                    .entry(project)
                    .or_default()
                    .insert(f.path.clone(), (f.instance.receipt(&f.stamp), id));
            }
            Ok(status)
        })?;
        match status {
            ImportStatus::Changed => result.skipped_count += 1,
            ImportStatus::Created => {
                result.imported_count += 1;
                if visited.insert(id) {
                    result.chat_ids.push(id);
                }
            }
            ImportStatus::AlreadyPresent => {
                if visited.insert(id) {
                    result.already_present_count += 1;
                    result.chat_ids.push(id);
                }
            }
        }
    }
    Ok(result)
}
