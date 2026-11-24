//! Multi-repository project discovery and managed task workspaces.
mod folders;
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
use uuid::Uuid;

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
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
