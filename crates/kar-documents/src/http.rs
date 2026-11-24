//! HTTP operations shared by the single-document server and Kar workbench.
use crate::store::{Command, DocumentState, DocumentStore, Receipt, Record, Rejection, Status};
use anyhow::Result;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::watch,
    time::{Instant, timeout_at},
};

#[derive(Clone)]
pub struct DocumentService {
    store: Arc<Mutex<DocumentStore>>,
    changed: watch::Sender<u64>,
}

pub struct Failure(pub anyhow::Error);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let code = if self.0.is::<Rejection>() {
            StatusCode::CONFLICT
        } else if self.0.is::<std::io::Error>() {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        };
        let mut body = json!({"error":self.0.to_string()});
        if let Some(Rejection::StaleResolution { current, .. }) = self.0.downcast_ref::<Rejection>()
        {
            body["current_revision"] = json!(current);
        }
        (code, Json(body)).into_response()
    }
}

impl DocumentService {
    pub fn new(store: DocumentStore) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            store: Arc::new(Mutex::new(store)),
            changed,
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/state", get(state))
            .route(
                "/proposals",
                post(stage).layer(DefaultBodyLimit::max(128 * 1024 * 1024)),
            )
            .route("/proposals/{digest}", get(proposal))
            .route("/revisions/{revision}", get(snapshot))
            .route("/events", get(events))
            .route("/operations", post(submit))
            .with_state(self.clone())
    }

    pub async fn submit(&self, command: Command) -> Result<Receipt, Failure> {
        let receipt = self.run(move |s| s.submit(command)).await?;
        self.changed.send_replace(receipt.cursor);
        Ok(receipt)
    }

    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut DocumentStore) -> Result<T> + Send + 'static,
    ) -> Result<T, Failure> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| anyhow::anyhow!("document lock poisoned"))?;
            f(&mut store)
        })
        .await
        .map_err(|e| Failure(e.into()))?
        .map_err(Failure)
    }
}

async fn state(State(app): State<DocumentService>) -> Result<Json<DocumentState>, Failure> {
    app.run(DocumentStore::state).await.map(Json)
}

async fn snapshot(
    State(app): State<DocumentService>,
    Path(rev): Path<u64>,
) -> Result<Response, Failure> {
    let bytes = app.run(move |s| s.snapshot(rev)).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn stage(
    State(app): State<DocumentService>,
    bytes: Bytes,
) -> Result<Json<serde_json::Value>, Failure> {
    let digest = app.run(move |s| s.stage(&bytes)).await?;
    Ok(Json(json!({"sha256":digest})))
}
async fn proposal(
    State(app): State<DocumentService>,
    Path(digest): Path<String>,
) -> Result<Response, Failure> {
    let bytes = app.run(move |s| s.proposal(&digest)).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn submit(
    State(app): State<DocumentService>,
    Json(command): Json<Command>,
) -> Result<(StatusCode, Json<Receipt>), Failure> {
    let receipt = app.submit(command).await?;
    Ok(receipt_response(receipt))
}

pub fn receipt_response(receipt: Receipt) -> (StatusCode, Json<Receipt>) {
    let code = if receipt.status == Status::Conflict {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    (code, Json(receipt))
}

#[derive(Deserialize, Default)]
struct Events {
    #[serde(default)]
    after: u64,
    #[serde(default)]
    wait_ms: u64,
}

async fn events(
    State(app): State<DocumentService>,
    Query(args): Query<Events>,
) -> Result<Json<Vec<Record>>, Failure> {
    let mut changed = app.changed.subscribe();
    let deadline = Instant::now() + Duration::from_millis(args.wait_ms.min(30_000));
    loop {
        let records = app.run(move |s| s.events(args.after)).await?;
        if !records.is_empty() || Instant::now() >= deadline {
            return Ok(Json(records));
        }
        // Check the working file periodically for saves made by an external editor.
        let until = deadline.min(Instant::now() + Duration::from_millis(500));
        let _ = timeout_at(until, changed.changed()).await;
    }
}
