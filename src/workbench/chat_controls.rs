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
    armed_quota_windows: Mutex<HashMap<Uuid, (Uuid, Uuid, u64)>>,
}

fn epoch_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
            if self.active_quota_hold_until(id).is_some() {
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

    pub(in crate::workbench) fn active_quota_hold_until(&self, chat: Uuid) -> Option<u64> {
        let now = epoch_now_ms();
        let history = self.0.chats.history(chat).ok()?;
        let last = history.turns.last()?;
        if last.status != crate::chat::TurnStatus::Failed {
            return None;
        }
        let failure = last.failure.as_ref()?;
        if failure.class != crate::FailureClass::Quota {
            return None;
        }
        let reset_at = failure.resets_at_ms?;
        (reset_at > now).then_some(reset_at)
    }

    fn scan_and_recover_quota_limits(&self) -> Option<Duration> {
        let now = epoch_now_ms();
        let projects = self.projects();
        // 1. Discover any newly quota-blocked chats and arm their recovery window.
        if let Ok(chats) = self.0.chats.list() {
            for info in chats {
                if self.archived(info.id) {
                    continue;
                }
                let Ok(history) = self.0.chats.history(info.id) else {
                    continue;
                };
                let Some(last) = history.turns.last() else {
                    continue;
                };
                if last.status == crate::chat::TurnStatus::Failed
                    && let Some(failure) = &last.failure
                    && failure.class == crate::FailureClass::Quota
                    && let Some(reset_at) = failure.resets_at_ms
                    && reset_at > now
                    && let Some(proj) = projects.iter().find(|p| {
                        self.owns_chat(&self.0.catalog.lock().unwrap(), p, &info)
                            .unwrap_or(false)
                    })
                {
                    self.0
                        .pump
                        .armed_quota_windows
                        .lock()
                        .unwrap()
                        .entry(info.id)
                        .or_insert((proj.id, last.id, reset_at));
                }
            }
        }

        // 2. Resume any armed chats whose `resets_at_ms` has arrived!
        let due: Vec<(Uuid, Uuid, Uuid)> = {
            let mut armed = self.0.pump.armed_quota_windows.lock().unwrap();
            let ready: Vec<Uuid> = armed
                .iter()
                .filter_map(|(chat, (_, _, at))| (*at <= now).then_some(*chat))
                .collect();
            ready
                .into_iter()
                .filter_map(|chat| {
                    armed
                        .remove(&chat)
                        .map(|(proj, turn, _)| (proj, chat, turn))
                })
                .collect()
        };

        let mut woke_any = false;
        for (project, chat, failed_turn) in due {
            if self.archived(chat) || self.binding(project, chat).is_err() {
                continue;
            }
            let Ok(history) = self.0.chats.history(chat) else {
                continue;
            };
            if history.turns.last().map(|t| t.id) != Some(failed_turn) {
                continue;
            }
            let has_queued = self
                .0
                .chats
                .queued_inputs(chat)
                .map(|q| q.iter().any(QueuedInput::outstanding))
                .unwrap_or(false);
            if !has_queued
                && std::env::var("KAR_AUTO_CONTINUE_ON_QUOTA_RESET").as_deref() != Ok("0")
            {
                let resume_id = crate::chat::stable_id([
                    chat.as_bytes().as_slice(),
                    failed_turn.as_bytes().as_slice(),
                    b"quota-recovery",
                ]);
                let _ = self
                    .0
                    .chats
                    .enqueue(chat, resume_id, "Continue where you left off.");
            }
            self.0.pump.errors.lock().unwrap().remove(&chat);
            woke_any = true;
        }
        if woke_any {
            self.wake_queue();
        }

        self.0
            .pump
            .armed_quota_windows
            .lock()
            .unwrap()
            .values()
            .map(|(_, _, at)| Duration::from_millis(at.saturating_sub(epoch_now_ms()).max(50)))
            .min()
    }

    fn try_restore_missing_worktree(
        &self,
        project: Uuid,
        info: &crate::chat::ChatInfo,
    ) -> Result<()> {
        if info.workspace.exists() {
            return Ok(());
        }
        let Ok(proj) = self.project(project) else {
            return Ok(());
        };
        let Ok(repo) = Repository::open(&proj.root) else {
            return Ok(());
        };
        if let Some(wt_id) = info.worktree_id {
            let _ = repo.restore_removed_worktree(wt_id)?;
        } else {
            let _ = repo.restore_removed_worktree_at(&info.workspace)?;
        }
        Ok(())
    }

    pub(super) fn start_services(&self) -> Result<()> {
        {
            let mut recovered = self.0.pump.recovered_workspaces.lock().unwrap();
            if !*recovered {
                super::mcp::worktree::recover_queue_reservations(self)?;
                super::forge::guides::recover(self)?;
                super::mcp::collaboration_tools::environment::cards::recover(self)?;
                super::mcp::collaboration_tools::environment::secrets::clean(&self.0.state)?;
                *recovered = true;
            }
        }
        self.0.shell.start(self);
        self.start_workflows()?;
        self.0.devices.maintain(self);
        self.0.task_folders.recover(self);
        self.0.pump.started.call_once(||{
            super::prepared_runs::recover(self);
            super::relay_client::tunnel::start(self);
            super::access::start_relay_workers(self.clone());
            super::storage::start_cleanup_worker(self.clone());
            let quota_weak = Arc::downgrade(&self.0);
            tokio::spawn(async move {
                loop {
                    let next_wait = {
                        let Some(inner) = quota_weak.upgrade() else {
                            return;
                        };
                        let app = Workbench(inner);
                        tokio::task::spawn_blocking(move || app.scan_and_recover_quota_limits())
                            .await
                            .ok()
                            .flatten()
                    };
                    let sleep_dur = next_wait
                        .unwrap_or(Duration::from_secs(5))
                        .min(Duration::from_secs(5));
                    tokio::time::sleep(sleep_dur).await;
                }
            });
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
        if super::mcp::worktree::pending(self, id) {
            return Err(Error::Busy);
        }
        self.attach_bound(project, id, owner).await
    }
    pub(super) async fn attach_bound(
        &self,
        project: Uuid,
        id: Uuid,
        owner: &mut MutexGuard<'_, Option<ChatSession>>,
    ) -> Result<()> {
        let _dispatch = self.0.chats.stop_fence(id, false)?;
        if owner.as_ref().is_some_and(ChatSession::is_closed)
            && let Some(old) = owner.take()
        {
            old.close().await?
        }
        if owner.is_none() {
            let info = self.0.chats.saved_info(id)?;
            self.try_restore_missing_worktree(project, &info)?;
            self.chat_directories(&info.workspace, &info.settings)?;
            let mut tools = None;
            let mut session = self
                .0
                .chats
                .resume_configured(id, |runtime, opts| {
                    tools = self.0.mcp.issue(project, id, runtime)?;
                    if let Some(lease) = &tools {
                        lease.configure(opts);
                    }
                    opts.unset_env.insert("KAR_WORKBENCH_TOKEN".into());
                    opts.prompt_suggestions = self.suggestion_source(id);
                    Ok(())
                })
                .await?;
            self.controller(id).bind_model_tools(&mut session, tools);
            **owner = Some(session);
        }
        if let Some(lease) = self.controller(id).model_tools.lock().unwrap().as_ref() {
            lease.touch();
        }
        self.0.pump.errors.lock().unwrap().remove(&id);
        Ok(())
    }
}

#[derive(Serialize)]
struct Queue {
    held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_resets_at_ms: Option<u64>,
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
        let quota_resets_at_ms = app.active_quota_hold_until(c);
        let delivery_error = app
            .0
            .pump
            .errors
            .lock()
            .unwrap()
            .get(&c)
            .map(|v| v.1.clone());
        Ok(Queue {
            held: held || quota_resets_at_ms.is_some(),
            quota_resets_at_ms,
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
        if let Some(schema) = m.requested_output_schema {
            app.register_run_schema(p, c, m.id, schema)?;
        }
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
            return Err(Error::Unsupported(
                "provider cannot resume a rewound conversation".into(),
            ));
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
        if app
            .0
            .chats
            .queued_inputs(chat)?
            .iter()
            .any(crate::chat::QueuedInput::outstanding)
        {
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
    #[serde(default, alias = "sideChat")]
    side_chat: bool,
    #[serde(default)]
    policy: crate::chat::ForkPolicy,
    id: Uuid,
    turn: Option<Uuid>,
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
    if request.side_chat {
        let history = tokio::spawn(async move {
            let _family = app.0.family_gate.lock().await;
            let info = app.binding(p, chat)?;
            if app.archived(chat) {
                return Err(Error::Refused("chat is archived".into()));
            }
            let turn = match (request.turn, app.saved_side_point(chat, request.id)) {
                (Some(t), _) => Some(t),
                (None, Some(saved)) => saved,
                (None, None) => app
                    .0
                    .chats
                    .history(chat)?
                    .turns
                    .into_iter()
                    .rev()
                    .find(|t| t.status == crate::chat::TurnStatus::Completed)
                    .map(|t| t.id),
            };
            app.reserve_fork_directory(
                chat,
                p,
                turn,
                request.id,
                project,
                &info.workspace,
                false,
                true,
            )?;
            if let Some(point) = turn {
                app.0
                    .chats
                    .fork_context(chat, point, request.id, request.policy)
                    .await?;
            } else {
                app.open_empty_side(project, request.id, &info).await?;
            }
            app.0.chats.history(request.id)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        return Ok(Json(ForkResult { project, history }));
    }
    let turn = request
        .turn
        .ok_or_else(|| Error::Refused("a workspace fork requires a completed turn".into()))?;
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
            Some(turn),
            request.id,
            project,
            destination.as_deref().unwrap_or(&info.workspace),
            destination.is_some(),
            false,
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
                turn,
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
                    return live.fork(turn, request.id, destination, configure).await;
                }
            };
            if let Some(source) = owner.as_ref().filter(|s| !s.is_closed()) {
                source
                    .workbench_fork(turn, request.id, destination, request.policy, configure)
                    .await
            } else {
                if let Some(closed) = owner.take() {
                    closed.close().await?;
                }
                app.0
                    .chats
                    .workbench_fork(
                        chat,
                        turn,
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
