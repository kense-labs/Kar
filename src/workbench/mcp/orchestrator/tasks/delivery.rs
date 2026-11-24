//! A child result is owned by its spawning parent. The parent inbox, not a remote template
//! or the task worker, decides which wake actually entered the native conversation.
use super::*;
use crate::chat::{CompletionCommand, CompletionSource};
use crate::workbench::http::ApiError;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};

pub(super) fn active(app: &Workbench, parent: Uuid) -> Result<Option<Uuid>> {
    Ok(app
        .0
        .chats
        .history(parent)?
        .turns
        .last()
        .filter(|t| t.status == TurnStatus::Running)
        .map(|t| t.id))
}
fn source(app: &Workbench, t: &Task) -> Result<CompletionSource> {
    let dispose = if app.archived(t.parent) {
        Some("parent archived")
    } else if t.phase == Phase::Cancelled {
        Some("delegated work cancelled")
    } else if still_granted(app, t).is_err() {
        Some("delegation grant revoked")
    } else {
        None
    };
    Ok(CompletionSource {
        task: t.id,
        parent_run: t.parent_turn,
        terminal: t.terminal(),
        wake: t.wake(),
        policy: t.spec.completion_wake.unwrap_or_default(),
        text: format!(
            "Delegated task {} finished ({})\n{}",
            t.id,
            serde_json::to_string(&t.phase)?,
            t.summary.as_deref().unwrap_or("")
        ),
        dispose,
        legacy_reported: t.completion.is_none() && t.delivered,
    })
}
pub(super) fn sync(
    app: &Workbench,
    t: &Task,
    command: Option<(Uuid, CompletionCommand)>,
    observe: Option<Uuid>,
) -> Result<Task> {
    if t.inbound_device.is_some() {
        return Err(refused("remote results belong to the originating parent"));
    }
    let src = source(app, t)?;
    let replayable = command.is_some();
    let reply = app
        .0
        .chats
        .delegation_completion(t.parent, src, command, observe)?;
    let current = if replayable {
        app.0
            .chats
            .delegation_completion(t.parent, source(app, t)?, None, None)?
    } else {
        reply.clone()
    };
    let mut updated = t.clone();
    if t.completion.as_ref() != Some(&current) || t.delivered != current.native_received {
        updated = change(app, t.id, |r| {
            r.delivered = current.native_received;
            r.completion = Some(current);
        })?;
        app.wake_queue();
    }
    updated.completion = Some(reply);
    Ok(updated)
}
pub(in crate::workbench::mcp) fn observe_read(
    app: &Workbench,
    c: &Caller,
    child: Uuid,
    observed: Option<Uuid>,
    items: &[Value],
) -> Result<()> {
    let (Some(g), Some(_)) = (&c.grant, observed) else {
        return Ok(());
    };
    let gate = app.0.mcp.command_gate(child);
    let _guard = gate.blocking_lock();
    let t = {
        let db = app.0.catalog.lock().unwrap();
        if !g.active() || Policy::current(&db, g.project, g.chat) != c.policy.as_ref() {
            return Err(refused("model grant changed"));
        }
        db.model_tasks
            .get(&child)
            .filter(|t| {
                t.parent == g.chat
                    && t.project == g.project
                    && t.spec.remote.is_none()
                    && t.inbound_device.is_none()
                    && t.terminal()
            })
            .cloned()
    };
    if let Some(t) = t
        && let Some(summary) = t.summary.as_deref().filter(|v| !v.is_empty())
        && let Some(result) = app
            .0
            .chats
            .history(child)?
            .turns
            .into_iter()
            .rev()
            .find(|r| r.status != TurnStatus::Running)
        && items.iter().any(|row| {
            row["run_id"] == json!(result.id)
                && row["kind"] == "assistant"
                && row["text"] == summary
        })
    {
        sync(app, &t, None, observed)?;
    }
    Ok(())
}
#[derive(Deserialize)]
struct Command {
    command_id: Uuid,
    #[serde(flatten)]
    action: CompletionCommand,
}
fn scoped(app: &Workbench, project: Uuid, parent: Uuid, id: Uuid) -> Result<Task> {
    app.binding(project, parent)?;
    let t = super::get(app, id)?;
    if t.project != project || t.parent != parent || t.inbound_device.is_some() {
        return Err(refused("unknown delegated task of this parent"));
    }
    Ok(t)
}
pub(super) async fn model(app: &Workbench, c: &Caller, mut input: Value) -> Result<Value> {
    let id = serde_json::from_value(
        input
            .as_object_mut()
            .and_then(|m| m.remove("task_id"))
            .ok_or_else(|| refused("missing task ID"))?,
    )?;
    let q: Command = serde_json::from_value(input)?;
    let gate = app.0.mcp.command_gate(id);
    let _guard = gate.lock().await;
    authorize(&app.0.catalog.lock().unwrap(), c, false, true)?;
    let g = c.grant.as_ref().unwrap();
    Ok(sync(
        app,
        &scoped(app, g.project, g.chat, id)?,
        Some((q.command_id, q.action)),
        None,
    )?
    .view())
}
type Route = Path<(Uuid, Uuid, Uuid)>;
async fn status(
    State(app): State<Workbench>,
    Path((p, parent, id)): Route,
) -> std::result::Result<Json<Value>, ApiError> {
    let gate = app.0.mcp.command_gate(id);
    let _guard = gate.lock().await;
    Ok(Json(
        sync(&app, &scoped(&app, p, parent, id)?, None, None)?.view(),
    ))
}
async fn change_delivery(
    State(app): State<Workbench>,
    Path((p, parent, id)): Route,
    Json(q): Json<Command>,
) -> std::result::Result<Json<Value>, ApiError> {
    let gate = app.0.mcp.command_gate(id);
    let _guard = gate.lock().await;
    Ok(Json(
        sync(
            &app,
            &scoped(&app, p, parent, id)?,
            Some((q.command_id, q.action)),
            None,
        )?
        .view(),
    ))
}
pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new().route(
        "/projects/{project}/chats/{parent}/delegations/{task}",
        get(status).post(change_delivery),
    )
}
