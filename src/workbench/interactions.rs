//! Owner and paired clients answer the exact request occurrence shown on their timeline.
use super::{Workbench, http::ApiError};
use crate::{
    Cursor, Error, PendingRequest, PermissionDecision, PolicyChoice, QuestionReply, ResourceId,
    Result, Session,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

impl Workbench {
    fn active_control(&self, project: Uuid, chat: Uuid) -> Result<Option<Arc<Session>>> {
        self.binding(project, chat)?;
        if self.archived(chat) {
            return Err(Error::Refused("chat is archived".into()));
        }
        Ok(self
            .controller(chat)
            .live
            .lock()
            .unwrap()
            .upgrade()
            .filter(|s| !s.is_closed()))
    }
}

#[derive(Default, Deserialize)]
struct Attempt {
    request_id: Option<Uuid>,
}

async fn interrupt(
    State(app): State<Workbench>,
    Path((p, c, turn)): Path<(Uuid, Uuid, Uuid)>,
    axum::extract::Query(attempt): axum::extract::Query<Attempt>,
) -> std::result::Result<Json<()>, ApiError> {
    let session = app
        .active_control(p, c)?
        .ok_or_else(crate::transport::AbortRequest::stale)?;
    session.abort_turn(turn, attempt.request_id).await?;
    Ok(Json(()))
}

async fn attach(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> std::result::Result<Json<Vec<PendingRequest>>, ApiError> {
    if let Some(session) = app.active_control(p, c)? {
        return Ok(Json(session.snapshot().pending_requests));
    }
    let slot = app.controller(c);
    let mut owner = slot.owner.try_lock().map_err(|_| Error::Busy)?;
    if app.archived(c) {
        return Err(Error::Refused("chat is archived".into()).into());
    }
    app.workspace(&app.project(p)?)?;
    app.attach(c, &mut owner).await?;
    Ok(Json(
        owner
            .as_ref()
            .expect("attached chat")
            .snapshot()
            .pending_requests,
    ))
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Answer {
    Decide {
        decision: PermissionDecision,
    },
    Question {
        reply: QuestionReply,
    },
    Policy {
        choice: PolicyChoice,
    },
    Resources {
        decision: PermissionDecision,
        resources: Vec<ResourceId>,
    },
    Native {
        payload: Value,
    },
}

#[derive(Deserialize)]
struct Respond {
    cursor: Cursor,
    id: Value,
    #[serde(flatten)]
    answer: Answer,
}

async fn respond(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(input): Json<Respond>,
) -> std::result::Result<Json<()>, ApiError> {
    let session = app.active_control(p, c)?.ok_or_else(|| {
        Error::Refused("request occurrence is offline; attach to see current requests".into())
    })?;
    let request = session
        .snapshot()
        .pending_requests
        .into_iter()
        .find(|r| r.cursor == input.cursor && r.id == input.id)
        .ok_or_else(|| Error::Refused("request occurrence is no longer pending".into()))?;
    // The native actor rechecks the cursor before writing. Concurrent decisions cannot
    // answer a later prompt even when the provider reuses the same numeric request ID.
    match input.answer {
        Answer::Decide { decision } => session.decide(&request, decision).await?,
        Answer::Question { reply } => session.answer_question(&request, reply).await?,
        Answer::Policy { choice } => session.select_policy(&request, choice).await?,
        Answer::Resources {
            decision,
            resources,
        } => {
            session
                .grant_resources(&request, decision, resources)
                .await?
        }
        Answer::Native { payload } => session.answer_request(&request, payload).await?,
    }
    Ok(Json(()))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/chats/{chat}/turns/{turn}/interrupt",
            post(interrupt),
        )
        .route("/projects/{project}/chats/{chat}/attach", post(attach))
        .route(
            "/projects/{project}/chats/{chat}/requests/respond",
            post(respond),
        )
}
