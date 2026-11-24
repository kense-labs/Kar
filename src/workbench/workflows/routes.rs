use super::super::http::ApiError;
use super::*;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as Param, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use hmac::{Hmac, KeyInit, Mac};
use sha2_mac::Sha256;

pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{project}/workflows", get(list))
        .route("/projects/{project}/workflows/{workflow}", put(save))
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
        .route("/hooks/workflows/{project}/{workflow}", post(hook))
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

#[derive(Deserialize)]
struct Delivery {
    id: String,
    #[serde(default)]
    input: Value,
}

fn signature(headers: &HeaderMap) -> Option<[u8; 32]> {
    let s = headers
        .get("x-kar-signature")?
        .to_str()
        .ok()?
        .strip_prefix("sha256=")?;
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut tag = [0u8; 32];
    for (i, b) in tag.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(tag)
}
async fn hook(
    State(app): State<Workbench>,
    Param((project, id)): Param<(Uuid, Uuid)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(tag) = signature(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    call(move || {
        app.project(project)?;
        let e = &app.0.workflows;
        let run = e.update(|d| {
            let w = d
                .workflows
                .get(&id)
                .filter(|w| {
                    w.project == project && matches!(w.definition.schedule, Schedule::Webhook)
                })
                .ok_or_else(missing)?
                .clone();
            let secret = d.secrets.get(&id).ok_or_else(missing)?;
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .expect("HMAC accepts variable length keys");
            mac.update(&body);
            mac.verify_slice(&tag).map_err(|_| {
                io::Error::new(io::ErrorKind::PermissionDenied, "invalid webhook signature")
            })?;
            let delivery: Delivery = serde_json::from_slice(&body)
                .map_err(|e| Error::Refused(format!("invalid webhook delivery: {e}")))?;
            if delivery.id.trim().is_empty() || delivery.id.len() > 256 {
                return Err(Error::Refused("invalid delivery identifier".into()));
            }
            insert_run(
                d,
                &w,
                Trigger::Webhook(delivery.id),
                delivery.input,
                e.clock.now_ms(),
            )
        })?;
        // A hook credential can trigger this definition, never read workflow state.
        Ok(json!({"run_id":run.id}))
    })
    .await
    .into_response()
}
