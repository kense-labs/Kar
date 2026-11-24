//! Route ordinary saves and collaborative document operations through the same writer.
use super::{
    Workbench,
    files::{self, Workspace},
    http::ApiError,
};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{Extensions, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use kar_documents::{
    http::{DocumentService, Failure, receipt_response},
    store::{Action, Command, DocumentStore, WorkingFile},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::Mutex,
};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Documents {
    // Only registered documents retain a writer. Ordinary files never enter the cache.
    writers: [Mutex<HashMap<PathBuf, DocumentService>>; 32],
}

struct WorkingPath {
    workspace: Workspace,
    relative: PathBuf,
}

impl WorkingFile for WorkingPath {
    fn read(&self) -> anyhow::Result<Vec<u8>> {
        Ok(self
            .workspace
            .read_bytes(&self.relative, 128 * 1024 * 1024)?)
    }
    fn publish(&self, bytes: &[u8]) -> anyhow::Result<()> {
        Ok(self.workspace.write_bytes(&self.relative, bytes)?)
    }
}

enum Access<T> {
    Ordinary(T),
    Document(DocumentService),
}

enum FileError {
    Workspace(Error),
    Document(Failure),
    Precondition,
}
impl From<Error> for FileError {
    fn from(e: Error) -> Self {
        Self::Workspace(e)
    }
}
impl From<Failure> for FileError {
    fn from(e: Failure) -> Self {
        Self::Document(e)
    }
}
impl IntoResponse for FileError {
    fn into_response(self) -> Response {
        match self {
            Self::Workspace(e) => ApiError::from(e).into_response(),
            Self::Document(e) => e.into_response(),
            Self::Precondition => (
                StatusCode::PRECONDITION_REQUIRED,
                Json(json!({"error":"collaborative saves require base_revision and operation_id"})),
            )
                .into_response(),
        }
    }
}

impl Workbench {
    async fn file_access<T: Send + 'static>(
        &self,
        project: Uuid,
        path: PathBuf,
        register: bool,
        create: bool,
        ordinary: impl FnOnce(&Workspace, &FsPath) -> Result<T> + Send + 'static,
    ) -> std::result::Result<Access<T>, FileError> {
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let workspace = app.workspace(&app.project(project)?)?;
            let relative = workspace.target(&path, create)?;
            let identity = workspace.root.join(&relative);
            let digest = Sha256::digest(identity.as_os_str().as_encoded_bytes());
            let dir = app.0.state.join("documents").join(format!("{digest:x}"));
            let mut writers = app.0.documents.writers
                [usize::from(digest[0]) % app.0.documents.writers.len()]
            .lock()
            .map_err(|_| Error::Closed("document registry lock poisoned".into()))?;
            if let Some(writer) = writers.get(&identity) {
                return Ok(Access::Document(writer.clone()));
            }
            if !register && !dir.join("manifest.json").try_exists().map_err(Error::Io)? {
                // Serialize this save with first-time registration of this same file.
                return Ok(Access::Ordinary(ordinary(&workspace, &relative)?));
            }
            let file = Box::new(WorkingPath {
                workspace,
                relative,
            });
            let store = DocumentStore::open_with_file(&dir, &identity, file).map_err(Failure)?;
            let service = DocumentService::new(store);
            writers.insert(identity, service.clone());
            Ok(Access::Document(service))
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
}

#[derive(Deserialize)]
struct FileQuery {
    path: PathBuf,
}

async fn document(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Query(q): Query<FileQuery>,
    request: Request,
) -> std::result::Result<Response, FileError> {
    let access = app
        .file_access(project, q.path, true, false, |_, _| Ok(()))
        .await?;
    let Access::Document(service) = access else {
        unreachable!("registration always opens a writer")
    };
    // Axum has stripped the project prefix. The inner router has its own path parameters.
    let (mut parts, body) = request.into_parts();
    parts.extensions = Extensions::new();
    let request = Request::from_parts(parts, body);
    Ok(service
        .router()
        .oneshot(request)
        .await
        .expect("infallible document router"))
}

async fn read_file(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Query(q): Query<FileQuery>,
) -> std::result::Result<Response, FileError> {
    let access = app
        .file_access(project, q.path, false, false, |workspace, relative| {
            files::preview(workspace.read_bytes(relative, files::READ_LIMIT)?)
        })
        .await?;
    let result = match access {
        Access::Ordinary(text) => json!({"text":text}),
        Access::Document(service) => {
            let (rev, data) = service
                .run(|s| {
                    let rev = s.state()?.revision;
                    Ok((rev, s.snapshot(rev)?))
                })
                .await?;
            json!({"text":files::preview(data)?, "revision":rev})
        }
    };
    Ok(Json(result).into_response())
}

#[derive(Deserialize)]
struct SaveText {
    text: String,
    base_revision: Option<u64>,
    operation_id: Option<String>,
}

async fn write_file(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Query(q): Query<FileQuery>,
    Json(input): Json<SaveText>,
) -> std::result::Result<Response, FileError> {
    if input.text.contains('\0') {
        return Err(Error::Refused("expected text content".into()).into());
    }
    let bytes = axum::body::Bytes::from(input.text);
    let ordinary_bytes = bytes.clone();
    let access = app
        .file_access(project, q.path, false, true, move |workspace, relative| {
            workspace.write_bytes(relative, &ordinary_bytes)
        })
        .await?;
    let Access::Document(service) = access else {
        return Ok(Json(()).into_response());
    };
    let (Some(base_revision), Some(operation_id)) = (input.base_revision, input.operation_id)
    else {
        return Err(FileError::Precondition);
    };
    let digest = service.run(move |s| s.stage(&bytes)).await?;
    let receipt = service
        .submit(Command {
            author: "owner".into(),
            operation_id,
            base_revision,
            action: Action::Save { sha256: digest },
        })
        .await?;
    Ok(receipt_response(receipt).into_response())
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{project}/file", get(read_file).put(write_file))
        .nest(
            "/projects/{project}/documents",
            Router::new().fallback(document),
        )
}
