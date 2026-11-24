use super::browser::{self, Adjust, Display, Viewport};
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
            "/projects/{p}/chats/{c}/desktop-previews/{id}",
            axum::routing::put(desktop),
        )
        .route("/projects/{p}/previews/events", get(preview_events))
        .route(
            "/projects/{p}/previews/{id}/status",
            axum::routing::put(report_status),
        )
        .route("/projects/{p}/preview-profiles", get(profile_catalog))
        .route(
            "/projects/{p}/preview-profiles/report",
            get(report_profiles),
        )
        .route(
            "/projects/{p}/previews/{id}",
            axum::routing::delete(close_preview),
        )
        .route("/projects/{p}/previews/{id}/tunnel", get(tunnel))
        .route(
            "/projects/{p}/previews/{id}/browser",
            get(browser_status).patch(adjust),
        )
        .route(
            "/projects/{p}/previews/{id}/viewport",
            axum::routing::put(viewport),
        )
        .route("/projects/{p}/previews/{id}/screen", get(screen))
        .route(
            "/projects/{p}/preview-profiles/{profile}/clear",
            post(clear_profile),
        )
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
    profile_id: Option<String>,
    #[serde(flatten)]
    display: Display,
}
async fn open_preview(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    device: Option<axum::Extension<DeviceLease>>,
    Json(q): Json<Target>,
) -> Reply<Preview> {
    app.project(p)?;
    let profile_id = match q.profile_id {
        Some(id) => id,
        None => {
            app.0
                .terminal_preview
                .reports
                .snapshot((p, device.as_ref().map(|d| d.id)))?
                .default_profile_id
        }
    };
    browser::profile(&profile_id)?;
    q.display.validate()?;
    let state = &app.0.terminal_preview;
    Ok(Json(
        state
            .grants
            .open(
                p,
                None,
                state.find(p, q.terminal)?,
                q.target,
                device.map(|d| d.0),
                profile_id,
                q.display,
            )
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

type Device = Option<axum::Extension<DeviceLease>>;
fn grant(
    app: &Workbench,
    p: Uuid,
    id: Uuid,
    device: Device,
) -> Result<std::sync::Arc<super::preview::Grant>> {
    app.project(p)?;
    app.0
        .terminal_preview
        .grants
        .get(p, id, device.map(|d| d.id))
}
async fn browser_status(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
) -> Reply<serde_json::Value> {
    let grant = grant(&app, p, id, device)?;
    Ok(Json(
        app.0
            .terminal_preview
            .browsers
            .control(grant, None, Adjust::default())
            .await?,
    ))
}
async fn viewport(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
    Json(viewport): Json<Viewport>,
) -> Reply<serde_json::Value> {
    let grant = grant(&app, p, id, device)?;
    Ok(Json(
        app.0
            .terminal_preview
            .browsers
            .control(grant, Some(viewport), Adjust::default())
            .await?,
    ))
}
async fn adjust(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
    Json(args): Json<Adjust>,
) -> Reply<serde_json::Value> {
    let grant = grant(&app, p, id, device)?;
    Ok(Json(
        app.0
            .terminal_preview
            .browsers
            .control(grant, None, args)
            .await?,
    ))
}
async fn screen(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
) -> std::result::Result<impl axum::response::IntoResponse, ApiError> {
    let grant = grant(&app, p, id, device)?;
    let png = app.0.terminal_preview.browsers.screen(grant).await?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        png,
    ))
}
async fn clear_profile(
    State(app): State<Workbench>,
    Path((p, profile)): Path<(Uuid, String)>,
    device: Device,
) -> Reply<()> {
    app.project(p)?;
    app.0
        .terminal_preview
        .browsers
        .clear_profile(p, device.map(|d| d.id), profile)
        .await?;
    Ok(Json(()))
}

async fn report_status(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
    Json(nav): Json<super::preview::Navigation>,
) -> Reply<Preview> {
    let grant = grant(&app, p, id, device)?;
    grant.report_status(nav)?;
    Ok(Json(grant.snapshot()))
}
async fn profile_catalog(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    device: Device,
) -> Reply<browser::profiles::Catalog> {
    blocking(move || {
        app.project(p)?;
        app.0
            .terminal_preview
            .reports
            .snapshot((p, device.map(|d| d.id)))
    })
    .await
}
async fn report_profiles(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    device: Device,
    upgrade: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    use axum::extract::ws::Message;
    app.project(p)?;
    let scope = (p, device.as_ref().map(|d| d.id));
    Ok(upgrade.on_upgrade(move |mut socket| async move {
        let client = Uuid::new_v4();
        let read = async {
            while let Some(Ok(msg)) = socket.recv().await {
                match msg {
                    Message::Text(body) => {
                        let report = serde_json::from_str(&body)
                            .map_err(Error::from)
                            .and_then(|r| app.0.terminal_preview.reports.report(scope, client, r));
                        let reply = match report {
                            Ok(catalog) => serde_json::json!({"catalog":catalog}),
                            Err(e) => serde_json::json!({"error":e.to_string()}),
                        };
                        if socket
                            .send(Message::Text(reply.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => continue,
                }
            }
        };
        let revoked = async {
            match device {
                Some(d) => d.0.revoked().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {_=read=>{},_=revoked=>{}}
        app.0.terminal_preview.reports.disconnect(scope, client);
    }))
}
#[derive(Deserialize)]
struct EventCursor {
    after: Option<u64>,
}
async fn preview_events(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    device: Device,
    Query(q): Query<EventCursor>,
    headers: axum::http::HeaderMap,
) -> std::result::Result<Response, ApiError> {
    use axum::response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    };
    app.project(p)?;
    let events = app.0.terminal_preview.grants.1.clone();
    let rx = events.changed.subscribe();
    let device = device.map(|d| d.id);
    let after = q
        .after
        .or_else(|| headers.get("last-event-id")?.to_str().ok()?.parse().ok())
        .unwrap_or(*rx.borrow());
    let stream = futures_util::stream::unfold(
        (events, rx, after, std::collections::VecDeque::new()),
        move |(events, mut rx, mut after, mut queued)| async move {
            loop {
                if let Some((id, r)) = queued.pop_front() {
                    let item = Event::default()
                        .id(format!("{id}"))
                        .event("preview")
                        .json_data(r)
                        .ok()?;
                    return Some((
                        Ok::<_, std::convert::Infallible>(item),
                        (events, rx, after, queued),
                    ));
                }
                let (next, rows) = events.page(p, device, after);
                after = next;
                queued.extend(rows);
                if queued.is_empty() && rx.changed().await.is_err() {
                    return None;
                }
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopReport {
    navigation: super::preview::Navigation,
    #[serde(default)]
    display: super::browser::Display,
}
async fn desktop(
    State(app): State<Workbench>,
    Path((p, c, id)): Path<(Uuid, Uuid, Uuid)>,
    device: Option<axum::Extension<DeviceLease>>,
    Json(q): Json<DesktopReport>,
) -> Reply<Preview> {
    let device = device.ok_or_else(crate::workbench::denied)?.0;
    app.binding(p, c)?;
    if app.archived(c) {
        return Err(crate::workbench::denied().into());
    }
    Ok(Json(app.0.terminal_preview.grants.report_desktop(
        p,
        c,
        device,
        id,
        q.navigation,
        q.display,
    )?))
}
