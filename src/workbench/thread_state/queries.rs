use super::super::ProjectChat;
use super::{ApiError, Store, Workbench, bad, blocking};
use crate::{
    Error, Result,
    chat::{Item, Role},
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    convert::Infallible,
    path::PathBuf,
};
use uuid::Uuid;

struct Shell {
    sequence: u64,
    rows: Vec<ProjectChat>,
}
impl Workbench {
    fn shell(&self) -> Result<Shell> {
        let db = self.0.catalog.lock().unwrap().clone();
        let mut rows = Vec::new();
        for info in self.0.chats.list()? {
            for project in db.projects.values() {
                if project.metadata.deleted
                    || self.allowed(&project.root).is_err()
                    || !self.owns_chat(&db, project, &info)?
                {
                    continue;
                }
                rows.push(ProjectChat {
                    project: project.id,
                    archived: db.archived.contains(&info.id),
                    metadata: db
                        .thread_state
                        .rows
                        .get(&info.id)
                        .cloned()
                        .unwrap_or_default(),
                    info,
                });
                break;
            }
        }
        rows.sort_by_key(|r| (r.metadata.updated_ms.max(r.info.created_ms), r.info.id));
        Ok(Shell {
            sequence: db.thread_state.sequence(),
            rows,
        })
    }
    pub(in crate::workbench) fn display_title(&self, row: &ProjectChat) -> Result<String> {
        if let Some(title) = &row.metadata.title {
            return Ok(title.clone());
        }
        let history = self.0.chats.history(row.info.id)?;
        Ok(history
            .turns
            .first()
            .and_then(|t| {
                t.items.iter().find_map(|entry| match &entry.item {
                    Item::Message {
                        role: Role::User,
                        text,
                        ..
                    } if !text.trim().is_empty() => Some(text.trim().chars().take(80).collect()),
                    _ => None,
                })
            })
            .unwrap_or_default())
    }
}
#[derive(Deserialize)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Active,
    Archived,
    Family {
        chat: Uuid,
    },
    Titles {
        chats: BTreeSet<Uuid>,
    },
    Links {
        chats: BTreeSet<Uuid>,
    },
    Worktree {
        path: PathBuf,
    },
    OlderSettled {
        before_ms: u64,
        after_id: Option<Uuid>,
        project_ids: Option<BTreeSet<Uuid>>,
        limit: usize,
    },
}
fn root_of(mut id: Uuid, parents: &BTreeMap<Uuid, Uuid>) -> Result<Uuid> {
    let mut seen = BTreeSet::new();
    while let Some(&p) = parents.get(&id) {
        if !seen.insert(id) {
            return Err(bad("invalid chat family cycle"));
        }
        id = p;
    }
    Ok(id)
}
fn project(app: Workbench, request: Request) -> Result<Value> {
    let Shell { sequence, mut rows } = app.shell()?;
    match request {
        Request::Active => rows.retain(|r| !r.archived),
        Request::Archived => rows.retain(|r| r.archived),
        Request::Family { chat } => {
            let parents = rows
                .iter()
                .filter_map(|r| {
                    r.info
                        .forked_from
                        .as_ref()
                        .map(|f| f.chat)
                        .or(r.metadata.side_chat_parent)
                        .map(|parent| (r.info.id, parent))
                })
                .collect();
            let wanted = root_of(chat, &parents)?;
            if rows.iter().any(|r| r.info.id == chat) {
                let mut family = Vec::new();
                for r in rows {
                    if !r.archived && root_of(r.info.id, &parents)? == wanted {
                        family.push(r);
                    }
                }
                rows = family;
            } else {
                rows.clear();
            }
        }
        Request::Titles { ref chats } | Request::Links { ref chats } => {
            let links = matches!(request, Request::Links { .. });
            let mut values = BTreeMap::new();
            for r in &rows {
                if chats.contains(&r.info.id) {
                    let title = app.display_title(r)?;
                    values.insert(
                        r.info.id,
                        if links {
                            json!({"title":title,"project":r.project})
                        } else {
                            json!(title)
                        },
                    );
                }
            }
            return Ok(json!({"sequence":sequence,"values":values}));
        }
        Request::Worktree { path } => {
            let actual = path.canonicalize()?;
            app.workspace_at(&actual)?;
            let ids: Vec<_> = rows
                .iter()
                .filter(|r| !r.archived && r.info.workspace == actual)
                .map(|r| r.info.id)
                .collect();
            return Ok(json!({"sequence":sequence,"chats":ids}));
        }
        Request::OlderSettled {
            before_ms,
            after_id,
            project_ids,
            limit,
        } => {
            if limit > 50 {
                return Err(bad("settled chat page limit exceeds 50"));
            }
            let (archived_projects, delegated_chats): (BTreeSet<Uuid>, BTreeSet<Uuid>) = {
                let db = app.0.catalog.lock().unwrap();
                (
                    db.projects
                        .values()
                        .filter(|p| p.metadata.archived)
                        .map(|p| p.id)
                        .collect(),
                    db.model_tasks.keys().copied().collect(),
                )
            };
            rows.retain(|r| {
                !r.archived
                    && !archived_projects.contains(&r.project)
                    && r.metadata.side_chat_parent.is_none()
                    && !delegated_chats.contains(&r.info.id)
                    && project_ids
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&r.project))
                    && r.metadata.settled_override.as_deref() == Some("settled")
                    && r.metadata.settled_ms.is_some_and(|t| {
                        t < before_ms
                            || (t == before_ms && after_id.is_some_and(|id| r.info.id > id))
                    })
            });
            if limit == 0 {
                let mut counts = BTreeMap::<Uuid, usize>::new();
                for r in &rows {
                    *counts.entry(r.project).or_default() += 1;
                }
                return Ok(json!({"sequence":sequence,"counts":counts,"chats":[]}));
            }
            rows.sort_by_key(|r| (std::cmp::Reverse(r.metadata.settled_ms), r.info.id));
            rows.truncate(limit);
        }
    }
    Ok(json!({"sequence":sequence,"chats":rows}))
}
async fn lookup(
    State(app): State<Workbench>,
    Json(q): Json<Request>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || project(app, q)).await
}

