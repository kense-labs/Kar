use super::*;
use crate::chat::{ChatInfo, ChatReplica, Item, Role, TurnStatus};
use std::{fs, io::ErrorKind, path::PathBuf, time::SystemTime};

#[derive(PartialEq, Eq)]
struct Stamp(u64, Option<SystemTime>);
impl Stamp {
    fn at(p: &Path) -> Result<Option<Self>> {
        match fs::metadata(p) {
            Ok(m) => Ok(Some(Self(m.len(), m.modified().ok()))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
#[derive(PartialEq, Eq)]
struct Version {
    log: Stamp,
    inbox: Option<Stamp>,
}
struct Entry {
    version: Version,
    info: ChatInfo,
    activity: Value,
    first_text: String,
    host_turn: Option<(Uuid, u64)>,
    pending_questions: Vec<crate::Cursor>,
    has_approval: bool,
    active: Option<ChatReplica>,
}
#[derive(Default)]
pub(super) struct Sources {
    entries: BTreeMap<Uuid, Entry>,
}
impl Sources {
    fn read(&mut self, app: &Workbench, id: Uuid, version: Version) -> Result<&Entry> {
        let store = &app.0.chats;
        let mut changed = true;
        if let Some(entry) = self.entries.get_mut(&id) {
            changed = entry.version != version;
            if let Some(active) = &mut entry.active {
                let before = active.cursor();
                active.refresh(store)?;
                changed |= before != active.cursor();
            }
        }
        if changed {
            let replica = match self.entries.get_mut(&id).and_then(|e| e.active.take()) {
                Some(active) => active,
                None => ChatReplica::from_snapshot(store.feed_snapshot(id)?),
            };
            let turns = replica.turns();
            let last = turns.last();
            let title = turns
                .first()
                .and_then(|t| {
                    t.items.iter().find_map(|i| match &i.item {
                        Item::Message {
                            role: Role::User,
                            text,
                            ..
                        } if !text.trim().is_empty() => {
                            Some(text.trim().chars().take(80).collect::<String>())
                        }
                        _ => None,
                    })
                })
                .unwrap_or_default();
            let background = replica.tasks().iter().filter(|t| !t.status.is_terminal())
                .map(|t| json!({"id":t.id,"source":t.source,"kind":t.task_type,"status":t.status,"started_at":t.started_at,"background_work":t.background_work.as_ref().map(|w|json!({"kind":w.kind,"pending":w.pending,"awaited":w.awaited,"turn_id":w.turn_id}))}))
                .collect::<Vec<_>>();
            let activity = json!({"turns":turns.len(),"latest_turn":last.map(|t|json!({"id":t.id,"status":t.status,"started_ms":t.started_ms,"finished_ms":t.finished_ms})),
                "pending_requests":replica.pending_requests().iter().map(|r|json!({"id":r.id,"cursor":r.cursor,"kind":r.kind})).collect::<Vec<_>>(),
                "background_working":replica.tasks().iter().any(crate::NativeTask::blocking_background),"background_tasks":background,"queued":store.shell_queue_count(&replica)?,"goal":replica.goal()});
            let info = replica.chat().clone();
            let keep =
                replica.needs_controller() || last.is_some_and(|t| t.status == TurnStatus::Running);
            self.entries.insert(
                id,
                Entry {
                    version,
                    info,
                    activity,
                    first_text: title,
                    host_turn: turns
                        .iter()
                        .rev()
                        .find(|t| t.host_message())
                        .map(|t| (t.id, t.started_ms)),
                    pending_questions: replica
                        .pending_requests()
                        .iter()
                        .filter(|r| r.kind == crate::RequestKind::Question)
                        .map(|r| r.cursor.clone())
                        .collect(),
                    has_approval: replica
                        .pending_requests()
                        .iter()
                        .any(|r| r.kind != crate::RequestKind::Question),
                    active: keep.then_some(replica),
                },
            );
        }
        Ok(self.entries.get(&id).expect("loaded chat"))
    }

    pub(super) fn scan(&mut self, app: &Workbench) -> Result<BTreeMap<Key, Value>> {
        let at = super::super::access::now();
        let guests = app.0.access.pending_guest_counts(at);
        let mut rows = BTreeMap::new();
        let mut files = BTreeMap::<Uuid, PathBuf>::new();
        for entry in fs::read_dir(app.0.chats.directory())? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "karchat")
                && let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse().ok())
            {
                files.insert(id, path);
            }
        }
        self.entries.retain(|k, _| files.contains_key(k));
        // Capture the catalog before reading journal state. No journal or inbox lock is taken
        // while holding the live catalog lock (metadata commands can take them in that order).
        let (projects, ownership, metadata, archived_chats, watched_pulls) = {
            let catalog = app.0.catalog.lock().unwrap();
            (
                catalog
                    .projects
                    .values()
                    .filter(|p| !p.metadata.deleted && app.allowed(&p.root).is_ok())
                    .cloned()
                    .collect::<Vec<_>>(),
                catalog.lifecycle.ownership_snapshot(),
                catalog.thread_state.rows.clone(),
                catalog.archived.clone(),
                super::super::forge::watches::waiting(&catalog),
            )
        };
        for p in &projects {
            rows.insert(
                Key {
                    kind: Kind::Project,
                    id: p.id,
                },
                serde_json::to_value(p)?,
            );
        }
        for (id, path) in files {
            let Ok(Some(log)) = Stamp::at(&path) else {
                continue;
            };
            let fresh;
            let moved = ownership.handoff_workspace(id);
            let info = if let Some(entry) = self.entries.get(&id).filter(|e| moved.is_none_or(|cwd| cwd == e.info.workspace)) {
                &entry.info
            } else {
                let header = if moved.is_some() { app.0.chats.saved_info(id) } else { app.0.chats.initial_info(id) };
                let Ok(header) = header else {
                    continue;
                };
                fresh = header;
                &fresh
            };
            if app.allowed(&info.workspace).is_err() {
                continue;
            }
            let mut project = None;
            for candidate in &projects {
                if app
                    .owns_chat_in(&ownership, candidate, info)
                    .unwrap_or(false)
                {
                    project = Some(candidate.id);
                    break;
                }
            }
            let Some(project) = project else {
                continue;
            };
            let key = Key {
                kind: Kind::Chat,
                id,
            };
            let inbox = Stamp::at(&app.0.chats.inbox_path(id));
            let entry = match inbox.and_then(|inbox| self.read(app, id, Version { log, inbox })) {
                Ok(entry) => entry,
                Err(_) => {
                    self.entries.remove(&id);
                    rows.insert(key, json!({"project":project,"unavailable":true}));
                    continue;
                }
            };
            let mut meta = metadata.get(&id).cloned().unwrap_or_default();
            if let Some(host) = entry.host_turn
                && meta.new_host_turn(Some(host))
            {
                meta = app.wake_sent_chat(project, id, &meta, host.1)?;
            }
            let archived = archived_chats.contains(&id);
            let guest_count = if archived {
                0
            } else {
                guests.get(&id).copied().unwrap_or(0)
            };
            let failed = &entry.activity["latest_turn"];
            let failed_after = failed["status"] == "failed"
                && failed["finished_ms"].as_u64().unwrap_or(0) > meta.snoozed_ms.unwrap_or(0);
            let effective = meta.snoozed_until_ms.is_some_and(|until| until > at)
                && guest_count == 0
                && !failed_after
                && !entry.has_approval
                && entry
                    .pending_questions
                    .iter()
                    .all(|c| meta.acknowledges_question(c));
            rows.insert(key,json!({"project":project,"workspace":entry.info.workspace,"runtime":entry.info.runtime,"model":entry.info.model,"native_id":entry.info.native_id,"created_ms":entry.info.created_ms,"forked_from":entry.info.forked_from,"title":meta.title.as_ref().unwrap_or(&entry.first_text),"archived":archived,"metadata":meta,"snoozed":effective,"pending_guest_messages":guest_count,"waiting_for_pull":watched_pulls.contains(&id),"activity":entry.activity}));
        }
        Ok(rows)
    }
}
