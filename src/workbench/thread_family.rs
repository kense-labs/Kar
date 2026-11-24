//! Project-scoped side chats and durable deletion commands.
use super::{Catalog, ProjectChat, Workbench, http::ApiError};
use crate::{Error, Result, chat::TurnStatus};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::delete,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Family {
    pub(super) deleted: BTreeMap<Uuid, Uuid>,
    pending_cleanup: BTreeSet<Uuid>,
    archive_owner: BTreeMap<Uuid, Uuid>,
    receipts: BTreeMap<Uuid, Receipt>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    project: Uuid,
    chat: Uuid,
    only_if_unused: bool,
    deleted: Vec<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    #[serde(alias = "commandId")]
    command_id: Uuid,
    #[serde(default, alias = "onlyIfUnused")]
    only_if_unused: bool,
}

impl Workbench {
    fn notify_family_change(&self) {
        let sequence = self.0.catalog.lock().unwrap().thread_state.sequence();
        self.0.thread_updates.send_if_modified(|n| {
            if *n < sequence {
                *n = sequence;
                true
            } else {
                false
            }
        });
        self.0.access.notify();
    }

    pub(super) async fn open_empty_side(
        &self,
        project: Uuid,
        id: Uuid,
        source: &crate::chat::ChatInfo,
    ) -> Result<()> {
        let slot = self.controller(id);
        let mut owner = slot.owner.lock().await;
        match self.0.chats.initial_info(id) {
            Ok(saved) if saved.workspace == source.workspace && saved.forked_from.is_none() => {
                return Ok(());
            }
            Ok(_) => return Err(Error::Refused("side chat ID already used".into())),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let mut opts = source.options();
        opts.pi_resume_file = None;
        opts.remove_env("KAR_WORKBENCH_TOKEN")?;
        opts.prompt_suggestions = self.suggestion_source(id);
        let tools = self.0.mcp.issue(project, id, &source.runtime)?;
        if let Some(lease) = &tools {
            lease.configure(&mut opts);
        }
        let mut session = self
            .0
            .chats
            .create_with_id(id, &source.runtime, None, opts)
            .await?;
        slot.bind_model_tools(&mut session, tools);
        *owner = Some(session);
        Ok(())
    }

    fn sides(&self, project: Uuid, parent: Uuid) -> Result<Vec<Uuid>> {
        let candidates: Vec<_> = {
            let db = self.0.catalog.lock().unwrap();
            db.thread_state
                .rows
                .iter()
                .filter(|(id, m)| {
                    m.side_chat_parent == Some(parent) && !db.family.deleted.contains_key(id)
                })
                .map(|(id, _)| *id)
                .collect()
        };
        for child in &candidates {
            // A reserved, unpublished side chat is retired with the parent too.
            if self
                .0
                .chats
                .directory()
                .join(format!("{child}.karchat"))
                .try_exists()?
            {
                self.binding(project, *child)?;
            }
        }
        Ok(candidates)
    }

    pub(super) async fn archive_family(
        &self,
        project: Uuid,
        chat: Uuid,
        archived: bool,
    ) -> Result<ProjectChat> {
        let _family = self.0.family_gate.lock().await;
        let info = self.binding(project, chat)?;
        let sides = self.sides(project, chat)?;
        let mut ids = vec![chat];
        {
            let db = self.0.catalog.lock().unwrap();
            if archived
                && db
                    .model_tasks
                    .values()
                    .any(|t| t.blocks_thread_archive(chat))
            {
                return Err(Error::Busy);
            }
            for id in sides {
                if archived
                    && !db.archived.contains(&id)
                    && !db.model_tasks.values().any(|t| t.blocks_thread_archive(id))
                    || !archived && db.family.archive_owner.get(&id) == Some(&chat)
                {
                    ids.push(id);
                }
            }
        }
        let slots: Vec<_> = ids.iter().map(|id| self.controller(*id)).collect();
        let mut guards = vec![];
        let mut fences = vec![];
        if archived {
            for (id, slot) in ids.iter().zip(&slots) {
                fences.push(self.0.chats.stop_fence(*id, true)?);
                let mut owner = match slot.owner.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        let live = slot.live.lock().unwrap().upgrade().ok_or(Error::Busy)?;
                        live.close().await?;
                        slot.owner.lock().await
                    }
                };
                if let Some(session) = owner.take() {
                    session.close().await?;
                }
                if self
                    .0
                    .chats
                    .directory()
                    .join(format!("{id}.karchat"))
                    .try_exists()?
                {
                    guards.push(self.0.chats.confirm_offline(*id)?);
                }
                *slot.model_tools.lock().unwrap() = None;
            }
        }
        self.update(|db| {
            let at = jiff::Timestamp::now().as_millisecond() as u64;
            for id in &ids {
                let changed = if archived {
                    db.archived.insert(*id)
                } else {
                    db.archived.remove(id)
                };
                if changed {
                    db.forge.linked_chat_changed(*id)?;
                    let row = db.thread_state.rows.entry(*id).or_default();
                    row.archived_ms = archived.then_some(at);
                    row.updated_ms = at;
                    if archived
                        && let Some(p) = row.side_chat_parent
                        && db.archived.contains(&p)
                    {
                        db.family.archive_owner.insert(*id, p);
                    } else {
                        db.family.archive_owner.remove(id);
                    }
                    db.thread_state.record_lifecycle(project, *id, false);
                }
            }
            Ok(())
        })?;
        self.notify_family_change();
        if !archived {
            self.wake_queue();
        }
        Ok(ProjectChat {
            project,
            metadata: self.thread_metadata(chat),
            info,
            archived,
        })
    }

    fn used(&self, id: Uuid) -> Result<bool> {
        let h = self.0.chats.history(id)?;
        let inherited: BTreeSet<_> = self
            .0
            .chats
            .fork_prefix(id)?
            .into_iter()
            .flat_map(|(_, ids)| ids)
            .collect();
        Ok(h.turns.iter().any(|t| !inherited.contains(&t.id))
            || !h.notifications.is_empty()
            || h.rewinds
                .iter()
                .any(|r| r.removed_turns.iter().any(|t| !inherited.contains(t)))
            || self
                .0
                .chats
                .queued_inputs(id)?
                .iter()
                .any(|i| i.outstanding()))
    }

    pub(super) fn recover_thread_deletions(&self) -> Result<()> {
        let (deleted, pending) = {
            let db = self.0.catalog.lock().unwrap();
            (db.family.deleted.clone(), db.family.pending_cleanup.clone())
        };
        if pending.is_empty() {
            return Ok(());
        }
        let mut fences = Vec::new();
        let mut leases = Vec::new();
        for id in &pending {
            fences.push(self.0.chats.stop_fence(*id, true)?);
            match self.0.chats.confirm_offline(*id) {
                Ok(lease) => leases.push(lease),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
        self.complete_deletions(&deleted, &pending)
    }

    fn complete_deletions(
        &self,
        deleted: &BTreeMap<Uuid, Uuid>,
        pending: &BTreeSet<Uuid>,
    ) -> Result<()> {
        self.0.chats.retire_chats(deleted, pending)?;
        self.update(|db| {
            db.family.pending_cleanup.retain(|id| !pending.contains(id));
            Ok(())
        })
    }

    async fn delete_chat(&self, project: Uuid, chat: Uuid, req: Delete) -> Result<Receipt> {
        let _family = self.0.family_gate.lock().await;
        self.project(project)?;
        self.recover_thread_deletions()?;
        {
            let db = self.0.catalog.lock().unwrap();
            if let Some(r) = db.family.receipts.get(&req.command_id) {
                if r.project != project || r.chat != chat || r.only_if_unused != req.only_if_unused
                {
                    return Err(Error::Refused("deletion command ID already used".into()));
                }
                return Ok(r.clone());
            }
        }
        self.binding(project, chat)?;
        let mut ids = vec![chat];
        ids.extend(self.sides(project, chat)?);
        let mut fences = vec![self.0.chats.stop_fence(chat, true)?];
        let mut reply = Receipt {
            project,
            chat,
            only_if_unused: req.only_if_unused,
            deleted: vec![],
        };
        if req.only_if_unused && self.used(chat)? {
            self.update(|db| {
                db.family.receipts.insert(req.command_id, reply.clone());
                Ok(())
            })?;
            return Ok(reply);
        }
        for id in &ids[1..] {
            fences.push(self.0.chats.stop_fence(*id, true)?);
        }
        let slots: Vec<_> = ids.iter().map(|id| self.controller(*id)).collect();
        let mut guards = Vec::new();
        let mut leases = Vec::new();
        {
            let db = self.0.catalog.lock().unwrap();
            if ids.iter().any(|id| {
                db.model_tasks
                    .values()
                    .any(|t| t.blocks_thread_archive(*id))
            }) {
                return Err(Error::Busy);
            }
        }
        for (id, slot) in ids.iter().zip(&slots) {
            let guard = slot.owner.try_lock().map_err(|_| Error::Busy)?;
            match self.0.chats.history(*id) {
                Ok(h) => {
                    if h.pending_rewind.is_some()
                        || h.pending_fork.is_some()
                        || h.pending_rollback.is_some()
                        || h.background_working
                        || h.turns.iter().any(|t| t.status == TurnStatus::Running)
                        || self
                            .0
                            .chats
                            .queued_inputs(*id)?
                            .iter()
                            .any(|i| i.outstanding())
                    {
                        return Err(Error::Busy);
                    }
                }
                Err(Error::Io(e)) if *id != chat && e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            guards.push(guard);
        }
        for ((id, slot), guard) in ids.iter().zip(&slots).zip(&mut guards) {
            if let Some(session) = guard.take() {
                session.close().await?;
            }
            *slot.model_tools.lock().unwrap() = None;
            match self.0.chats.confirm_offline(*id) {
                Ok(lease) => leases.push(lease),
                Err(Error::Io(e)) if *id != chat && e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
        // Native background events queued before close have now reached the durable journal.
        if req.only_if_unused && self.used(chat)? {
            self.update(|db| {
                db.family.receipts.insert(req.command_id, reply.clone());
                Ok(())
            })?;
            return Ok(reply);
        }
        reply.deleted = ids;
        let pending: BTreeSet<_> = reply.deleted.iter().copied().collect();
        let deleted = self.update(|db: &mut Catalog| {
            for id in &pending {
                db.family.deleted.insert(*id, project);
                db.family.pending_cleanup.insert(*id);
                db.family.archive_owner.remove(id);
                db.archived.insert(*id);
                db.thread_state.rows.remove(id);
                db.thread_state.record_lifecycle(project, *id, true);
                if let Some(perms) = db.model_tool_permissions.get_mut(&project) {
                    perms.remove(id);
                }
                db.forge.linked_chat_changed(*id)?;
            }
            db.family.receipts.insert(req.command_id, reply.clone());
            Ok(db.family.deleted.clone())
        })?;
        self.notify_family_change();
        self.complete_deletions(&deleted, &pending)?;
        Ok(reply)
    }
}

async fn remove(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
    Json(request): Json<Delete>,
) -> std::result::Result<Json<Receipt>, ApiError> {
    let receipt = tokio::spawn(async move { app.delete_chat(project, chat, request).await })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
    Ok(Json(receipt))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new().route("/projects/{project}/chats/{chat}", delete(remove))
}
