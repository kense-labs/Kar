//! Owner and paired clients answer the exact request occurrence shown on their timeline.
use super::{Workbench, http::ApiError};
use crate::{
    Cursor, Error, PendingRequest, PermissionDecision, PolicyChoice, QuestionReply, ResourceId,
    Result, Session,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

impl Workbench {
    pub(super) fn active_control(&self, project: Uuid, chat: Uuid) -> Result<Option<Arc<Session>>> {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowOccurrence {
    started_at: Cursor,
}

async fn stop_workflow(
    State(app): State<Workbench>,
    Path((project, chat, task)): Path<(Uuid, Uuid, String)>,
    Json(input): Json<WorkflowOccurrence>,
) -> std::result::Result<Json<()>, ApiError> {
    let session = app
        .active_control(project, chat)?
        .ok_or_else(|| Error::Refused("workflow controller is unavailable".into()))?;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        session.stop_workflow(&task, input.started_at),
    )
    .await
    .map_err(|_| Error::Closed("workflow stop timed out awaiting the native reply".into()))??;
    Ok(Json(()))
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

#[derive(Deserialize)]
struct Promote {
    command_id: Uuid,
    #[serde(flatten)]
    target: crate::chat::SteerTarget,
}

async fn promote(
    State(app): State<Workbench>,
    Path((project, chat, input)): Path<(Uuid, Uuid, Uuid)>,
    Json(req): Json<Promote>,
) -> std::result::Result<Json<crate::chat::QueuedInput>, ApiError> {
    Ok(Json(
        steer_queued(app, project, chat, input, req.command_id, req.target).await?,
    ))
}
pub(in crate::workbench) async fn steer_queued(
    app: Workbench,
    project: Uuid,
    chat: Uuid,
    input: Uuid,
    command_id: Uuid,
    target: crate::chat::SteerTarget,
) -> Result<crate::chat::QueuedInput> {
    steer_input(app, project, chat, input, command_id, target, None).await
}
pub(in crate::workbench) async fn steer_input(
    app: Workbench,
    project: Uuid,
    chat: Uuid,
    input: Uuid,
    command_id: Uuid,
    target: crate::chat::SteerTarget,
    new_input: Option<(String, Vec<Uuid>)>,
) -> Result<crate::chat::QueuedInput> {
    let session = app.active_control(project, chat)?;
    // Once reserved, the command must finish recording its result even if the HTTP client disconnects.
    let receipt = tokio::spawn(async move {
        let store = app.0.chats.clone();
        let claim = {
            let store = store.clone();
            let online = session.is_some();
            tokio::task::spawn_blocking(move || {
                store.claim_queue_steer(chat, input, command_id, target, online, new_input)
            })
            .await
            .map_err(|e| Error::Closed(e.to_string()))??
        };
        let payload = match claim {
            crate::chat::SteerClaim::Prior(receipt) => return Ok::<_, Error>(receipt),
            crate::chat::SteerClaim::Deliver(payload) => payload,
        };
        let session = session.expect("claim requires an active controller");
        // Codex turn/steer waits for the native JSON-RPC result. Fire-and-forget transports
        // need the correlated event in the target turn before showing delivery as confirmed.
        let native_acknowledged = session.protocol() == crate::Protocol::Codex;
        let result = if session.capabilities().steering {
            session
                .steer_turn(
                    target.turn,
                    target.request_id,
                    input,
                    payload.text,
                    payload.images,
                )
                .await
        } else {
            Err(Error::Unsupported(
                "this provider does not support steering".into(),
            ))
        };
        let receipt = tokio::task::spawn_blocking(move || {
            store.finish_queue_steer(chat, input, command_id, native_acknowledged, result)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        app.wake_queue();
        Ok(receipt)
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    Ok(receipt)
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
    app.attach(p, c, &mut owner).await?;
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
    #[serde(alias = "permission")]
    Decide {
        decision: PermissionDecision,
    },
    Question {
        reply: QuestionReply,
        #[serde(default)]
        attachments_by_question_id: std::collections::BTreeMap<String, Vec<Uuid>>,
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

#[derive(Deserialize)]
struct DismissRequest {
    cursor: Cursor,
    id: Value,
}

fn read_orphaned_sync(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
    cursor: &Cursor,
    id: &Value,
) -> Result<Option<(crate::chat::ChatInfo, crate::OrphanedQuestion)>> {
    let info = app.binding(project, chat)?;
    if app.archived(chat) {
        return Err(Error::Refused("chat is archived".into()));
    }
    if let Some(session) = app.active_control(project, chat)?
        && let Some(questions) = session.orphaned_questions()
    {
        return Ok(questions
            .into_iter()
            .find(|q| &q.cursor == cursor && &q.id == id)
            .map(|q| (info, q)));
    }
    let snapshot = app.0.chats.feed_snapshot(chat)?;
    Ok(snapshot
        .history
        .orphaned_questions
        .into_iter()
        .find(|q| &q.cursor == cursor && &q.id == id)
        .map(|q| (snapshot.history.chat, q)))
}

fn take_orphaned_sync(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
    cursor: &Cursor,
    id: &Value,
) -> Result<Option<crate::OrphanedQuestion>> {
    app.binding(project, chat)?;
    if app.archived(chat) {
        return Err(Error::Refused("chat is archived".into()));
    }
    if let Some(session) = app.active_control(project, chat)?
        && let Some(res) = session.take_orphaned_question(cursor, id)
    {
        return res;
    }
    match app.0.chats.take_orphaned_question(chat, cursor, id) {
        Err(Error::Busy) => {
            if let Some(session) = app.active_control(project, chat)?
                && let Some(res) = session.take_orphaned_question(cursor, id)
            {
                return res;
            }
            Err(Error::Busy)
        }
        other => other,
    }
}

pub(in crate::workbench) async fn answer_orphaned_question(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
    cursor: Cursor,
    id: Value,
    reply: QuestionReply,
    attachments_by_question_id: std::collections::BTreeMap<String, Vec<Uuid>>,
) -> Result<()> {
    let state = app.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let (_, orphaned) = read_orphaned_sync(&state, project, chat, &cursor, &id)?
            .ok_or_else(|| Error::Refused("request occurrence is no longer pending".into()))?;
        match reply {
            QuestionReply::Cancel | QuestionReply::Decline => {
                if !attachments_by_question_id.is_empty() {
                    return Err(Error::Refused(
                        "attachments require a submitted answer".into(),
                    ));
                }
                take_orphaned_sync(&state, project, chat, &cursor, &id)?.ok_or_else(|| {
                    Error::Refused("request occurrence is no longer pending".into())
                })?;
                Ok(())
            }
            QuestionReply::Submit { values } => {
                let notes = if attachments_by_question_id.is_empty() {
                    std::collections::BTreeMap::new()
                } else {
                    state
                        .0
                        .chats
                        .question_attachment_notes(chat, attachments_by_question_id)?
                };
                let formatted = orphaned.validate_and_format(&values, &notes)?;
                let info = state.binding(project, chat)?;
                crate::input::check_text(&formatted, crate::input::text_limit(&info.runtime))?;
                state.workspace(&state.project(project)?)?;
                take_orphaned_sync(&state, project, chat, &cursor, &id)?.ok_or_else(|| {
                    Error::Refused("request occurrence is no longer pending".into())
                })?;
                let id_json = serde_json::to_string(&id).unwrap_or_default();
                let turn_id = crate::chat::stable_id([
                    b"orphaned-question-answer".as_slice(),
                    chat.as_bytes().as_slice(),
                    cursor.stream_id.as_bytes().as_slice(),
                    &cursor.after_seq.to_le_bytes(),
                    id_json.as_bytes(),
                ]);
                state
                    .0
                    .chats
                    .enqueue_attached(chat, turn_id, formatted, &[])?;
                state.wake_queue();
                Ok(())
            }
        }
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))?
}

pub(in crate::workbench) async fn dismiss_request(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
    cursor: Cursor,
    id: Value,
) -> Result<()> {
    if let Some(session) = app.active_control(project, chat)?
        && let Some(request) = session
            .snapshot()
            .pending_requests
            .into_iter()
            .find(|r| r.cursor == cursor && r.id == id)
    {
        return tokio::spawn(async move {
            match request.kind {
                crate::RequestKind::Question => {
                    session
                        .answer_question(&request, QuestionReply::Cancel)
                        .await
                }
                crate::RequestKind::Permission => {
                    session.decide(&request, PermissionDecision::Cancel).await
                }
                _ => Err(Error::Unsupported(
                    "this request cannot be dismissed".into(),
                )),
            }
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?;
    }
    let state = app.clone();
    tokio::task::spawn_blocking(move || {
        take_orphaned_sync(&state, project, chat, &cursor, &id)?
            .map(drop)
            .ok_or_else(|| Error::Refused("request occurrence is no longer pending".into()))
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))?
}

pub(in crate::workbench) async fn respond_question_mcp(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
    cursor: Cursor,
    id: Value,
    reply: QuestionReply,
) -> Result<()> {
    if let Some(session) = app.active_control(project, chat)?
        && let Some(request) =
            session.snapshot().pending_requests.into_iter().find(|r| {
                r.kind == crate::RequestKind::Question && r.cursor == cursor && r.id == id
            })
    {
        crate::questions::answer(&request.method, &request.body, reply.clone())?;
        return tokio::spawn(async move { session.answer_question(&request, reply).await })
            .await
            .map_err(|e| Error::Closed(e.to_string()))?;
    }
    answer_orphaned_question(
        app,
        project,
        chat,
        cursor,
        id,
        reply,
        std::collections::BTreeMap::new(),
    )
    .await
}

async fn dismiss(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(input): Json<DismissRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    dismiss_request(&app, p, c, input.cursor, input.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn respond(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(input): Json<Respond>,
) -> std::result::Result<Json<()>, ApiError> {
    if let Some(session) = app.active_control(p, c)?
        && let Some(request) = session
            .snapshot()
            .pending_requests
            .into_iter()
            .find(|r| r.cursor == input.cursor && r.id == input.id)
    {
        // Validate payload and attachments synchronously before spawning so invalid input
        // returns 400 immediately without consuming the request.
        let mut notes = std::collections::BTreeMap::new();
        match &input.answer {
            Answer::Decide { decision } => {
                crate::interaction::decision_payload(&request.method, &request.body, *decision)
                    .ok_or_else(|| {
                        Error::Unsupported(
                            "this request does not offer that permission decision".into(),
                        )
                    })?;
            }
            Answer::Question {
                reply,
                attachments_by_question_id,
            } => {
                if !attachments_by_question_id.is_empty() {
                    let store = app.0.chats.clone();
                    let ids = attachments_by_question_id.clone();
                    notes = tokio::task::spawn_blocking(move || {
                        store.question_attachment_notes(c, ids)
                    })
                    .await
                    .map_err(|e| Error::Closed(e.to_string()))??;
                }
                crate::questions::answer_with_files(
                    &request.method,
                    &request.body,
                    reply.clone(),
                    notes.clone(),
                )?;
            }
            Answer::Policy { choice } => {
                crate::permissions::select(&request.method, &request.body, *choice).ok_or_else(
                    || Error::Refused("policy option is not offered on this request".into()),
                )?;
            }
            Answer::Resources {
                decision,
                resources,
            } => {
                crate::permissions::grant(&request.method, &request.body, *decision, resources)
                    .ok_or_else(|| {
                        Error::Refused("invalid permission scope or resource selection".into())
                    })?;
            }
            Answer::Native { .. } => {}
        }
        let answer = input.answer;
        tokio::spawn(async move {
            match answer {
                Answer::Decide { decision } => session.decide(&request, decision).await,
                Answer::Question { reply, .. } => {
                    session
                        .answer_question_with_files(&request, reply, notes)
                        .await
                }
                Answer::Policy { choice } => session.select_policy(&request, choice).await,
                Answer::Resources {
                    decision,
                    resources,
                } => session.grant_resources(&request, decision, resources).await,
                Answer::Native { payload } => session.answer_request(&request, payload).await,
            }
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        return Ok(Json(()));
    }

    match input.answer {
        Answer::Question {
            reply,
            attachments_by_question_id,
        } => {
            answer_orphaned_question(
                &app,
                p,
                c,
                input.cursor,
                input.id,
                reply,
                attachments_by_question_id,
            )
            .await?;
            Ok(Json(()))
        }
        Answer::Decide { .. }
        | Answer::Policy { .. }
        | Answer::Resources { .. }
        | Answer::Native { .. } => {
            app.binding(p, c)?;
            if app.archived(c) {
                return Err(Error::Refused("chat is archived".into()).into());
            }
            Err(Error::Refused("request occurrence is no longer pending".into()).into())
        }
    }
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/chats/{chat}/tasks/{task}/stop-workflow",
            post(stop_workflow),
        )
        .route(
            "/projects/{project}/chats/{chat}/queue/{input}/steer",
            post(promote),
        )
        .route(
            "/projects/{project}/chats/{chat}/turns/{turn}/interrupt",
            post(interrupt),
        )
        .route("/projects/{project}/chats/{chat}/attach", post(attach))
        .route(
            "/projects/{project}/chats/{chat}/requests/respond",
            post(respond),
        )
        .route(
            "/projects/{project}/chats/{chat}/requests/dismiss",
            post(dismiss),
        )
}
