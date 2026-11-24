//! Mark the live provider's background tasks while preserving each task's original user turn.
use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{BackgroundWaitReceipt, BackgroundWaitRequest, Error, NativeTask, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use std::time::Duration;
use uuid::Uuid;

#[derive(Serialize)]
pub struct BackgroundTasks {
    /// Working because of awaited background work, independently of a foreground model turn.
    pub background_working: bool,
    pub pending: Vec<NativeTask>,
    /// Entries whose owning process or controller vanished before delivering a terminal outcome.
    pub lost: Vec<NativeTask>,
    /// Foreground results stay in history. A previously finished run waiting on lost work
    /// has an interrupted workbench outcome on restart.
    pub interrupted_turns: Vec<Uuid>,
}

impl Workbench {
    pub fn background_tasks(&self, project: Uuid, chat: Uuid) -> Result<BackgroundTasks> {
        let info = self.binding(project, chat)?;
        let history = self.0.chats.history(chat)?;
        let mut pending = Vec::new();
        let mut lost = Vec::new();
        for task in history.tasks {
            if !task.source.path.is_empty()
                || task.source.session_id.as_deref() != Some(info.native_id.as_str())
                || task.background_work.is_none()
            {
                continue;
            }
            if task.pending_background() {
                pending.push(task);
            } else if task.ended_on_exit {
                lost.push(task);
            }
        }
        pending.sort_by_key(|r| r.background_work.as_ref().unwrap().order);
        let interrupted_turns = history
            .turns
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    crate::chat::TurnStatus::Completed | crate::chat::TurnStatus::Aborted
                ) && lost.iter().any(|r| {
                    r.background_work
                        .as_ref()
                        .is_some_and(|w| w.awaited && w.turn_id == Some(t.id)
                            && r.background_exit != Some(crate::BackgroundExit::HostClosed))
                })
            })
            .map(|t| t.id)
            .collect();
        Ok(BackgroundTasks {
            interrupted_turns,
            background_working: pending.iter().any(NativeTask::blocking_background),
            pending,
            lost,
        })
    }

    pub async fn await_background_tasks(
        &self,
        project: Uuid,
        chat: Uuid,
        request: BackgroundWaitRequest,
    ) -> Result<BackgroundWaitReceipt> {
        let native = self
            .active_control(project, chat)?
            .ok_or_else(|| Error::Closed("background task controller unavailable".into()))?;
        tokio::time::timeout(
            Duration::from_secs(15),
            native.await_background_tasks(request),
        )
        .await
        .map_err(|_| Error::Timeout("updating background tasks".into()))?
    }
}

async fn get_tasks(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> std::result::Result<Json<BackgroundTasks>, ApiError> {
    blocking(move || app.background_tasks(p, c)).await
}
async fn mark(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(req): Json<BackgroundWaitRequest>,
) -> Response {
    match app.await_background_tasks(p, c, req).await {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => {
            let (status, reason) = match &error {
                Error::Io(_) => return ApiError::from(error).into_response(),
                Error::Closed(_) => (StatusCode::CONFLICT, "unavailable"),
                Error::Unsupported(_) => (StatusCode::BAD_REQUEST, "unsupported"),
                Error::Refused(msg) if msg == BackgroundWaitRequest::UNMATCHED => {
                    (StatusCode::BAD_REQUEST, "unmatched")
                }
                Error::Refused(msg) if msg == "chat is archived" => {
                    (StatusCode::CONFLICT, "unavailable")
                }
                Error::Refused(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
                Error::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, "failed"),
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "failed"),
            };
            (
                status,
                Json(serde_json::json!({"error":error.to_string(),"reason":reason})),
            )
                .into_response()
        }
    }
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/chats/{chat}/background-tasks",
            get(get_tasks),
        )
        .route(
            "/projects/{project}/chats/{chat}/background-tasks/await",
            post(mark),
        )
}
