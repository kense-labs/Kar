use super::Workbench;
use crate::{
    Error, Protocol, Result, Session, SessionSettings,
    chat::{Handoff, HandoffRequest, HandoffStatus, InputStatus, TurnStatus},
};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use uuid::Uuid;

#[allow(clippy::large_enum_variant)]
enum Precheck {
    Completed(Handoff),
    Ready(Option<crate::provider_instance::HistoryInstance>),
}

#[derive(Clone, Serialize, Deserialize)]
struct Deferred {
    project: Uuid,
    chat: Uuid,
    request: HandoffRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<SessionSettings>,
}

fn deferred_path(app: &Workbench, chat: Uuid) -> PathBuf {
    app.0
        .state
        .join("deferred-handoffs")
        .join(format!("{chat}.json"))
}

fn read_deferred(app: &Workbench, chat: Uuid) -> Result<Option<Deferred>> {
    match fs::File::open(deferred_path(app, chat)) {
        Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_deferred(app: &Workbench, row: &Deferred) -> Result<()> {
    let path = deferred_path(app, row.chat);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(&mut tmp, row)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn clear_deferred(app: &Workbench, chat: Uuid, expected_id: Uuid) -> bool {
    let Ok(Some(saved)) = read_deferred(app, chat) else {
        return false;
    };
    if saved.request.id != expected_id {
        return false;
    }
    let _ = fs::remove_file(deferred_path(app, chat));
    true
}

fn prevalidate(
    app: &Workbench,
    chat: Uuid,
    request: &HandoffRequest,
    settings: &mut Option<SessionSettings>,
) -> Result<Precheck> {
    if app.archived(chat) {
        return Err(Error::Refused("chat is archived".into()));
    }
    let history = app.0.chats.history(chat)?;
    if history
        .rewinds
        .iter()
        .any(|r| r.id == request.id || r.retired_handoffs.contains(&request.id))
    {
        return Err(Error::Refused(
            "command UUID belongs to a prior conversation revision".into(),
        ));
    }
    if let Some(h) = history
        .handoffs
        .into_iter()
        .find(|h| h.request.id == request.id)
    {
        if &h.request != request || h.requested_settings != *settings {
            return Err(Error::Refused(
                "handoff id belongs to another request".into(),
            ));
        }
        if h.status != HandoffStatus::Prepared {
            return Ok(Precheck::Completed(h));
        }
        return Ok(Precheck::Ready(h.destination_instance));
    }
    if request.runtime.is_empty() {
        return Err(Error::Refused("empty target runtime".into()));
    }
    if !(512..=1024 * 1024).contains(&request.budget_bytes) {
        return Err(Error::Refused(
            "handoff budget must be between 512 and 1048576 UTF-8 bytes".into(),
        ));
    }
    let destination = request
        .instance_id
        .as_deref()
        .map(|id| crate::provider_instance::for_launch(&request.runtime, id))
        .transpose()?;
    if let Some(s) = settings {
        s.additional_directories = app.chat_directories(&history.chat.workspace, s)?;
    }
    if history.pending_fork.is_some()
        || history.pending_rewind.is_some()
        || history.pending_rollback.is_some()
    {
        return Err(Error::Busy);
    }
    if history.chat.runtime == request.runtime
        && history.chat.model == request.model
        && settings
            .as_ref()
            .is_none_or(|s| s == &history.chat.settings)
        && destination.as_ref().is_none_or(|instance| {
            history
                .chat
                .active_instance()
                .map_or(history.chat.runtime.as_str(), |i| &i.id)
                == instance.id
        })
    {
        return Err(Error::Refused(
            "target provider session is already active".into(),
        ));
    }
    Ok(Precheck::Ready(destination))
}

async fn stop_active_session(app: &Workbench, chat: Uuid, session: Arc<Session>) {
    if matches!(session.protocol(), Protocol::Cybion | Protocol::Morph)
        && let Ok(history) = app.0.chats.history(chat)
        && let Some(turn) = history
            .turns
            .last()
            .filter(|t| t.status == TurnStatus::Running)
    {
        let _ = session.abort_turn(turn.id, None).await;
    }
    let close = session.close();
    tokio::pin!(close);
    if tokio::time::timeout(Duration::from_millis(300), &mut close)
        .await
        .is_err()
    {
        let root = session.process_root.clone();
        let _ = tokio::task::spawn_blocking(move || match root {
            Some(root) => root.stop_dedicated_group(),
            None => Ok(false),
        })
        .await;
        let _ = close.await;
    }
}

pub(crate) async fn apply_deferred(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
) -> Result<Option<Handoff>> {
    app.binding(project, chat)?;
    let Some(mut saved) = read_deferred(app, chat)? else {
        return Ok(None);
    };
    if saved.project != project || saved.chat != chat {
        return Ok(None);
    }
    let controller = app.controller(chat);
    let mut owner = controller.owner.try_lock().map_err(|_| Error::Busy)?;
    if let Some(source) = &*owner
        && !source.is_closed()
        && source.needs_controller()?
    {
        return Err(Error::Busy);
    }
    let destination = match prevalidate(app, chat, &saved.request, &mut saved.settings)? {
        Precheck::Completed(h) => {
            clear_deferred(app, chat, saved.request.id);
            return Ok(Some(h));
        }
        Precheck::Ready(d) => d,
    };
    if !clear_deferred(app, chat, saved.request.id) {
        return Ok(None);
    }
    if let Some(source) = owner.take() {
        source.close().await?;
    }
    let tools = app.0.mcp.issue(project, chat, &saved.request.runtime)?;
    let (receipt, mut next) = app
        .0
        .chats
        .handoff_with_settings(chat, saved.request, saved.settings, destination, |opts| {
            opts.prompt_suggestions = app.suggestion_source(chat);
            if let Some(lease) = &tools {
                lease.configure(opts);
            }
        })
        .await?;
    if let Some(session) = &mut next {
        controller.bind_model_tools(session, tools);
    }
    *owner = next;
    Ok(Some(receipt))
}

impl Workbench {
    pub fn handoffs(&self, project: Uuid, chat: Uuid) -> Result<Vec<Handoff>> {
        self.binding(project, chat)?;
        Ok(self.0.chats.history(chat)?.handoffs)
    }

    pub async fn apply_deferred_handoff(
        &self,
        project: Uuid,
        chat: Uuid,
    ) -> Result<Option<Handoff>> {
        apply_deferred(self, project, chat).await
    }

    pub async fn handoff(
        &self,
        project: Uuid,
        chat: Uuid,
        request: HandoffRequest,
    ) -> Result<Handoff> {
        let (mut settings, interrupt_flag, when_mode) = request.take_options();
        if interrupt_flag && when_mode == Some("next_turn") {
            return Err(Error::Refused("conflicting handoff timing options".into()));
        }
        let timing = if interrupt_flag {
            "interrupt"
        } else {
            when_mode.unwrap_or("immediate")
        };
        self.binding(project, chat)?;
        let app = self.clone();
        tokio::spawn(async move {
            let prechecked = {
                let state = app.clone();
                let req = request.clone();
                let mut s = settings.clone();
                let (check, validated) = tokio::task::spawn_blocking(move || -> Result<_> {
                    let check = prevalidate(&state, chat, &req, &mut s)?;
                    Ok((check, s))
                })
                .await
                .map_err(|e| Error::Closed(e.to_string()))??;
                settings = validated;
                check
            };
            if let Precheck::Completed(h) = prechecked {
                return Ok(h);
            }
            let controller = app.controller(chat);
            if timing == "next_turn" {
                let busy = match controller.owner.try_lock() {
                    Err(_) => true,
                    Ok(slot) => {
                        slot.as_ref()
                            .is_some_and(|s| !s.is_closed() && s.needs_controller().unwrap_or(true))
                            || app
                                .0
                                .chats
                                .queued_inputs(chat)?
                                .iter()
                                .any(|i| matches!(i.status, InputStatus::Sending))
                    }
                };
                if busy {
                    if let Some(existing) = read_deferred(&app, chat)?
                        && existing.request.id == request.id
                        && (existing.request != request || existing.settings != settings)
                    {
                        return Err(Error::Refused(
                            "handoff id belongs to another request".into(),
                        ));
                    }
                    let destination = match prechecked {
                        Precheck::Ready(d) => d,
                        Precheck::Completed(h) => return Ok(h),
                    };
                    let row = Deferred {
                        project,
                        chat,
                        request: request.clone(),
                        settings: settings.clone(),
                    };
                    write_deferred(&app, &row)?;
                    let preview = {
                        let state = app.clone();
                        let req = request.clone();
                        let set = settings.clone();
                        let dest = destination.clone();
                        tokio::task::spawn_blocking(move || {
                            state.0.chats.preview_handoff(chat, req, set, dest)
                        })
                        .await
                        .map_err(|e| Error::Closed(e.to_string()))??
                    };
                    let bg_app = app.clone();
                    let bg_controller = controller.clone();
                    let req_id = request.id;
                    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
                    tokio::spawn(async move {
                        let _ = queued_tx.send(());
                        let mut owner = bg_controller.owner.lock().await;
                        while owner.as_ref().is_some_and(|s| {
                            !s.is_closed() && s.needs_controller().unwrap_or(false)
                        }) {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                        let Ok(Some(mut saved)) = read_deferred(&bg_app, chat) else {
                            return;
                        };
                        if saved.project != project
                            || saved.chat != chat
                            || saved.request.id != req_id
                        {
                            return;
                        }
                        let Ok(Precheck::Ready(dest)) =
                            prevalidate(&bg_app, chat, &saved.request, &mut saved.settings)
                        else {
                            clear_deferred(&bg_app, chat, req_id);
                            return;
                        };
                        if !clear_deferred(&bg_app, chat, req_id) {
                            return;
                        }
                        if let Some(source) = owner.take() {
                            let _ = source.close().await;
                        }
                        let Ok(tools) = bg_app.0.mcp.issue(project, chat, &saved.request.runtime)
                        else {
                            return;
                        };
                        if let Ok((_, mut next)) = bg_app
                            .0
                            .chats
                            .handoff_with_settings(
                                chat,
                                saved.request,
                                saved.settings,
                                dest,
                                |opts| {
                                    opts.prompt_suggestions = bg_app.suggestion_source(chat);
                                    if let Some(lease) = &tools {
                                        lease.configure(opts);
                                    }
                                },
                            )
                            .await
                        {
                            if let Some(session) = &mut next {
                                bg_controller.bind_model_tools(session, tools);
                            }
                            *owner = next;
                            bg_app.wake_queue();
                        }
                    });
                    let _ = queued_rx.await;
                    tokio::task::yield_now().await;
                    return Ok(preview);
                }
            }
            let mut owner = if timing == "interrupt" {
                match controller.owner.try_lock() {
                    Ok(owner) => {
                        if let Some(source) = owner.as_ref()
                            && !source.is_closed()
                            && source.needs_controller()?
                            && let Some(live) = source.live_session().upgrade()
                        {
                            stop_active_session(&app, chat, live).await;
                        }
                        owner
                    }
                    Err(_) => {
                        let live = controller.live.lock().unwrap().upgrade();
                        if let Some(live) = live {
                            stop_active_session(&app, chat, live).await;
                        }
                        tokio::time::timeout(Duration::from_secs(10), controller.owner.lock())
                            .await
                            .map_err(|_| Error::Busy)?
                    }
                }
            } else {
                // A switch requested mid-turn must not silently occur after the caller's observed turn.
                controller.owner.try_lock().map_err(|_| Error::Busy)?
            };
            let destination = match prevalidate(&app, chat, &request, &mut settings)? {
                Precheck::Completed(h) => return Ok(h),
                Precheck::Ready(d) => d,
            };
            if timing != "interrupt"
                && let Some(source) = &*owner
                && (source.needs_controller()?
                    || app
                        .0
                        .chats
                        .queued_inputs(chat)?
                        .iter()
                        .any(|i| matches!(i.status, InputStatus::Pending | InputStatus::Sending)))
            {
                return Err(Error::Busy);
            }
            let _ = fs::remove_file(deferred_path(&app, chat));
            if let Some(source) = owner.take() {
                source.close().await?;
            }
            let tools = app.0.mcp.issue(project, chat, &request.runtime)?;
            let (receipt, mut next) = app
                .0
                .chats
                .handoff_with_settings(chat, request, settings, destination, |opts| {
                    opts.prompt_suggestions = app.suggestion_source(chat);
                    if let Some(lease) = &tools {
                        lease.configure(opts);
                    }
                })
                .await?;
            if let Some(session) = &mut next {
                controller.bind_model_tools(session, tools);
            }
            *owner = next;
            Ok(receipt)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
}
