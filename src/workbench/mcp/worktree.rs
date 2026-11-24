//! The MCP call admits a move and releases the caller. A durable worker rebinds the same Kar chat
//! at a turn boundary; waiting for the current model from inside its tool call would deadlock.
use super::*;
use crate::{
    Error, Result,
    chat::ChatInfo,
    workbench_git::{BranchQuery, Repository},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    command_id: Uuid,
    branch: String,
    #[serde(default = "head")]
    base_ref: String,
    continuation_prompt: Option<String>,
    start_from_origin: Option<bool>,
}
fn head() -> String {
    "HEAD".into()
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Pending,
    Attached,
    Failed,
}

#[derive(Clone, Serialize, Deserialize)]
pub(in crate::workbench) struct Move {
    project: Uuid,
    chat: Uuid,
    request: Request,
    source: PathBuf,
    directory: PathBuf,
    workspace: Option<PathBuf>,
    continuation_id: Option<Uuid>,
    #[serde(default)]
    start_from_origin: bool,
    #[serde(default)]
    remote_base: Option<String>,
    status: Phase,
    detail: Option<String>,
}

pub(in crate::workbench) fn task_for(app: &Workbench, chat: &ChatInfo) -> Option<Uuid> {
    app.0
        .catalog
        .lock()
        .unwrap()
        .model_worktree_moves
        .values()
        .find(|r| r.chat == chat.id && r.workspace.as_deref() == Some(&chat.workspace))
        .map(|r| r.request.command_id)
}
pub(in crate::workbench) fn pending(app: &Workbench, chat: Uuid) -> bool {
    app.0
        .catalog
        .lock()
        .unwrap()
        .model_worktree_moves
        .values()
        .any(|r| r.chat == chat && r.status == Phase::Pending)
}
pub(in crate::workbench) fn recover(app: &Workbench) {
    let ids: Vec<_> = app
        .0
        .catalog
        .lock()
        .unwrap()
        .model_worktree_moves
        .values()
        .filter(|r| r.status == Phase::Pending)
        .map(|r| r.request.command_id)
        .collect();
    for command in ids {
        launch(app.clone(), command);
    }
}
fn launch(app: Workbench, id: Uuid) {
    tokio::spawn(async move {
        let row = app
            .0
            .catalog
            .lock()
            .unwrap()
            .model_worktree_moves
            .get(&id)
            .cloned();
        let Some(row) = row.filter(|r| r.status == Phase::Pending) else {
            return;
        };
        let controller = app.controller(row.chat);
        let Ok(mut owner) = controller.owner.try_lock() else {
            return;
        };
        let result = finish(&app, &row, &controller, &mut owner).await;
        if let Err(e) = result {
            // A committed chat move always wins. In particular, a failure publishing the project
            // catalog must be repaired after restart rather than turning the receipt into a refusal.
            if !matches!(e, Error::Busy) && !committed(&app, &row).unwrap_or(true) {
                let _ = app.update(|db| {
                    if let Some(r) = db
                        .model_worktree_moves
                        .get_mut(&id)
                        .filter(|r| r.status == Phase::Pending)
                    {
                        r.status = Phase::Failed;
                        r.detail = Some(e.to_string());
                    }
                    Ok(())
                });
            }
        }
    });
}
fn committed(app: &Workbench, row: &Move) -> Result<bool> {
    let info = app.0.chats.saved_info(row.chat)?;
    if info.workspace == row.source {
        return Ok(false);
    }
    Ok(app.0.chats.history(row.chat)?.handoffs.iter().any(|h| {
        h.request.id == row.request.command_id && h.source_workspace.as_ref() == Some(&row.source)
    }))
}
fn default_origin(project: &super::super::Project) -> bool {
    project
        .metadata
        .settings
        .get("newWorktreesStartFromOrigin")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}
