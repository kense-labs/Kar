//! Project ownership and durable checkout bindings for chats.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task: Option<Uuid>,
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
    pub(in crate::workbench) fn ownership_snapshot(&self) -> Self {
        Self {
            owners: self.owners.clone(),
            ..Self::default()
        }
    }
    pub(in crate::workbench) fn handoff_workspace(&self, chat: Uuid) -> Option<&Path> {
        let binding = self.owners.get(&chat)?;
        binding.task.map(|_| binding.cwd.as_path())
    }
    pub(in crate::workbench) fn move_checkout(
        &mut self,
        project: Uuid,
        chat: Uuid,
        task: Uuid,
        from: &Path,
        to: &Path,
    ) -> Result<()> {
        if let Some(b) = self.owners.get(&chat)
            && (b.project != project || b.cwd != from && b.cwd != to)
        {
            return Err(denied());
        }
        let binding = self.owners.entry(chat).or_insert_with(|| Binding {
            project,
            cwd: to.into(),
            fork: None,
            task: None,
        });
        binding.cwd = to.into();
        binding.task = Some(task);
        Ok(())
    }
    pub(super) fn can_bind_import(&self, project: Uuid, chat: Uuid, cwd: &Path) -> bool {
        self.owners
            .get(&chat)
            .is_none_or(|b| b.project == project && b.cwd == cwd && b.fork.is_none())
    }
    pub(super) fn bind_import(&mut self, project: Uuid, chat: Uuid, cwd: PathBuf) {
        self.owners.entry(chat).or_insert(Binding {
            task: None,
            project,
            cwd,
            fork: None,
        });
    }

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
        if catalog.family.deleted.contains_key(&info.id) {
            return Ok(false);
        }
        self.owns_chat_in(&catalog.lifecycle, p, info)
    }

    pub(in crate::workbench) fn owns_chat_in(
        &self,
        ownership: &State,
        p: &Project,
        info: &ChatInfo,
    ) -> Result<bool> {
        if let Some(b) = ownership.owners.get(&info.id) {
            if b.project != p.id || b.cwd != info.workspace {
                return Ok(false);
            }
            self.check_bound_task(b, info.id)?;
            return Ok(true);
        }
        // CLI chats and catalogs predating durable ownership are adopted on the next mutation.
        if let Some(parent) = info
            .forked_from
            .as_ref()
            .and_then(|f| ownership.owners.get(&f.chat))
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

    fn check_bound_task(&self, binding: &Binding, chat: Uuid) -> Result<()> {
        let path = self
            .0
            .task_folders
            .workspace(binding.project, binding.task.unwrap_or(chat))?;
        if path.as_ref().is_some_and(|cwd| cwd != &binding.cwd)
            || binding.task.is_some() && path.is_none()
        {
            return Err(denied());
        }
        Ok(())
    }

    fn remember_chats(&self, c: &mut Catalog, p: &Project) -> Result<()> {
        for info in self.0.chats.list()? {
            if !c.lifecycle.owners.contains_key(&info.id) && self.owns_chat(c, p, &info)? {
                c.lifecycle.owners.insert(
                    info.id,
                    Binding {
                        task: None,
                        project: p.id,
                        fork: info.forked_from.as_ref().map(|f| (f.chat, f.turn)),
                        cwd: info.workspace,
                    },
                );
            }
        }
        Ok(())
    }

    pub(in crate::workbench) fn bind_model_checkout(
        &self,
        project: Uuid,
        chat: Uuid,
        cwd: &Path,
    ) -> Result<()> {
        self.update(|db| {
            if let Some(binding) = db.lifecycle.owners.get(&chat) {
                if binding.project != project || binding.cwd != cwd || binding.fork.is_some() {
                    return Err(denied());
                }
            } else {
                match self.0.chats.initial_info(chat) {
                    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => return Err(denied()),
                    Err(e) => return Err(e),
                }
                db.lifecycle.bind_import(project, chat, cwd.to_owned());
            }
            Ok(())
        })
    }
    pub(super) fn reserve_chat_directory(&self, project: Uuid, id: Uuid) -> Result<PathBuf> {
        self.update(|c| {
            if c.family.deleted.contains_key(&id) {
                return Err(missing());
            }
            if c.thread_state
                .rows
                .get(&id)
                .is_some_and(|m| m.side_chat_parent.is_some())
            {
                return Err(Error::Refused("chat ID belongs to a side chat".into()));
            }
            let p = c.projects.get(&project).ok_or_else(missing)?;
            if let Some(b) = c.lifecycle.owners.get(&id) {
                if b.fork.is_some() {
                    return Err(Error::Refused("chat ID belongs to a fork".into()));
                }
                if b.project != project {
                    return Err(missing());
                }
                self.workspace_at(&b.cwd)?;
                self.check_bound_task(b, id)?;
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
                    task: None,
                    project,
                    cwd: cwd.clone(),
                    fork: None,
                },
            );
            Ok(cwd)
        })
    }

    pub(super) fn saved_side_point(&self, parent: Uuid, child: Uuid) -> Option<Option<Uuid>> {
        let db = self.0.catalog.lock().unwrap();
        if db.thread_state.rows.get(&child)?.side_chat_parent != Some(parent) {
            return None;
        }
        Some(db.lifecycle.owners.get(&child)?.fork.map(|(_, turn)| turn))
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reserve_fork_directory(
        &self,
        parent: Uuid,
        parent_project: Uuid,
        turn: Option<Uuid>,
        child: Uuid,
        target: Uuid,
        cwd: &Path,
        independent: bool,
        side_chat: bool,
    ) -> Result<()> {
        self.update(|c| {
            if c.family.deleted.contains_key(&child) || c.family.deleted.contains_key(&parent) {
                return Err(missing());
            }
            if side_chat
                && (parent == child
                    || target != parent_project
                    || c.model_tasks.contains_key(&parent)
                    || c.thread_state
                        .rows
                        .get(&parent)
                        .is_some_and(|m| m.side_chat_parent.is_some()))
            {
                return Err(Error::Refused(
                    "a side chat requires a main chat in the same project".into(),
                ));
            }
            if super::mcp::pending_launch(c, child).is_some()
                || super::prepared_runs::reservation(c, child).is_some()
            {
                return Err(Error::Refused(
                    "chat ID is reserved for a model launch".into(),
                ));
            }
            let fork = turn.map(|t| (parent, t));
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
                if b.project != target
                    || b.cwd != cwd
                    || b.fork != fork
                    || c.thread_state
                        .rows
                        .get(&child)
                        .and_then(|m| m.side_chat_parent)
                        != side_chat.then_some(parent)
                {
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
                    if !side_chat
                        && info.workspace == cwd
                        && info
                            .forked_from
                            .as_ref()
                            .is_some_and(|f| Some((f.chat, f.turn)) == fork) => {}
                Ok(_) => return Err(Error::Refused("chat ID already used".into())),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            if side_chat {
                c.thread_state
                    .rows
                    .entry(child)
                    .or_default()
                    .side_chat_parent = Some(parent);
            }
            c.lifecycle.owners.insert(
                child,
                Binding {
                    task: None,
                    project: target,
                    cwd: cwd.into(),
                    fork,
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
    Ok(Json(model_mutation(app, input, None).await?))
}
pub(in crate::workbench) async fn model_mutation(
    app: Workbench,
    input: Value,
    calling: Option<Uuid>,
) -> Result<Project> {
    let host = app.clone();
    let result = tokio::task::spawn_blocking(move || host.mutate_project(input))
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
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
            let close = async move {
                let mut owner = slot.owner.lock().await;
                if let Some(session) = owner.take() {
                    session.close().await?;
                }
                Ok::<_, Error>(())
            };
            // A model making this call is still inside the turn holding its own controller.
            if Some(id) == calling {
                tokio::spawn(close);
            } else {
                close.await?;
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
