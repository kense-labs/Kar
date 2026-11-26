//! Only the paired device can poll or cancel its delegated task. A caller's stable
//! command ID maps to a device-specific child, even after either workbench restarts.
use super::*;
use crate::workbench::{
    access::DeviceLease, http::ApiError, mcp::collaboration_tools::environment,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::get,
};
use reqwest::Method;
#[derive(Serialize, Deserialize)]
struct Reply {
    environment_id: Uuid,
    project_id: Uuid,
    command_id: Uuid,
    thread_id: Uuid,
    phase: Phase,
    summary: Option<String>,
    output: Option<Value>,
}
fn remote_id(device: Uuid, command: Uuid) -> Uuid {
    crate::chat::stable_id([
        device.as_bytes().as_slice(),
        command.as_bytes().as_slice(),
        b"peer delegated chat",
    ])
}
type Route = Path<(Uuid, Uuid, Uuid)>;
fn access(app: &Workbench, lease: &DeviceLease, env: Uuid, project: Uuid) -> Result<Uuid> {
    if !lease.active() || environment::id(app)? != env {
        return Err(refused("paired environment or device changed"));
    }
    app.0
        .access
        .peer_task_template(lease.id, project)
        .ok_or_else(|| refused("device is not granted a task template for this project"))
}
fn reply(t: &Task, env: Uuid, cmd: Uuid) -> Json<Reply> {
    Json(Reply {
        environment_id: env,
        project_id: t.project,
        command_id: cmd,
        thread_id: t.id,
        phase: t.phase,
        summary: t.summary.clone(),
        output: t.output.clone(),
    })
}
async fn status(
    State(app): State<Workbench>,
    Extension(lease): Extension<DeviceLease>,
    Path((env, p, cmd)): Route,
) -> std::result::Result<Json<Reply>, ApiError> {
    let template = access(&app, &lease, env, p)?;
    let t = super::get(&app, remote_id(lease.id, cmd))?;
    if t.inbound_device != Some(lease.id) || t.project != p || t.parent != template {
        return Err(refused("this device does not own the task").into());
    }
    Ok(reply(&t, env, cmd))
}
async fn submit(
    State(app): State<Workbench>,
    Extension(lease): Extension<DeviceLease>,
    Path((env, p, cmd)): Route,
    method: axum::http::Method,
    Json(spec): Json<Spec>,
) -> std::result::Result<Json<Reply>, ApiError> {
    let template = access(&app, &lease, env, p)?;
    if spec.remote.is_some()
        || spec.task.trim().is_empty()
        || spec.task.len() > 65536
        || spec
            .title
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 800 || s.contains(['\n', '\r', '\0']))
    {
        return Err(refused("invalid delegated work").into());
    }
    let info = app.binding(p, template)?;
    let policy = Policy::current(&app.0.catalog.lock().unwrap(), p, template).cloned();
    let scope = policy
        .as_ref()
        .and_then(|p| p.thread_tools.clone())
        .unwrap_or_default();
    let (runtime, model) = child_model(
        &info,
        &scope,
        spec.runtime.as_deref(),
        spec.model.as_deref(),
    )?;
    let directory = match spec.isolation.as_deref().unwrap_or("shared") {
        "shared" if spec.branch.is_none() => None,
        "worktree" => Some(
            policy
                .as_ref()
                .and_then(|p| p.worktree_directory())
                .ok_or_else(|| refused("remote template has no approved worktree directory"))?
                .to_owned(),
        ),
        _ => return Err(refused("invalid remote isolation").into()),
    };
    let id = remote_id(lease.id, cmd);
    let cancel = method == Method::DELETE;
    let t = app.update(|db| {
        if !lease.active() {
            return Err(refused("pairing revoked"));
        }
        if let Some(old) = db.model_tasks.get_mut(&id) {
            if old.project != p
                || old.parent != template
                || old.spec != spec
                || old.inbound_device != Some(lease.id)
            {
                return Err(refused(
                    "this command already names different delegated work",
                ));
            }
            if cancel && !old.terminal() {
                old.phase = Phase::Cancelling;
            }
            return Ok(old.clone());
        }
        if exists(&app, id)? {
            return Err(refused("child identity is already in use"));
        }
        let task = Task {
            id,
            project: p,
            parent: template,
            spec,
            runtime,
            model,
            workspace: info.workspace,
            settings: info.settings,
            approvals: info.approvals,
            directory,
            phase: if cancel {
                Phase::Cancelled
            } else {
                Phase::Admitted
            },
            summary: None,
            output: None,
            delivered: false,
            parent_turn: None,
            completion: None,
            inbound_device: Some(lease.id),
            remote_chat: None,
            remote_link: None,
            next_peer_poll_ms: 0,
        };
        db.model_tasks.insert(id, task.clone());
        Ok(task)
    })?;
    app.wake_queue();
    Ok(reply(&t, env, cmd))
}
pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new().route(
        "/peer/delegations/{environment}/{project}/{command}",
        get(status).put(submit).delete(submit),
    )
}
fn unconfirmed(app: &Workbench, t: &Task, msg: &str) -> Result<()> {
    change(app, t.id, |r| {
        r.phase = Phase::Unconfirmed;
        r.summary = Some(msg.to_owned());
    })?;
    app.wake_queue();
    Ok(())
}
pub(super) async fn advance(app: &Workbench, t: &Task) -> Result<()> {
    let remote = t
        .spec
        .remote
        .as_ref()
        .ok_or_else(|| refused("missing remote task target"))?;
    let now = jiff::Timestamp::now().as_millisecond();
    if now < t.next_peer_poll_ms {
        return Ok(());
    }
    let granted = still_granted(app, t).is_ok();
    let cancel = t.phase == Phase::Cancelling || !granted;
    let method = if cancel {
        Method::DELETE
    } else if t.remote_chat.is_none() {
        Method::PUT
    } else {
        Method::GET
    };
    let env = remote.environment_id.to_string();
    let p = remote.project_id.to_string();
    let id = t.id.to_string();
    let mut request = match environment::bound_peer_request(
        app,
        remote.environment_id,
        Some(
            t.remote_link
                .ok_or_else(|| refused("missing paired device identity"))?,
        ),
        method.clone(),
        &["peer", "delegations", &env, &p, &id],
    ) {
        Ok(req) => req,
        Err(_) => {
            return unconfirmed(
                app,
                t,
                "The paired environment link is unavailable; the remote outcome is unconfirmed",
            );
        }
    };
    if method != Method::GET {
        let mut spec = t.spec.clone();
        spec.remote = None;
        request = request.json(&spec);
    }
    change(app, t.id, |r| {
        r.next_peer_poll_ms = now.saturating_add(4_000);
        if cancel {
            r.phase = Phase::Cancelling;
        } else if r.remote_chat.is_none() {
            r.phase = Phase::Opening;
        }
    })?;
    let mut response = match request.send().await {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let http = response.status();
    if !http.is_success() {
        if http.is_server_error() || matches!(http.as_u16(), 408 | 429) {
            return Ok(());
        }
        return unconfirmed(
            app,
            t,
            &format!(
                "The remote environment returned HTTP {}; check the original paired device before reissuing work",
                http.as_u16()
            ),
        );
    }
    let mut bytes = vec![];
    loop {
        match response.chunk().await {
            Ok(Some(part)) if bytes.len().saturating_add(part.len()) <= 1_048_576 => {
                bytes.extend(part)
            }
            Ok(Some(_)) => return unconfirmed(app, t, "Remote task result exceeded one MiB"),
            Ok(None) => break,
            Err(_) => return Ok(()),
        }
    }
    let Ok(data) = serde_json::from_slice::<Reply>(&bytes) else {
        return unconfirmed(app, t, "Invalid remote delegation response");
    };
    if (data.environment_id, data.project_id, data.command_id)
        != (remote.environment_id, remote.project_id, t.id)
        || t.remote_chat.is_some_and(|id| id != data.thread_id)
    {
        return unconfirmed(app, t, "Remote delegation identity changed");
    }
    change(app, t.id, |r| {
        r.remote_chat = Some(data.thread_id);
        r.phase = match data.phase {
            Phase::Admitted | Phase::Opening => Phase::Running,
            other => other,
        };
        if cancel && matches!(data.phase, Phase::ResultAvailable | Phase::Failed) {
            r.phase = Phase::Cancelled;
        }
        r.summary = data.summary;
        r.output = data.output;
    })?;
    if matches!(
        data.phase,
        Phase::ResultAvailable | Phase::Failed | Phase::Cancelled | Phase::Unconfirmed
    ) {
        app.wake_queue();
    }
    Ok(())
}