fn directory_ok(policy: Option<&Policy>, target: &Path) -> bool {
    policy.and_then(Policy::worktree_directory) == target.parent()
}
async fn finish(
    app: &Workbench,
    row: &Move,
    slot: &crate::workbench::Controller,
    owner: &mut tokio::sync::MutexGuard<'_, Option<crate::chat::ChatSession>>,
) -> Result<()> {
    let already = committed(app, row)?;
    let id = row.request.command_id;
    if !already {
        if !directory_ok(
            Policy::current(&app.0.catalog.lock().unwrap(), row.project, row.chat),
            &row.directory,
        ) {
            return Err(Error::Refused(
                "worktree handoff permission was revoked".into(),
            ));
        }
        let info = app.binding(row.project, row.chat)?;
        if info.workspace != row.source {
            return Err(Error::Refused("workspace has changed".into()));
        }
        if owner
            .as_ref()
            .is_some_and(|s| !s.is_closed() && s.needs_controller().unwrap_or(true))
        {
            return Err(Error::Busy);
        }
    }
    let service = app.clone();
    let op = row.clone();
    let (target, remote) = tokio::task::spawn_blocking(move || {
        if service.destination(&op.directory)? != op.directory {
            return Err(crate::workbench::denied());
        }
        let (task, remote) = crate::workbench::task_folders::handoff_task(
            &service,
            op.project,
            id,
            op.directory,
            op.request.branch,
            op.request.base_ref,
            op.start_from_origin,
        )?;
        if service.0.task_folders.workspace(op.project, id)?.as_ref() != Some(&task.project_path) {
            return Err(crate::workbench::denied());
        }
        Ok((task.project_path.canonicalize()?, remote))
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    app.workspace_at(&target)?;
    if !already {
        app.update(|db| {
            if !directory_ok(Policy::current(db, row.project, row.chat), &row.directory) {
                return Err(Error::Refused(
                    "worktree handoff permission was revoked".into(),
                ));
            }
            db.model_worktree_moves
                .get_mut(&id)
                .ok_or_else(crate::workbench::missing)?
                .workspace = Some(target.clone());
            Ok(())
        })?;
        if let Some(source) = owner.take() {
            source.close().await?;
        }
        let info = app.0.chats.saved_info(row.chat)?;
        let tools = app.0.mcp.issue(row.project, row.chat, &info.runtime)?;
        let pin = crate::workbench_git::pin_checkout(&target)?;
        let new = app
            .0
            .chats
            .move_workspace(
                row.chat,
                id,
                &row.source,
                &target,
                pin.as_ref().map(|(id, _)| *id),
                |opts| {
                    if let Some(lease) = &tools {
                        lease.configure(opts);
                    }
                    opts.prompt_suggestions = app.suggestion_source(row.chat);
                },
            )
            .await;
        match new {
            Ok(Some(mut chat)) => {
                slot.bind_model_tools(&mut chat, tools);
                **owner = Some(chat);
            }
            Ok(None) => {}
            Err(Error::Busy) => {
                // A crashed prior host may have left a native turn to reconcile. Re-open its
                // original native session normally, then retry at the next idle boundary.
                app.attach_bound(row.project, row.chat, owner).await?;
                return Err(Error::Busy);
            }
            Err(e) => return Err(e),
        }
    }
    let info = app.0.chats.saved_info(row.chat)?;
    if info.workspace != target {
        return Err(Error::Refused("published chat workspace mismatch".into()));
    }
    // The inbox uses this stable UUID for the optional continuation. Retrying after either
    // the inbox append or catalog publication cannot enqueue or submit the message twice.
    if let (Some(prompt), Some(input)) = (&row.request.continuation_prompt, row.continuation_id) {
        app.0.chats.enqueue(row.chat, input, prompt)?;
    }
    app.update(|db| {
        db.lifecycle
            .move_checkout(row.project, row.chat, id, &row.source, &target)?;
        let r = db
            .model_worktree_moves
            .get_mut(&id)
            .ok_or_else(crate::workbench::missing)?;
        r.workspace = Some(target);
        r.remote_base = remote;
        r.status = Phase::Attached;
        r.detail = None;
        Ok(())
    })?;
    app.wake_queue();
    Ok(())
}

pub(super) async fn call(
    app: Workbench,
    caller: Caller,
    name: &str,
    mut args: Value,
) -> Result<Value> {
    let fields = args
        .as_object_mut()
        .ok_or_else(|| Error::Refused("expected worktree arguments".into()))?;
    let project = serde_json::from_value(fields.remove("project_id").unwrap_or(Value::Null))?;
    let chat = serde_json::from_value(fields.remove("thread_id").unwrap_or(Value::Null))?;
    let name = name.to_owned();
    let service = app.clone();
    let (value, command) = tokio::task::spawn_blocking(move || {
        dispatch(&service, &caller, project, chat, &name, args)
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    if let Some(id) = command {
        launch(app, id);
    }
    Ok(value)
}

fn dispatch(
    app: &Workbench,
    caller: &Caller,
    project: Option<Uuid>,
    chat: Option<Uuid>,
    name: &str,
    args: Value,
) -> Result<(Value, Option<Uuid>)> {
    let grant = caller
        .grant
        .as_ref()
        .filter(|g| g.active())
        .ok_or_else(|| Error::Refused("active model session required".into()))?;
    let (project, chat) = toolkit::target(app, caller, project, chat)?;
    let info = app.binding(project, chat)?;
    let registered = app.project(project)?;
    if registered.metadata.deleted || app.archived(chat) {
        return Err(crate::workbench::missing());
    }
    if name == "kar_worktree_list" {
        let query: BranchQuery = serde_json::from_value(args)?;
        return Ok((
            json!(app.chat_worktree_refs(&info.workspace, &query)?),
            None,
        ));
    }
    if name == "kar_worktree_status" {
        if !args.as_object().is_some_and(|a| a.is_empty()) {
            return Err(Error::Refused("unexpected status arguments".into()));
        }
        let checkout = app.0.task_folders.workspace(project, chat)?.is_some()
            || info.worktree_id.is_some()
            || task_for(app, &info).is_some();
        let repo = Repository::open(&info.workspace).ok();
        let branch = repo
            .as_ref()
            .and_then(|r| r.status().ok())
            .and_then(|s| s.branch);
        let db = app.0.catalog.lock().unwrap();
        let rows = db
            .model_worktree_moves
            .values()
            .filter(|r| r.project == project && r.chat == chat)
            .collect::<Vec<_>>();
        return Ok((
            json!({"attached":checkout,"workspace":info.workspace,"project_workspace":registered.root,"branch":branch,"default_start_from_origin":default_origin(&registered),"handoffs":rows}),
            None,
        ));
    }
    let request: Request = serde_json::from_value(args)?;
    if request.branch.is_empty()
        || request.branch.starts_with('-')
        || request.branch.len() > 1024
        || request.base_ref.is_empty()
        || request.base_ref.starts_with('-')
        || request.base_ref.len() > 1024
        || request
            .continuation_prompt
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 65536)
    {
        return Err(Error::Refused(
            "invalid branch, base or continuation prompt".into(),
        ));
    }
    let id = request.command_id;
    if id == chat {
        return Err(Error::Refused(
            "handoff command must differ from chat identifier".into(),
        ));
    }
    let receipt = app.update(|db| {
        let policy = Policy::current(db, project, chat);
        if caller.plan || !grant.active() || policy != caller.policy.as_ref() {
            return Err(crate::workbench::denied());
        }
        if db
            .projects
            .get(&project)
            .is_none_or(|p| p.root != registered.root)
        {
            return Err(crate::workbench::denied());
        }
        let directory = policy
            .and_then(Policy::worktree_directory)
            .ok_or_else(crate::workbench::denied)?
            .join(format!("kar-{id}"));
        if let Some(old) = db.model_worktree_moves.get(&id) {
            if old.project != project
                || old.chat != chat
                || old.request != request
                || old.directory != directory
            {
                return Err(Error::Refused(
                    "worktree handoff command already used".into(),
                ));
            }
            return Ok(old.clone());
        }
        if info.workspace != registered.root
            || info.worktree_id.is_some()
            || db
                .model_worktree_moves
                .values()
                .any(|r| r.chat == chat && r.status != Phase::Failed)
        {
            return Err(Error::Refused(
                "chat already has a worktree or pending handoff".into(),
            ));
        }
        let continuation_id = request.continuation_prompt.as_ref().map(|_| Uuid::new_v4());
        let start_from_origin = request
            .start_from_origin
            .unwrap_or_else(|| default_origin(&registered));
        let row = Move {
            project,
            chat,
            continuation_id,
            start_from_origin,
            remote_base: None,
            request,
            source: info.workspace,
            directory,
            workspace: None,
            status: Phase::Pending,
            detail: None,
        };
        db.model_worktree_moves.insert(id, row.clone());
        Ok(row)
    })?;
    Ok((
        json!({"handoff":receipt}),
        (receipt.status == Phase::Pending).then_some(id),
    ))
}
