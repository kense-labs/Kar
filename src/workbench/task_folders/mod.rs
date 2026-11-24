//! Multi-repository project discovery and managed task workspaces.
pub(super) mod checkpoints;
mod folders;
mod setup;
mod task;
use super::{
    Workbench,
    http::{ApiError, blocking},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
pub(super) use folders::Tracked;
use serde::Deserialize;
use std::path::PathBuf;
pub(super) use task::{Store, single};
use uuid::Uuid;

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{p}/tasks/{id}/folder",
            get(task_info).put(create_task),
        )
        .route(
            "/projects/{p}/tasks/{id}/folder/repositories",
            axum::routing::post(add_repository),
        )
        .route("/projects/{p}/folders/containing", get(containing))
        .route("/projects/{p}/tasks/{id}/setup/events", get(progress))
        .route(
            "/projects/{p}/tasks/{id}/setup/cancel",
            axum::routing::post(cancel),
        )
        .route("/projects/{p}/folders", get(list).put(track))
        .route("/projects/{p}/folders/scan", get(scan))
}
async fn list(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
) -> Result<Json<Vec<folders::Folder>>, ApiError> {
    blocking(move || folders::list(&app, &app.project(p)?)).await
}
#[derive(Deserialize)]
struct ScanInput {
    root: Option<PathBuf>,
}
async fn scan(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<ScanInput>,
) -> Result<Json<folders::Scan>, ApiError> {
    blocking(move || {
        let project = app.project(p)?;
        folders::scan(&app, &project, q.root.as_deref().unwrap_or(&project.root))
    })
    .await
}
async fn track(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    Json(rows): Json<Vec<Tracked>>,
) -> Result<Json<Vec<folders::Folder>>, ApiError> {
    blocking(move || folders::track(&app, p, rows)).await
}

async fn task_info(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Option<task::Task>>, ApiError> {
    blocking(move || {
        app.project(p)?;
        Ok(app.0.task_folders.get(p, id)?.map(|r| r.task))
    })
    .await
}
async fn create_task(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Json(q): Json<task::Create>,
) -> Result<Json<task::Task>, ApiError> {
    blocking(move || task::create(&app, p, id, q)).await
}
#[derive(Deserialize)]
struct Add {
    repository_path: PathBuf,
}
async fn add_repository(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Json(q): Json<Add>,
) -> Result<Json<task::Task>, ApiError> {
    blocking(move || task::add(&app, p, id, q.repository_path)).await
}
#[derive(Deserialize)]
struct Find {
    path: PathBuf,
}
async fn containing(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<Find>,
) -> Result<Json<Option<task::Task>>, ApiError> {
    blocking(move || {
        app.project(p)?;
        app.allowed(&q.path)?;
        app.0.task_folders.find(p, &q.path)
    })
    .await
}
async fn cancel(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    app.project(p)?;
    let life = app.0.task_folders.lifecycle(p, id)?;
    let mut changed = life.updates.subscribe();
    if !app.0.task_folders.cancel(p, id)? {
        return Ok(Json(serde_json::json!({"cancelled":false})));
    }
    loop {
        if let Some(snapshot) = changed.borrow_and_update().as_ref()
            && snapshot.phase != "running"
        {
            if snapshot.phase != "cancelled" {
                return Err(crate::Error::Refused(
                    "worktree setup could not finish cancellation; workspace was preserved".into(),
                )
                .into());
            }
            break;
        }
        changed
            .changed()
            .await
            .map_err(|_| crate::Error::Closed("setup tracker closed".into()))?;
    }
    Ok(Json(serde_json::json!({"cancelled":true})))
}
async fn progress(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    };
    app.project(p)?;
    let life = app.0.task_folders.lifecycle(p, id)?;
    let mut rx = life.updates.subscribe();
    let seen = rx.borrow_and_update().clone();
    let current = seen.or(app.0.task_folders.get(p, id)?.map(|r| r.progress));
    let stream =
        futures_util::stream::unfold((rx, Some(current)), |(mut rx, initial)| async move {
            let item = match initial {
                Some(value) => value,
                None => {
                    rx.changed().await.ok()?;
                    rx.borrow_and_update().clone()
                }
            };
            let e = Event::default()
                .event("worktree_setup")
                .json_data(item)
                .ok()?;
            Some((Ok::<_, std::convert::Infallible>(e), (rx, None)))
        });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
