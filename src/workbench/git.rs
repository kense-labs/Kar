//! Project-scoped HTTP access to the same repository operations used by the Kar CLI.
use super::{Project, Workbench, denied, http::ApiError};
use crate::{
    Error, Result,
    workbench_git::{Repository, Status},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    path::{Path as FsPath, PathBuf},
    time::Duration,
};
use uuid::Uuid;

impl Workbench {
    pub(super) fn repository(&self, project: Uuid) -> Result<Repository> {
        let p = self.project(project)?;
        self.workspace(&p)?;
        let repo = Repository::open(&p.root)?;
        if repo.root() != p.root {
            return Err(Error::Refused(
                "project must be registered at its checkout root".into(),
            ));
        }
        Ok(repo)
    }
    pub(super) fn destination(&self, path: &FsPath) -> Result<PathBuf> {
        if !path.is_absolute() {
            return Err(Error::Refused("expected an absolute destination".into()));
        }
        let parent = path.parent().ok_or_else(denied)?.canonicalize()?;
        let dst = parent.join(path.file_name().ok_or_else(denied)?);
        self.allowed(&dst)?;
        if std::fs::symlink_metadata(&dst).is_ok_and(|m| m.is_symlink()) {
            return Err(denied());
        }
        Ok(dst)
    }
}
#[derive(Deserialize)]
struct CloneProject {
    source: String,
    directory: PathBuf,
    name: String,
}
async fn clone_project(
    State(app): State<Workbench>,
    Json(p): Json<CloneProject>,
) -> std::result::Result<Json<Project>, ApiError> {
    super::http::blocking(move || {
        let dst = app.destination(&p.directory)?;
        if p.name.trim().is_empty() {
            return Err(Error::Refused("project name required".into()));
        }
        let source = if FsPath::new(&p.source).is_absolute() {
            let source = FsPath::new(&p.source).canonicalize()?;
            app.allowed(&source)?;
            source.into_os_string()
        } else {
            let url = url::Url::parse(&p.source).map_err(|_| {
                Error::Refused(
                    "expected an HTTPS or SSH repository URL or an authorized local path".into(),
                )
            })?;
            if !matches!(url.scheme(), "https" | "ssh")
                || url.host().is_none()
                || url.password().is_some()
            {
                return Err(Error::Refused(
                    "expected an HTTPS or SSH repository URL without embedded credentials".into(),
                ));
            }
            p.source.into()
        };
        let repo = Repository::clone_into(&source, &dst)?;
        app.register(repo.root(), &p.name)
    })
    .await
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Action {
    Stage {
        paths: Vec<PathBuf>,
    },
    Commit {
        message: String,
    },
    Push,
    Pull,
    WorktreeAdd {
        id: Uuid,
        directory: PathBuf,
        branch: String,
        start: String,
    },
    WorktreeRemove {
        id: Uuid,
    },
}
async fn change(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(op): Json<Action>,
) -> std::result::Result<Json<Value>, ApiError> {
    super::http::blocking(move || {
        let repo = app.repository(project)?;
        match op {
            Action::Stage { paths } => repo.stage(&paths)?,
            Action::Commit { message } => return Ok(json!({"commit":repo.commit(&message)?})),
            Action::Push => repo.push()?,
            Action::Pull => return Ok(json!({"commit":repo.pull()?})),
            Action::WorktreeAdd {
                id,
                directory,
                branch,
                start,
            } => {
                return Ok(json!(repo.create_worktree(
                    id,
                    &app.destination(&directory)?,
                    &branch,
                    &start
                )?));
            }
            Action::WorktreeRemove { id } => repo.remove_worktree_if(id, |path| {
                if app.destination(path)? != path {
                    return Err(denied());
                }
                Ok(())
            })?,
        }
        Ok(json!(repo.status()?))
    })
    .await
}
async fn status(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
) -> std::result::Result<Json<Status>, ApiError> {
    super::http::blocking(move || app.repository(project)?.status()).await
}
async fn watch(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
) -> std::result::Result<Response, ApiError> {
    let init = app.clone();
    let first = super::http::blocking(move || init.repository(project)?.status())
        .await?
        .0;
    let stream = stream::unfold(
        (app, Some(first), None),
        move |(app, mut next, mut previous)| async move {
            loop {
                let value = if let Some(first) = next.take() {
                    first
                } else {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let host = app.clone();
                    tokio::task::spawn_blocking(move || host.repository(project)?.status())
                        .await
                        .ok()?
                        .ok()?
                };
                if previous.as_ref() != Some(&value) {
                    let event = Event::default().event("status").json_data(&value).ok()?;
                    previous = Some(value);
                    return Some((Ok::<_, Infallible>(event), (app, None, previous)));
                }
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/clone", post(clone_project))
        .route("/projects/{project}/git", get(status).post(change))
        .route("/projects/{project}/git/events", get(watch))
}
