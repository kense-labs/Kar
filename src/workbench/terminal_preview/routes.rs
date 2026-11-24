use super::{
    InputReceipt, Meta, Page, Port, Status, TerminalEvent, Workbench, preview::Preview, process,
};
use crate::workbench::access::DeviceLease;
use crate::{Error, Result, workbench::http::ApiError};
use axum::{
    Json, Router,
    extract::{Path, Query, State, WebSocketUpgrade},
    response::Response,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

type Reply<T> = std::result::Result<Json<T>, ApiError>;
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Reply<T> {
    Ok(Json(
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| Error::Closed(e.to_string()))??,
    ))
}

pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{p}/terminals", get(list).post(create))
        .route("/projects/{p}/terminals/{id}", get(info).delete(close))
        .route("/projects/{p}/terminals/{id}/output", get(output))
        .route("/projects/{p}/terminals/{id}/input", post(input))
        .route("/projects/{p}/terminals/{id}/clear", post(clear))
        .route(
            "/projects/{p}/terminals/{id}/size",
            axum::routing::put(resize),
        )
        .route("/projects/{p}/terminals/{id}/ports", get(ports))
        .route(
            "/projects/{p}/terminals/{id}/ports/stop",
            post(stop_listener),
        )
        .route("/projects/{p}/previews", get(previews).post(open_preview))
        .route(
            "/projects/{p}/previews/{id}",
            axum::routing::delete(close_preview),
        )
        .route("/projects/{p}/previews/{id}/tunnel", get(tunnel))
}

#[derive(Deserialize)]
struct New {
    worktree: Option<std::path::PathBuf>,
    id: Uuid,
    cols: Option<u16>,
    rows: Option<u16>,
}
async fn create(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    Json(q): Json<New>,
) -> Reply<Meta> {
    blocking(move || {
        app.new_terminal(
            p,
            q.id,
            q.worktree,
            q.cols.unwrap_or(120),
            q.rows.unwrap_or(30),
        )
    })
    .await
}
async fn list(State(app): State<Workbench>, Path(p): Path<Uuid>) -> Reply<Vec<Meta>> {
    blocking(move || {
        app.project(p)?;
        let mut v: Vec<_> = app
            .0
            .terminal_preview
            .terminals
            .lock()
            .unwrap()
            .values()
            .map(|t| t.snapshot())
            .filter(|m| m.project == p)
            .collect();
        v.sort_by_key(|m| m.id);
        Ok(v)
    })
    .await
}
async fn info(State(app): State<Workbench>, Path((p, id)): Path<(Uuid, Uuid)>) -> Reply<Meta> {
    blocking(move || {
        app.project(p)?;
        Ok(app.0.terminal_preview.find(p, id)?.snapshot())
    })
    .await
}
async fn close(State(app): State<Workbench>, Path((p, id)): Path<(Uuid, Uuid)>) -> Reply<Meta> {
    blocking(move || {
        app.project(p)?;
        app.0.terminal_preview.find(p, id)?.close()
    })
    .await
}
#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: u64,
    #[serde(default)]
    wait_ms: u64,
}
async fn output(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Query(q): Query<Cursor>,
) -> Reply<Page> {
    app.project(p)?;
    let terminal = app.0.terminal_preview.find(p, id)?;
    let mut changed = terminal.changed.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(q.wait_ms.min(30_000));
    loop {
        let t = terminal.clone();
        let result = blocking(move || t.page(q.after)).await?;
        if result.0.next != q.after
            || !matches!(
                result.0.terminal.status,
                Status::Running | Status::Starting | Status::Closing
            )
            || tokio::time::Instant::now() >= deadline
        {
            return Ok(result);
        }
        if tokio::time::timeout_at(deadline, changed.changed())
            .await
            .is_err()
        {
            return Ok(result);
        }
    }
}
async fn clear(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Reply<TerminalEvent> {
    blocking(move || {
        app.project(p)?;
        app.0.terminal_preview.find(p, id)?.clear()
    })
    .await
}

#[derive(Deserialize)]
struct Input {
    id: Uuid,
    data_base64: String,
}
async fn input(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Json(q): Json<Input>,
) -> Reply<InputReceipt> {
    blocking(move || {
        app.project(p)?;
        let bytes = STANDARD
            .decode(q.data_base64)
            .map_err(|_| Error::Refused("invalid terminal input encoding".into()))?;
        app.0.terminal_preview.find(p, id)?.input(q.id, &bytes)
    })
    .await
}
#[derive(Deserialize)]
struct Size {
    cols: u16,
    rows: u16,
}
async fn resize(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Json(q): Json<Size>,
) -> Reply<Meta> {
    blocking(move || {
        app.project(p)?;
        app.0.terminal_preview.find(p, id)?.resize(q.cols, q.rows)
    })
    .await
}
async fn ports(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Reply<Vec<Port>> {
    blocking(move || {
        app.project(p)?;
        let term = app.0.terminal_preview.find(p, id)?.snapshot();
        if matches!(term.status, Status::Running)
            && let Some(root) = term.process
        {
            return process::ports(root);
        }
        Ok(vec![])
    })
    .await
}
#[derive(Deserialize)]
struct StopListener {
    pid: u32,
    port: u16,
    born: Option<u64>,
}
async fn stop_listener(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Json(q): Json<StopListener>,
) -> Reply<()> {
    blocking(move || {
        app.project(p)?;
        let term = app.0.terminal_preview.find(p, id)?.snapshot();
        let root = term
            .process
            .filter(|_| matches!(term.status, Status::Running))
            .ok_or_else(super::missing)?;
        process::stop_listener(root, q.pid, q.port, q.born)
    })
    .await
}
#[derive(Deserialize)]
struct Target {
    terminal: Uuid,
    target: Port,
}
async fn open_preview(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    device: Option<axum::Extension<DeviceLease>>,
    Json(q): Json<Target>,
) -> Reply<Preview> {
    app.project(p)?;
    let state = &app.0.terminal_preview;
    Ok(Json(
        state
            .grants
            .open(p, state.find(p, q.terminal)?, q.target, device.map(|d| d.0))
            .await?,
    ))
}
async fn previews(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    device: Option<axum::Extension<DeviceLease>>,
) -> Reply<Vec<Preview>> {
    app.project(p)?;
    Ok(Json(
        app.0.terminal_preview.grants.list(p, device.map(|d| d.id)),
    ))
}
async fn close_preview(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Option<axum::Extension<DeviceLease>>,
) -> Reply<()> {
    app.project(p)?;
    app.0
        .terminal_preview
        .grants
        .close(p, id, device.map(|d| d.id))?;
    Ok(Json(()))
}
async fn tunnel(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Option<axum::Extension<DeviceLease>>,
    upgrade: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    app.project(p)?;
    let grant = app
        .0
        .terminal_preview
        .grants
        .get(p, id, device.map(|d| d.id))?;
    Ok(upgrade.on_upgrade(move |socket| grant.websocket(socket)))
}
