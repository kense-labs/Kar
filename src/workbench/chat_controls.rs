use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{
    Error, Result, SessionOptions,
    chat::{ChatHistory, ChatSession, CheckpointPoint, InputStatus, QueuedInput, RollbackReport},
    workbench_git::Repository,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Once},
    time::{Duration, Instant},
};
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct QueuePump {
    started: Once,
    wake: Arc<Notify>,
    errors: Mutex<HashMap<Uuid, (Instant, String)>>,
}

impl Workbench {
    // Existing .inputs journals also cover chats first opened with the CLI.
    fn queued_chats(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let projects = self.projects();
        let mut found = Vec::new();
        for e in std::fs::read_dir(self.0.chats.directory())? {
            let path = e?.path();
            if path.extension().is_none_or(|v| v != "inputs") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            if self.archived(id) {
                continue;
            }
            if self
                .0
                .pump
                .errors
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|(time, _)| time.elapsed() < Duration::from_secs(30))
            {
                continue;
            }
            let controller = self.controller(id);
            match controller.owner.try_lock() {
                Ok(slot) if slot.as_ref().is_none_or(ChatSession::is_closed) => {}
                _ => continue,
            }
            let info = match self.0.chats.saved_info(id) {
                Ok(info) => info,
                Err(e) => {
                    self.queue_error(id, &e);
                    continue;
                }
            };
            let Some(project) = projects.iter().find(|p| {
                self.owns_chat(&self.0.catalog.lock().unwrap(), p, &info)
                    .unwrap_or(false)
            }) else {
                continue;
            };
            match self.0.chats.queued_inputs(id) {
                Ok(q)
                    if q.iter().any(|e| {
                        matches!(
                            e.status,
                            InputStatus::Pending | InputStatus::Sending | InputStatus::Uncertain
                        )
                    }) =>
                {
                    found.push((project.id, id))
                }
                Err(e) => self.queue_error(id, &e),
                _ => {}
            }
        }
        Ok(found)
    }
    fn queue_error(&self, id: Uuid, e: &Error) {
        self.0
            .pump
            .errors
            .lock()
            .unwrap()
            .insert(id, (Instant::now(), e.to_string()));
    }

    pub(super) fn wake_queue(&self) {
        self.0.pump.wake.notify_one();
    }

    pub(super) fn start_services(&self) -> Result<()> {
        self.start_workflows()?;
        self.0.task_folders.recover(self);
        self.0.pump.started.call_once(||{
            let weak=Arc::downgrade(&self.0);
            let wake=self.0.pump.wake.clone();
            tokio::spawn(async move{
                loop{
                    let Some(inner)=weak.upgrade() else{return};
                    let app=Workbench(inner);
                    let scan=app.clone();
                    match tokio::task::spawn_blocking(move||{ super::access::reconcile_guest_inputs(&scan); scan.queued_chats() }).await {
                        Ok(Ok(pending))=>for(project,chat)in pending{
                            let app=app.clone();
                            tokio::spawn(async move {
                                let slot=app.controller(chat);
                                let Ok(mut owner)=slot.owner.try_lock() else{return};
                                if app.archived(chat) || app.binding(project,chat).is_err(){return}
                                // Native history is reconciled by ChatStore before its FIFO worker starts.
                                if let Err(e)=app.attach(chat,&mut owner).await {app.queue_error(chat,&e);}
                            });
                        },
                        Ok(Err(e))=>eprintln!("Kar queue scan: {e}"),
                        Err(e)=>eprintln!("Kar queue worker: {e}"),
                    }
                    drop(app);
                    tokio::select!{_=wake.notified()=>{},_=tokio::time::sleep(Duration::from_secs(15))=>{}}
                }
            });
        });
        Ok(())
    }
    pub(super) async fn attach(
        &self,
        id: Uuid,
        owner: &mut MutexGuard<'_, Option<ChatSession>>,
    ) -> Result<()> {
        if owner.as_ref().is_some_and(ChatSession::is_closed)
            && let Some(old) = owner.take()
        {
            old.close().await?
        }
        if owner.is_none() {
            let info = self.0.chats.saved_info(id)?;
            self.chat_directories(&info.workspace, &info.settings)?;
            let session = self
                .0
                .chats
                .resume_with(id, |opts| {
                    opts.unset_env.insert("KAR_WORKBENCH_TOKEN".into());
                })
                .await?;
            self.controller(id).publish(&session);
            **owner = Some(session);
        }
        self.0.pump.errors.lock().unwrap().remove(&id);
        Ok(())
    }
}

