use super::{
    Project, ProjectChat, Workbench,
    search::{Hit, SearchKind},
};
use crate::{
    Error, Result,
    chat::{ChatFeedSnapshot, ChatTurn},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use std::{io, path::PathBuf};

use uuid::Uuid;

pub(super) struct ApiError(Error);
impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::Io(e) if e.kind() == io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            Error::Io(e) if e.kind() == io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            Error::Busy => StatusCode::CONFLICT,
            Error::Refused(reason) if reason == crate::transport::AbortRequest::STALE => {
                StatusCode::CONFLICT
            }
            Error::InvalidCursor | Error::Refused(_) | Error::Unsupported(_) => {
                StatusCode::BAD_REQUEST
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}
type Reply<T> = std::result::Result<Json<T>, ApiError>;

pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Reply<T> {
    Ok(Json(
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| Error::Closed(e.to_string()))??,
    ))
}

pub(super) fn router(app: Workbench, token: String) -> Result<Router> {
    if token.is_empty() {
        return Err(Error::Refused("empty workbench token".into()));
    }
    let lazy_start = if tokio::runtime::Handle::try_current().is_ok() {
        app.start_services()?;
        None
    } else {
        Some(app.clone())
    };
    let hooks = super::workflows::hooks()
        .merge(super::assets::tickets())
        .with_state(app.clone());
    let auth = super::access::Auth::new(app.clone(), token);
    let access = super::access::routes(app.clone(), auth.clone());
    let router = Router::new()
        .route("/projects", get(projects).post(register))
        .route("/projects/{project}/chats", get(chats).post(create_chat))
        .route("/projects/{project}/models/{runtime}", get(models))
        .route("/projects/{project}/chats/{chat}/snapshot", get(snapshot))
        .route("/projects/{project}/chats/{chat}/events", get(events))
        .route("/projects/{project}/chats/{chat}/messages", post(send))
        .route(
            "/projects/{project}/chats/{chat}/handoffs",
            get(handoffs).post(handoff),
        )
        .route("/projects/{project}/chats/{chat}/archive", put(archive))
        .merge(super::agent_sessions::routes())
        .merge(super::chat_controls::routes())
        .merge(super::thread_find::routes())
        .merge(super::thread_state::routes())
        .merge(super::interactions::routes())
        .merge(super::force_stop::routes())
        .merge(super::assets::routes())
        .merge(super::workflows::routes())
        .merge(super::devices::routes())
        .merge(super::observability::routes())
        .merge(super::documents::routes())
        .merge(super::forge::routes())
        .merge(super::git::routes())
        .merge(super::git_actions::routes())
        .merge(super::files::routes())
        .merge(super::terminal_preview::routes())
        .merge(super::task_folders::routes())
        .merge(super::project_lifecycle::routes())
        .route("/projects/{project}/search", get(search_project))
        .route("/search/chats", get(search_chats))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(app.clone(),super::observability::observe))
        .with_state(app)
        .layer(middleware::from_fn_with_state(
            auth,
            super::access::authorize,
        ))
        .merge(access)
        .merge(hooks);
    if let Some(app) = lazy_start {
        return Ok(router.layer(middleware::from_fn(
            move |req: axum::extract::Request, next: middleware::Next| {
                let app = app.clone();
                async move {
                    if let Err(e) = app.start_services() {
                        return ApiError(e).into_response();
                    }
                    next.run(req).await
                }
            },
        )));
    }
    Ok(router)
}

async fn projects(State(app): State<Workbench>) -> Json<Vec<Project>> {
    Json(app.projects())
}
#[derive(Deserialize)]
struct Register {
    root: PathBuf,
    name: String,
}
async fn register(State(app): State<Workbench>, Json(p): Json<Register>) -> Reply<Project> {
    blocking(move || app.register(&p.root, &p.name)).await
}
#[derive(Deserialize, Default)]
struct ListChats {
    #[serde(default)]
    include_archived: bool,
}
async fn chats(
    State(app): State<Workbench>,
    Path(id): Path<Uuid>,
    Query(q): Query<ListChats>,
) -> Reply<Vec<ProjectChat>> {
    blocking(move || app.chats(id, q.include_archived)).await
}
#[derive(Deserialize)]
struct NewChat {
    id: Option<Uuid>,
    runtime: String,
    model: Option<String>,
    #[serde(default)]
    approvals: crate::ApprovalMode,
    #[serde(default)]
    settings: crate::SessionSettings,
}
async fn create_chat(
    State(app): State<Workbench>,
    Path(id): Path<Uuid>,
    Json(p): Json<NewChat>,
) -> Reply<ProjectChat> {
    Ok(Json(
        app.create_chat_with_settings(
            id,
            p.id.unwrap_or_else(Uuid::new_v4),
            &p.runtime,
            p.model,
            p.approvals,
            p.settings,
        )
        .await?,
    ))
}
async fn models(
    State(app): State<Workbench>,
    Path((project, runtime)): Path<(Uuid, String)>,
) -> Reply<crate::forms::ModelListing> {
    let project = app.project(project)?;
    app.workspace(&project)?;
    let rt = crate::runtime(&runtime)?;
    let mut opts = crate::SessionOptions::new(project.root);
    opts.remove_env("KAR_WORKBENCH_TOKEN")?;
    Ok(Json(rt.form_model_listing(&opts, Vec::new()).await))
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    attachments: Vec<Uuid>,
    id: Uuid,
    text: String,
}
async fn send(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(m): Json<Message>,
) -> Reply<ChatTurn> {
    Ok(Json(
        app.send_attached(p, c, m.id, m.text, m.attachments).await?,
    ))
}
async fn handoffs(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> Reply<Vec<crate::chat::Handoff>> {
    blocking(move || app.handoffs(p, c)).await
}
async fn handoff(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(request): Json<crate::chat::HandoffRequest>,
) -> Reply<crate::chat::Handoff> {
    Ok(Json(app.handoff(p, c, request).await?))
}
#[derive(Deserialize)]
struct Archive {
    archived: bool,
}
async fn archive(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(a): Json<Archive>,
) -> Reply<ProjectChat> {
    Ok(Json(app.set_archived(p, c, a.archived).await?))
}
async fn snapshot(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> Reply<ChatFeedSnapshot> {
    blocking(move || {
        app.binding(p, c)?;
        app.0.chats.feed_snapshot(c)
    })
    .await
}
async fn events(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let store = app.0.chats.clone();
    if let Err(e) = blocking(move || app.binding(p, c)).await {
        return e.into_response();
    }
    crate::chat::feed::event_response(store, c, headers, uri).await
}
#[derive(Deserialize)]
struct Search {
    q: String,
    kind: Option<SearchKind>,
    limit: Option<usize>,
}
async fn search_project(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    Query(q): Query<Search>,
) -> Reply<Vec<Hit>> {
    blocking(move || {
        app.search(
            Some(p),
            q.kind.unwrap_or(SearchKind::Paths),
            &q.q,
            q.limit.unwrap_or(50),
        )
    })
    .await
}
async fn search_chats(State(app): State<Workbench>, Query(q): Query<Search>) -> Reply<Vec<Hit>> {
    blocking(move || app.search(None, SearchKind::Messages, &q.q, q.limit.unwrap_or(50))).await
}
