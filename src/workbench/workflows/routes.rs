use super::super::http::ApiError;
use super::*;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as Param, State},
    routing::{get, post, put},
};

pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .merge(super::views::routes())
        .route("/projects/{project}/workflows", get(list))
        .route(
            "/projects/{project}/workflows/{workflow}/webhook/rules",
            get(webhook::rules_get).put(webhook::rules_put),
        )
        .route(
            "/projects/{project}/workflows/{workflow}/webhook/rotate",
            post(webhook::rotate),
        )
        .route(
            "/projects/{project}/workflows/{workflow}/deliveries",
            get(webhook::list),
        )
        .route(
            "/projects/{project}/workflows/{workflow}/deliveries/{delivery}",
            get(webhook::detail),
        )
        .route(
            "/projects/{project}/workflows/{workflow}",
            put(save).delete(super::views::remove),
        )
        .route(
            "/projects/{project}/workflows/{workflow}/runs",
            get(runs).post(trigger),
        )
        .route("/projects/{project}/workflow-runs/{run}", get(run))
        .route(
            "/projects/{project}/workflow-runs/{run}/actions",
            post(action),
        )
}
pub(in crate::workbench) fn hooks() -> Router<Workbench> {
    Router::new()
        .route(
            "/hooks/workflows/{project}/{workflow}",
            post(webhook::signed),
        )
        .route(
            "/hooks/workflows/{project}/{workflow}/{token}",
            post(webhook::tokenized),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024))
}

type Reply<T> = std::result::Result<Json<T>, ApiError>;
async fn call<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Reply<T> {
    Ok(Json(
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| Error::Closed(e.to_string()))??,
    ))
}
#[derive(Deserialize)]
struct Save {
    #[serde(flatten)]
    definition: Definition,
    webhook_secret: Option<String>,
}
async fn save(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, Uuid)>,
    Json(req): Json<Save>,
) -> Reply<Workflow> {
    call(move || app.save_workflow(p, id, req.definition, req.webhook_secret)).await
}
async fn list(State(app): State<Workbench>, Param(p): Param<Uuid>) -> Reply<Vec<Workflow>> {
    call(move || app.workflows(p)).await
}
async fn runs(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, Uuid)>,
) -> Reply<Vec<Run>> {
    call(move || app.workflow_runs(p, id)).await
}
async fn run(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, Uuid)>) -> Reply<Run> {
    call(move || app.workflow_run(p, id)).await
}
#[derive(Deserialize)]
struct TriggerRequest {
    operation: Uuid,
    #[serde(default)]
    input: Value,
}
async fn trigger(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, Uuid)>,
    Json(req): Json<TriggerRequest>,
) -> Reply<Run> {
    call(move || app.trigger_workflow(p, id, req.operation, req.input)).await
}
#[derive(Deserialize)]
struct ActionRequest {
    operation: Uuid,
    #[serde(flatten)]
    action: Action,
}
async fn action(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, Uuid)>,
    Json(req): Json<ActionRequest>,
) -> Reply<Run> {
    call(move || app.workflow_action(p, id, req.operation, req.action)).await
}
