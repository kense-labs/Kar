//! Project catalog transactions. A chat's physical directory is immutable even when its project moves.
mod managed;
mod mutation;
use super::{
    Catalog, Project, Workbench, denied,
    http::{ApiError, blocking},
    missing,
};
use crate::{Error, Result, chat::ChatInfo};
use axum::{Json, Router, extract::State as App, routing::post};
use cap_fs_ext::DirExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub settings: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    project: Uuid,
    cwd: PathBuf,
    #[serde(default)]
    fork: Option<(Uuid, Uuid)>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct State {
    owners: BTreeMap<Uuid, Binding>,
    receipts: BTreeMap<String, (Value, Project)>,
    scratch: Option<Uuid>,
    #[serde(default)]
    deleted_projects: std::collections::BTreeSet<Uuid>,
}

impl State {
    pub(in crate::workbench) fn is_deleted(&self, id: Uuid) -> bool {
        self.deleted_projects.contains(&id)
    }
}

impl Workbench {
    pub(super) fn owns_chat(
        &self,
        catalog: &Catalog,
        p: &Project,
        info: &ChatInfo,
    ) -> Result<bool> {
        if let Some(b) = catalog.lifecycle.owners.get(&info.id) {
            if b.project != p.id || b.cwd != info.workspace {
                return Ok(false);
            }
            if let Some(task) = self.0.task_folders.workspace(p.id, info.id)?
                && task != b.cwd
            {
                return Err(denied());
            }
            return Ok(true);
        }
        // CLI chats and catalogs predating durable ownership are adopted on the next mutation.
        if let Some(parent) = info
            .forked_from
            .as_ref()
            .and_then(|f| catalog.lifecycle.owners.get(&f.chat))
            && parent.cwd == info.workspace
        {
            return Ok(parent.project == p.id);
        }
        if info.workspace == p.root {
            return Ok(true);
        }
        match self.0.task_folders.workspace(p.id, info.id) {
            Ok(path) => Ok(path.as_ref() == Some(&info.workspace)),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn remember_chats(&self, c: &mut Catalog, p: &Project) -> Result<()> {
        for info in self.0.chats.list()? {
            if !c.lifecycle.owners.contains_key(&info.id) && self.owns_chat(c, p, &info)? {
                c.lifecycle.owners.insert(
                    info.id,
                    Binding {
                        project: p.id,
                        fork: info.forked_from.as_ref().map(|f| (f.chat, f.turn)),
                        cwd: info.workspace,
                    },
                );
            }
        }
        Ok(())
    }

    pub(super) fn reserve_chat_directory(&self, project: Uuid, id: Uuid) -> Result<PathBuf> {
        self.update(|c| {
            let p = c.projects.get(&project).ok_or_else(missing)?;
            if let Some(b) = c.lifecycle.owners.get(&id) {
                if b.fork.is_some() {
                    return Err(Error::Refused("chat ID belongs to a fork".into()));
                }
                if b.project != project {
                    return Err(missing());
                }
                self.workspace_at(&b.cwd)?;
                if let Some(task) = self.0.task_folders.workspace(project, id)?
                    && task != b.cwd
                {
                    return Err(denied());
                }
                return Ok(b.cwd.clone());
            }
            let cwd = match self.0.chats.initial_info(id) {
                Ok(info) if info.forked_from.is_none() && self.owns_chat(c, p, &info)? => {
                    info.workspace
                }
                Ok(_) => {
                    return Err(Error::Refused(
                        "chat ID belongs to a different creation request".into(),
                    ));
                }
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(cwd) = self.0.task_folders.workspace(project, id)? {
                        cwd
                    } else if c.lifecycle.scratch == Some(project) {
                        self.claim_scratch_chat(&p.root, id)?
                    } else {
                        p.root.clone()
                    }
                }
                Err(e) => return Err(e),
            };
            self.workspace_at(&cwd)?;
            c.lifecycle.owners.insert(
                id,
                Binding {
                    project,
                    cwd: cwd.clone(),
                    fork: None,
                },
            );
            Ok(cwd)
        })
    }

    pub(super) fn reserve_fork_directory(
        &self,
        parent: Uuid,
        parent_project: Uuid,
        turn: Uuid,
        child: Uuid,
        target: Uuid,
        cwd: &Path,
        independent: bool,
    ) -> Result<()> {
        self.update(|c| {
            let source = self.0.chats.saved_info(parent)?;
            if !self.owns_chat(
                c,
                c.projects.get(&parent_project).ok_or_else(missing)?,
                &source,
            )? {
                return Err(missing());
            }
            let p = c.projects.get(&target).ok_or_else(missing)?;
            if let Some(b) = c.lifecycle.owners.get(&child) {
                if b.project != target || b.cwd != cwd || b.fork != Some((parent, turn)) {
                    return Err(Error::Refused(
                        "chat ID belongs to a different fork request".into(),
                    ));
                }
                return Ok(());
            }
            if independent && p.root != cwd {
                return Err(Error::Busy);
            }
            match self.0.chats.initial_info(child) {
                Ok(info)
                    if info.workspace == cwd
                        && info
                            .forked_from
                            .as_ref()
                            .is_some_and(|f| (f.chat, f.turn) == (parent, turn)) => {}
                Ok(_) => return Err(Error::Refused("chat ID already used".into())),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            c.lifecycle.owners.insert(
                child,
                Binding {
                    project: target,
                    cwd: cwd.into(),
                    fork: Some((parent, turn)),
                },
            );
            Ok(())
        })
    }
    pub(super) fn saved_fork_directory(&self, child: Uuid, target: Uuid) -> Option<PathBuf> {
        self.0
            .catalog
            .lock()
            .unwrap()
            .lifecycle
            .owners
            .get(&child)
            .filter(|b| b.project == target && b.fork.is_some())
            .map(|b| b.cwd.clone())
    }

    /// Create or open directories component by component under a pinned authorized handle.
    fn ensure_directory(&self, path: &Path) -> Result<PathBuf> {
        let root = self.allowed(path)?;
        let mut dir = root.dir.try_clone()?;
        for part in path
            .strip_prefix(&root.path)
            .map_err(|_| denied())?
            .components()
        {
            let std::path::Component::Normal(name) = part else {
                return Err(denied());
            };
            match dir.create_dir(name) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            dir = dir.open_dir_nofollow(name)?;
        }
        self.workspace_at(path)?;
        Ok(path.to_owned())
    }
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/mutate", post(mutate))
        .route("/projects/ensure-scratch", post(scratch))
        .route("/projects/create-new", post(create_new))
        .route("/projects/create-from-thread", post(promote))
}
async fn mutate(
    App(app): App<Workbench>,
    Json(input): Json<Value>,
) -> std::result::Result<Json<Project>, ApiError> {
    // Finish a committed deletion even if the HTTP client disconnects while the operation is in flight.
    tokio::spawn(async move { finish_mutation(app, input).await })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
}
async fn finish_mutation(
    app: Workbench,
    input: Value,
) -> std::result::Result<Json<Project>, ApiError> {
    let host = app.clone();
    let result = blocking(move || host.mutate_project(input)).await?;
    if result.metadata.deleted {
        super::forge::clone_jobs::finish_discard(&app, result.id).await?;
        let ids: Vec<_> = app
            .0
            .catalog
            .lock()
            .unwrap()
            .lifecycle
            .owners
            .iter()
            .filter(|(_, binding)| binding.project == result.id)
            .map(|(id, _)| *id)
            .collect();
        app.0.access.notify();
        for id in ids {
            let slot = app.controller(id);
            let mut owner = slot.owner.lock().await;
            if let Some(session) = owner.take() {
                session.close().await?;
            }
        }
    }
    Ok(result)
}
async fn scratch(App(app): App<Workbench>) -> std::result::Result<Json<Project>, ApiError> {
    blocking(move || app.ensure_scratch()).await
}
#[derive(Deserialize)]
struct Named {
    name: String,
}
async fn create_new(
    App(app): App<Workbench>,
    Json(req): Json<Named>,
) -> std::result::Result<Json<managed::Created>, ApiError> {
    blocking(move || app.create_named(&req.name)).await
}
#[derive(Deserialize)]
struct Promote {
    chat: Uuid,
}
async fn promote(
    App(app): App<Workbench>,
    Json(req): Json<Promote>,
) -> std::result::Result<Json<Project>, ApiError> {
    let host = app.clone();
    let result = blocking(move || host.promote_scratch_chat(req.chat)).await?;
    app.0.access.notify();
    Ok(result)
}