#[derive(Serialize)]
struct Queue {
    inputs: Vec<QueuedInput>,
    delivery_error: Option<String>,
}
async fn list(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> std::result::Result<Json<Queue>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        let inputs = app.0.chats.queued_inputs(c)?;
        let delivery_error = app
            .0
            .pump
            .errors
            .lock()
            .unwrap()
            .get(&c)
            .map(|v| v.1.clone());
        Ok(Queue {
            inputs,
            delivery_error,
        })
    })
    .await
}
#[derive(Deserialize)]
struct Input {
    id: Uuid,
    text: String,
}
async fn enqueue(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(m): Json<Input>,
) -> std::result::Result<Json<QueuedInput>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        if app.archived(c) {
            return Err(Error::Refused("chat is archived".into()));
        }
        let receipt = app.0.chats.enqueue(c, m.id, m.text)?;
        app.0.pump.errors.lock().unwrap().remove(&c);
        app.0.pump.wake.notify_one();
        Ok(receipt)
    })
    .await
}
async fn withdraw(
    State(app): State<Workbench>,
    Path((p, c, id)): Path<(Uuid, Uuid, Uuid)>,
) -> std::result::Result<Json<QueuedInput>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        app.0.chats.withdraw(c, id)
    })
    .await
}
#[derive(Deserialize)]
struct Rollback {
    turn: Uuid,
    #[serde(default)]
    before: bool,
    #[serde(default)]
    dry_run: bool,
}
async fn rollback(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(q): Json<Rollback>,
) -> std::result::Result<Json<RollbackReport>, ApiError> {
    app.binding(p, c)?;
    let point = CheckpointPoint {
        turn: q.turn,
        before: q.before,
    };
    let slot = app.controller(c);
    let owner = slot.owner.try_lock().map_err(|_| Error::Busy)?;
    if let Some(session) = owner.as_ref() {
        return Ok(Json(session.rollback(point, q.dry_run).await?));
    }
    blocking(move || app.0.chats.rollback(c, point, q.dry_run)).await
}
#[derive(Deserialize)]
struct DiffRange {
    #[serde(default)]
    from: usize,
    to: usize,
    ignore_whitespace: Option<bool>,
}
async fn turn_diff(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
    axum::extract::Query(range): axum::extract::Query<DiffRange>,
) -> std::result::Result<Json<crate::chat::TurnDiff>, ApiError> {
    blocking(move || {
        app.binding(project, chat)?;
        app.0.chats.turn_diff(
            chat,
            range.from,
            range.to,
            range.ignore_whitespace.unwrap_or(true),
        )
    })
    .await
}

async fn checkpoints(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    blocking(move||{
        app.binding(p,c)?;
        let h=app.0.chats.history(c)?;
        Ok(serde_json::json!({"position":h.workspace_position,"turns":h.turns.into_iter().map(|t|serde_json::json!({"turn":t.id,"checkpoint":t.checkpoint})).collect::<Vec<_>>() }))
    }).await
}

#[derive(Deserialize)]
struct Fork {
    id: Uuid,
    turn: Uuid,
    destination_project: Option<Uuid>,
}

#[derive(Serialize)]
struct ForkResult {
    project: Uuid,
    history: ChatHistory,
}

fn isolate_host_token(options: &mut SessionOptions) {
    options.unset_env.insert("KAR_WORKBENCH_TOKEN".into());
}

async fn fork(
    State(app): State<Workbench>,
    Path((p, chat)): Path<(Uuid, Uuid)>,
    Json(request): Json<Fork>,
) -> std::result::Result<Json<ForkResult>, ApiError> {
    let project = request.destination_project.unwrap_or(p);
    let host = app.clone();
    let (destination, published) = tokio::task::spawn_blocking(move || -> Result<_> {
        let info = host.binding(p, chat)?;
        host.workspace_at(&info.workspace)?;
        let destination = if project == p {
            None
        } else {
            let target = host
                .saved_fork_directory(request.id, project)
                .unwrap_or(host.project(project)?.root);
            host.workspace_at(&target)?;
            // A project at a checkout subdirectory must map to precisely that
            // subdirectory in the target. The core also pins the managed checkout ID.
            let source_repo = Repository::open(&info.workspace)?;
            let target_repo = Repository::open(&target)?;
            let source_subdir = info
                .workspace
                .strip_prefix(source_repo.root())
                .map_err(|_| super::denied())?;
            if !source_repo.same_repository(&target_repo)
                || target.strip_prefix(target_repo.root()).ok() != Some(source_subdir)
            {
                return Err(Error::Refused(
                    "fork projects must use corresponding directories in the same repository"
                        .into(),
                ));
            }
            Some(target)
        };
        let published = match host.0.chats.initial_info(request.id) {
            Ok(_) => true,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        host.reserve_fork_directory(
            chat,
            p,
            request.turn,
            request.id,
            project,
            destination.as_deref().unwrap_or(&info.workspace),
            destination.is_some(),
        )?;
        Ok((destination, published))
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    let history = if published {
        // The core checks the original request binding. A reply lost after publication
        // can be retrieved while either conversation is processing another turn.
        app.0
            .chats
            .fork_at_with_id_configured(
                chat,
                request.turn,
                request.id,
                destination,
                isolate_host_token,
            )
            .await?
    } else {
        tokio::spawn(async move {
            let slot = app.controller(chat);
            let mut owner = slot.owner.try_lock().map_err(|_| Error::Busy)?;
            app.binding(p, chat)?;
            if app.archived(chat) {
                return Err(Error::Refused("chat is archived".into()));
            }
            if let Some(source) = owner.as_ref().filter(|s| !s.is_closed()) {
                source
                    .fork_at_with_id(request.turn, request.id, destination, isolate_host_token)
                    .await
            } else {
                if let Some(closed) = owner.take() {
                    closed.close().await?;
                }
                app.0
                    .chats
                    .fork_at_with_id_configured(
                        chat,
                        request.turn,
                        request.id,
                        destination,
                        isolate_host_token,
                    )
                    .await
            }
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??
    };
    Ok(Json(ForkResult { project, history }))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/chats/{chat}/queue",
            get(list).post(enqueue),
        )
        .route(
            "/projects/{project}/chats/{chat}/queue/{id}",
            axum::routing::delete(withdraw),
        )
        .route(
            "/projects/{project}/chats/{chat}/checkpoints",
            get(checkpoints),
        )
        .route(
            "/projects/{project}/chats/{chat}/checkpoints/diff",
            get(turn_diff),
        )
        .route("/projects/{project}/chats/{chat}/rollback", post(rollback))
        .route("/projects/{project}/chats/{chat}/forks", post(fork))
}
