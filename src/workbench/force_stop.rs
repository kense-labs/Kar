//! Release the controller of the selected run while preserving the FIFO and its original native binding.
use super::{Workbench, http::ApiError};
use crate::{Error, Protocol, Result, Session, chat::TurnStatus};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stop {
    turn: Option<Uuid>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Released,
    Killed,
    Shared,
    Releasing,
}

#[derive(Serialize)]
struct Receipt {
    turn: Option<Uuid>,
    outcome: Outcome,
    shared_with: Vec<Uuid>,
}

impl Workbench {
    fn shared_controllers(&self, id: Uuid, session: &Session) -> Vec<Uuid> {
        let Some(root) = &session.process_root else {
            return vec![];
        };
        if !root.shared() {
            return vec![];
        }
        let mut others: Vec<_> = self
            .0
            .controllers
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(other, controller)| {
                if *other == id {
                    return None;
                }
                let handle = controller.live.lock().unwrap().upgrade()?;
                (!handle.is_closed()
                    && handle
                        .process_root
                        .as_ref()
                        .is_some_and(|r| root.shares_process_with(r)))
                .then_some(*other)
            })
            .collect();
        others.sort();
        others
    }
}

async fn stop(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
    Json(input): Json<Stop>,
) -> std::result::Result<Json<Receipt>, ApiError> {
    let state = app.clone();
    let (fence, target, running, interrupted, stream) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            state.binding(project, chat)?;
            let fence = state.0.chats.stop_fence(chat, true)?;
            let history = state.0.chats.history(chat)?;
            let last = history.turns.last();
            let target = last.map(|t| t.id);
            if input.turn.is_some() && input.turn != target {
                return Err(crate::transport::AbortRequest::stale());
            }
            let running = last.is_some_and(|t| t.status == TurnStatus::Running);
            let interrupted = running || last.is_some_and(|t| t.status == TurnStatus::Interrupted);
            Ok((
                fence,
                target,
                running,
                interrupted,
                history.cursor.map(|c| c.stream_id),
            ))
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
    let controller = app.controller(chat);
    let live = controller.live.lock().unwrap().upgrade();
    let Some(session) = live else {
        // A CLI process can own this journal even when the HTTP server has no controller.
        let state = app.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _offline = state.0.chats.confirm_offline(chat)?;
            let is_child =
                super::mcp::orchestrator::Task::prepare_force_stop(&state, project, chat)?;
            let sync_ids = super::mcp::orchestrator::Task::finish_force_stop_fenced(
                &state,
                project,
                chat,
                target,
                interrupted,
            )?;
            drop(fence);
            super::mcp::orchestrator::Task::finish_force_stop_unfenced(
                &state, chat, is_child, &sync_ids,
            );
            state.wake_queue();
            Ok(())
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        return Ok(Json(Receipt {
            turn: target,
            outcome: Outcome::Released,
            shared_with: vec![],
        }));
    };
    if stream.is_some_and(|stream| stream != session.stream_id()) {
        return Err(Error::Busy.into());
    }
    let state = app.clone();
    let is_child = tokio::task::spawn_blocking(move || {
        super::mcp::orchestrator::Task::prepare_force_stop(&state, project, chat)
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    let shared_with = app.shared_controllers(chat, &session);
    let shared = !shared_with.is_empty();
    let mut teardown = tokio::spawn(async move {
        // A hosted controller may only detach on close. Cancel its own active request first.
        if running
            && matches!(session.protocol(), Protocol::Cybion | Protocol::Morph)
            && let Some(turn) = target
            && let Err(e) = session.abort_turn(turn, None).await
        {
            if !matches!(&e, Error::Refused(reason) if reason == crate::transport::AbortRequest::STALE)
            {
                return Err(e);
            }
            if app
                .0
                .chats
                .history(chat)?
                .turns
                .last()
                .is_some_and(|t| t.status == TurnStatus::Running)
            {
                return Err(e);
            }
        }
        let (closed, signalled) = {
            let close = session.close();
            tokio::pin!(close);
            match tokio::time::timeout(Duration::from_millis(300), &mut close).await {
                Ok(result) => (result, false),
                Err(_) => {
                    let root = session.process_root.clone();
                    let sent = tokio::task::spawn_blocking(move || match root {
                        Some(root) => root.stop_dedicated_group(),
                        None => Ok(false),
                    })
                    .await
                    .map_err(|e| Error::Closed(e.to_string()))??;
                    (close.await, sent)
                }
            }
        };
        if closed.is_err() && !session.is_closed() {
            return closed.map(|()| Outcome::Released);
        }
        let killed = signalled
            || session
                .process_root
                .as_ref()
                .is_some_and(|root| !root.shared())
                && session
                    .records()
                    .iter()
                    .rev()
                    .find(|r| {
                        r.direction == crate::Direction::Host
                            && r.events
                                .iter()
                                .any(|e| matches!(e, crate::Event::Exited { .. }))
                    })
                    .is_some_and(|r| {
                        r.native["type"] == "exited"
                            && r.native.get("code").is_some_and(|v| v.is_null())
                    });
        let identity = Arc::downgrade(&session);
        drop(session);
        let mut owner = controller.owner.lock().await;
        if owner
            .as_ref()
            .is_some_and(|bound| bound.live_session().ptr_eq(&identity))
            && let Some(bound) = owner.take()
        {
            bound.close().await?;
        }
        drop(owner);
        let state = app.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let sync_ids = super::mcp::orchestrator::Task::finish_force_stop_fenced(
                &state,
                project,
                chat,
                target,
                interrupted,
            )?;
            drop(fence);
            super::mcp::orchestrator::Task::finish_force_stop_unfenced(
                &state, chat, is_child, &sync_ids,
            );
            Ok(())
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        app.wake_queue();
        closed?;
        Ok::<_, Error>(if shared {
            Outcome::Shared
        } else if killed {
            Outcome::Killed
        } else {
            Outcome::Released
        })
    });
    let outcome = match tokio::time::timeout(Duration::from_secs(10), &mut teardown).await {
        Ok(task) => task.map_err(|e| Error::Closed(e.to_string()))??,
        Err(_) => Outcome::Releasing,
    };
    Ok(Json(Receipt {
        turn: target,
        outcome,
        shared_with,
    }))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new().route("/projects/{project}/chats/{chat}/force-stop", post(stop))
}
