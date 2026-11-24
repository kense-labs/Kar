use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{
    Error, Result, SessionOptions,
    chat::{ChatHistory, ChatSession, CheckpointPoint, QueuedInput, RollbackReport},
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
    recovered_workspaces: Mutex<bool>,
    started: Once,
    wake: Arc<Notify>,
    errors: Mutex<HashMap<Uuid, (Instant, String)>>,
    settled_background: Mutex<HashMap<Uuid, (u64, std::time::SystemTime)>>,
}

impl Workbench {
    // Chats opened through the CLI also participate. An awaited task may be
    // the only outstanding work, with no inbox file on disk.
    fn queued_chats(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let projects = self.projects();
        let mut found = std::collections::BTreeSet::new();
        for e in std::fs::read_dir(self.0.chats.directory())? {
            let path = e?.path();
            let background = match path.extension().and_then(|v| v.to_str()) {
                Some("inputs") => false,
                Some("karchat") => true,
                _ => continue,
            };
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
            if background {
                let metadata = std::fs::metadata(&path)?;
                let stamp = (metadata.len(), metadata.modified()?);
                if self.0.pump.settled_background.lock().unwrap().get(&id) == Some(&stamp) {
                    continue;
                }
                match self.0.chats.needs_background_wake(id) {
                    Ok(true) => {}
                    Ok(false) => {
                        self.0
                            .pump
                            .settled_background
                            .lock()
                            .unwrap()
                            .insert(id, stamp);
                        continue;
                    }
                    Err(e) => {
                        self.queue_error(id, &e);
                        continue;
                    }
                }
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
            if background {
                found.insert((project.id, id));
                continue;
            }
            match self.0.chats.queued_inputs(id) {
                Ok(q) if q.iter().any(QueuedInput::outstanding) => {
                    found.insert((project.id, id));
                }
                Err(e) => self.queue_error(id, &e),
                _ => {}
            }
        }
        Ok(found.into_iter().collect())
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
        {
            let mut recovered=self.0.pump.recovered_workspaces.lock().unwrap();
            if !*recovered {
                super::mcp::worktree::recover_queue_reservations(self)?;
                super::mcp::collaboration_tools::environment::cards::recover(self)?;
                super::mcp::collaboration_tools::environment::secrets::clean(&self.0.state)?;
                *recovered=true;
            }
        }
        self.0.shell.start(self);
        self.start_workflows()?;
        self.0.devices.maintain(self);
        self.0.task_folders.recover(self);
        self.0.pump.started.call_once(||{
            let cleaner=Arc::downgrade(&self.0);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(300)).await;
                    let Some(app)=cleaner.upgrade() else {return;};
                    let state=app.state.clone();
                    drop(app);
                    let _=tokio::task::spawn_blocking(move || super::mcp::collaboration_tools::environment::secrets::clean(&state)).await;
                }
            });
            let weak=Arc::downgrade(&self.0);
            let wake=self.0.pump.wake.clone();
            tokio::spawn(async move{
                loop{
                    let Some(inner)=weak.upgrade() else{return};
                    let app=Workbench(inner);
                    super::mcp::worktree::recover(&app);
                    super::mcp::orchestrator::recover(&app);
                    super::forge::watches::recover(&app);
                    let scan=app.clone();
                    match tokio::task::spawn_blocking(move||{ super::access::reconcile_guest_inputs(&scan); scan.queued_chats() }).await {
                        Ok(Ok(pending))=>for(project,chat)in pending{
                            let app=app.clone();
                            tokio::spawn(async move {
                                let slot=app.controller(chat);
                                let Ok(mut owner)=slot.owner.try_lock() else{return};
                                if app.archived(chat) || app.binding(project,chat).is_err(){return}
                                // Native history is reconciled by ChatStore before its FIFO worker starts.
                                if let Err(e)=app.attach(project,chat,&mut owner).await {app.queue_error(chat,&e);}
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
        project: Uuid,
        id: Uuid,
        owner: &mut MutexGuard<'_, Option<ChatSession>>,
    ) -> Result<()> {
        if super::mcp::worktree::pending(self, id) { return Err(Error::Busy); }
        self.attach_bound(project,id,owner).await
    }
    pub(super) async fn attach_bound(
        &self, project: Uuid, id: Uuid, owner: &mut MutexGuard<'_, Option<ChatSession>>,
    ) -> Result<()> {
        let _dispatch = self.0.chats.stop_fence(id, false)?;
        if owner.as_ref().is_some_and(ChatSession::is_closed)
            && let Some(old) = owner.take()
        {
            old.close().await?
        }
        if owner.is_none() {
            let info = self.0.chats.saved_info(id)?;
            self.chat_directories(&info.workspace, &info.settings)?;
            let mut tools = None;
            let mut session = self
                .0
                .chats
                .resume_configured(id, |runtime, opts| {
                    tools = self.0.mcp.issue(project, id, runtime)?;
                    if let Some(lease) = &tools { lease.configure(opts); }
                    opts.unset_env.insert("KAR_WORKBENCH_TOKEN".into());
                    opts.prompt_suggestions = self.suggestion_source(id);
                    Ok(())
                })
                .await?;
            self.controller(id).bind_model_tools(&mut session, tools);
            **owner = Some(session);
        }
        if let Some(lease) = self.controller(id).model_tools.lock().unwrap().as_ref() {lease.touch();}
        self.0.pump.errors.lock().unwrap().remove(&id);
        Ok(())
    }
}

#[derive(Serialize)]
struct Queue {
    held: bool,
    inputs: Vec<QueuedInput>,
    delivery_error: Option<String>,
}
async fn list(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> std::result::Result<Json<Queue>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        let (held, inputs) = app.0.chats.queue_view(c)?;
        let delivery_error = app
            .0
            .pump
            .errors
            .lock()
            .unwrap()
            .get(&c)
            .map(|v| v.1.clone());
        Ok(Queue {
            held,
            inputs,
            delivery_error,
        })
    })
    .await
}
#[derive(Deserialize)]
struct QueueCommand {
    command_id: Uuid,
    #[serde(flatten)]
    action: crate::chat::QueueAction,
}
async fn control_queue(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(request): Json<QueueCommand>,
) -> std::result::Result<Json<crate::chat::QueueReceipt>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        if app.archived(c) {
            return Err(Error::Refused("chat is archived".into()));
        }
        let receipt = app
            .0
            .chats
            .queue_control(c, request.command_id, request.action)?;
        app.0.pump.errors.lock().unwrap().remove(&c);
        app.wake_queue();
        Ok(receipt)
    })
    .await
}
#[derive(Deserialize)]
struct Input {
    requested_output_schema: Option<serde_json::Value>,
    #[serde(default)]
    attachments: Vec<Uuid>,
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
        if let Some(schema)=m.requested_output_schema { app.register_run_schema(p,c,m.id,schema)?; }
        let receipt = app
            .0
            .chats
            .enqueue_attached(c, m.id, m.text, &m.attachments)?;
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
#[serde(deny_unknown_fields)]
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
async fn rewind(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
    Json(request): Json<crate::chat::RewindRequest>,
) -> std::result::Result<Json<crate::chat::ContextRewind>, ApiError> {
    let result = tokio::spawn(async move {
        app.binding(project, chat)?;
        let controller = app.controller(chat);
        let history = app.0.chats.history(chat)?;
        if let Some(old) = history.rewinds.into_iter().find(|r| r.id == request.id) {
            old.check(&request)?;
            return Ok(old);
        }
        if history.chat.process_only {
            return Err(Error::Unsupported("provider cannot resume a rewound conversation".into()));
        }
        let mut owner = controller.owner.try_lock().map_err(|_| Error::Busy)?;
        if app.archived(chat) {
            return Err(Error::Refused("chat is archived".into()));
        }
        if let Some(session) = owner.as_ref()
            && session.needs_controller()?
        {
            return Err(Error::Busy);
        }
        if app.0.chats.queued_inputs(chat)?.iter().any(crate::chat::QueuedInput::outstanding) {
            return Err(Error::Busy);
        }
        if let Some(session) = owner.take() {
            session.close().await?;
        }
        app.0
            .chats
            .rewind_configured(chat, request, |_, opts| {
                isolate_host_token(opts);
                Ok(())
            })
            .await
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    Ok(Json(result))
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
    #[serde(default)]
    policy: crate::chat::ForkPolicy,
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
            .fork_at_with_policy(
                chat,
                request.turn,
                request.id,
                destination,
                request.policy,
                isolate_host_token,
            )
            .await?
    } else {
        tokio::spawn(async move {
            let mut child_lease = None;
            let configure = |runtime: &str, opts: &mut SessionOptions| -> Result<()> {
                isolate_host_token(opts);
                child_lease = app.0.mcp.issue(project, request.id, runtime)?;
                if let Some(lease) = &child_lease {
                    lease.configure(opts);
                }
                Ok(())
            };
            let slot = app.controller(chat);
            app.binding(p, chat)?;
            if app.archived(chat) {
                return Err(Error::Refused("chat is archived".into()));
            }
            let mut owner = match slot.owner.try_lock() {
                Ok(owner) => owner,
                Err(_) => {
                    let destination = destination.ok_or(Error::Busy)?;
                    let live = slot.fork.lock().unwrap().clone().ok_or(Error::Busy)?;
                    return live
                        .fork(request.turn, request.id, destination, configure)
                        .await;
                }
            };
            if let Some(source) = owner.as_ref().filter(|s| !s.is_closed()) {
                source
                    .workbench_fork(
                        request.turn,
                        request.id,
                        destination,
                        request.policy,
                        configure,
                    )
                    .await
            } else {
                if let Some(closed) = owner.take() {
                    closed.close().await?;
                }
                app.0
                    .chats
                    .workbench_fork(
                        chat,
                        request.turn,
                        request.id,
                        destination,
                        request.policy,
                        configure,
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
            "/projects/{project}/chats/{chat}/queue/commands",
            post(control_queue),
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
        .route("/projects/{project}/chats/{chat}/rewinds", post(rewind))
        .route("/projects/{project}/chats/{chat}/rollback", post(rollback))
        .route("/projects/{project}/chats/{chat}/forks", post(fork))
}