#[derive(Deserialize, Default)]
struct Cursor {
    after: Option<u64>,
}
impl Store {
    pub(super) fn page(&self, p: Uuid, after: u64) -> (u64, VecDeque<super::Change>) {
        let mut read = after;
        let mut changes = VecDeque::new();
        for row in self.changes.iter().skip(after as usize) {
            read = row.sequence;
            if row.project == p {
                changes.push_back(row.clone());
                if changes.len() == 64 {
                    break;
                }
            }
        }
        (read, changes)
    }
}
async fn events(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    Query(q): Query<Cursor>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    app.project(p)?;
    let after = match headers.get("last-event-id") {
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or(Error::InvalidCursor)?,
        None => q.after.unwrap_or(0),
    };
    let rx = app.0.thread_updates.subscribe();
    if after > app.0.catalog.lock().unwrap().thread_state.sequence() {
        return Err(Error::InvalidCursor.into());
    }
    let stream = futures_util::stream::unfold(
        (app, rx, after, VecDeque::<super::Change>::new()),
        move |(app, mut rx, mut read, mut pending)| async move {
            loop {
                if let Some(change) = pending.pop_front() {
                    let event = Event::default()
                        .id(change.sequence.to_string())
                        .event("chat")
                        .json_data(change)
                        .ok()?;
                    return Some((Ok::<_, Infallible>(event), (app, rx, read, pending)));
                }
                {
                    let db = app.0.catalog.lock().unwrap();
                    let project = db.projects.get(&p)?;
                    if project.metadata.deleted || app.allowed(&project.root).is_err() {
                        return None;
                    }
                    (read, pending) = db.thread_state.page(p, read);
                }
                if pending.is_empty() && rx.changed().await.is_err() {
                    return None;
                }
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/chat-state/query", post(lookup))
        .route("/projects/{p}/chat-state/events", get(events))
}
