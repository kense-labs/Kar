//! Route ordinary saves and collaborative document operations through the same writer.
#[cfg(target_os = "macos")]
mod native;

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
    io::Read,
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

#[derive(Clone, Copy)]
enum FileMode {
    Read,
    Write,
    Register,
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
    async fn document_service(
        &self,
        project: Uuid,
        path: PathBuf,
    ) -> std::result::Result<DocumentService, FileError> {
        match self
            .file_access(project, path, FileMode::Register, |_, _| Ok(()))
            .await?
        {
            Access::Document(service) => Ok(service),
            Access::Ordinary(_) => unreachable!("registration always opens a writer"),
        }
    }

    async fn file_access<T: Send + 'static>(
        &self,
        project: Uuid,
        path: PathBuf,
        mode: FileMode,
        ordinary: impl FnOnce(&Workspace, &FsPath) -> Result<T> + Send + 'static,
    ) -> std::result::Result<Access<T>, FileError> {
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let project = app.project(project)?;
            let (workspace, requested) = if matches!(mode, FileMode::Read) && path.is_absolute() {
                let root = &app.allowed(&path)?.path;
                (
                    app.workspace_at(root)?,
                    path.strip_prefix(root).expect("authorized root"),
                )
            } else {
                (app.workspace(&project)?, path.as_path())
            };
            let relative = workspace.target(requested, matches!(mode, FileMode::Write))?;
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
            if !matches!(mode, FileMode::Register)
                && !dir.join("manifest.json").try_exists().map_err(Error::Io)?
            {
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
    #[serde(default)]
    allow_missing: bool,
}

async fn document(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Query(q): Query<FileQuery>,
    request: Request,
) -> std::result::Result<Response, FileError> {
    let service = app.document_service(project, q.path).await?;
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
    let path = if let Ok(rest) = q.path.strip_prefix("~") {
        std::env::home_dir().ok_or_else(super::denied)?.join(rest)
    } else {
        q.path
    };
    let access = app
        .file_access(project, path, FileMode::Read, |workspace, relative| {
            let file = workspace.open_regular(relative)?;
            let size = file.metadata().map_err(Error::Io)?.len();
            let mut bytes = Vec::new();
            file.take(files::READ_LIMIT as u64)
                .read_to_end(&mut bytes)
                .map_err(Error::Io)?;
            files::text_preview(&bytes, size)
        })
        .await;
    if q.allow_missing
        && matches!(&access, Err(FileError::Workspace(Error::Io(e))) if e.kind()==std::io::ErrorKind::NotFound)
    {
        app.project(project)?;
        return Ok(
            Json(json!({"text":"","byte_length":0,"truncated":false,"exists":false}))
                .into_response(),
        );
    }
    let result = match access? {
        Access::Ordinary(value) => value,
        Access::Document(service) => {
            let (rev, data) = service
                .run(|s| {
                    let rev = s.state()?.revision;
                    Ok((rev, s.snapshot(rev)?))
                })
                .await?;
            let mut result = files::text_preview(&data, data.len() as u64)?;
            result["revision"] = rev.into();
            result
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
        .file_access(
            project,
            q.path,
            FileMode::Write,
            move |workspace, relative| {
                if !ordinary_bytes.starts_with(b"\xef\xbb\xbf")
                    && let Ok(mut file) = workspace.open_regular(relative)
                {
                    let mut prefix = [0; 3];
                    if file.read_exact(&mut prefix).is_ok() && prefix == *b"\xef\xbb\xbf" {
                        return workspace
                            .write_bytes(relative, &[&prefix[..], &ordinary_bytes].concat());
                    }
                }
                workspace.write_bytes(relative, &ordinary_bytes)
            },
        )
        .await?;
    let Access::Document(service) = access else {
        return Ok(Json(()).into_response());
    };
    let (Some(base_revision), Some(operation_id)) = (input.base_revision, input.operation_id)
    else {
        return Err(FileError::Precondition);
    };
    let digest = service
        .run(move |s| {
            if !bytes.starts_with(b"\xef\xbb\xbf")
                && s.snapshot(base_revision)?.starts_with(b"\xef\xbb\xbf")
            {
                s.stage(&[&b"\xef\xbb\xbf"[..], &bytes].concat())
            } else {
                s.stage(&bytes)
            }
        })
        .await?;
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
    let docs = Router::new().fallback(document);
    #[cfg(target_os = "macos")]
    let docs = docs.route("/native", axum::routing::post(native::propose));
    Router::new()
        .route("/projects/{project}/file", get(read_file).put(write_file))
        .nest("/projects/{project}/documents", docs)
}
