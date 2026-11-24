//! Guest reads are authorized by the shared timeline. A private upload ticket alone grants nothing.
use super::*;
use crate::chat::{Attachment as ChatAttachment, ChatHistory};
use std::collections::{BTreeSet, HashMap};

pub(super) enum Visible {
    Chat(ChatAttachment),
    Published(Uuid, retention::Published),
}

impl Visible {
    fn id(&self) -> Uuid {
        match self {
            Self::Chat(a) => a.id,
            Self::Published(id, _) => *id,
        }
    }
    pub(super) fn view(&self) -> Value {
        match self {
            Self::Chat(a) => json!({"id":a.id,"name":a.name,"bytes":a.bytes}),
            Self::Published(id, f) => json!({"id":id,"name":f.name,"bytes":f.size}),
        }
    }
}

// A legacy guest file was recorded in the share registry instead of in ChatTurn.attachments.
// Associate it only with turns in that chat's visible prefix, including nested forks.
fn inherited_limits(app: &Workbench, h: &ChatHistory) -> BTreeMap<Uuid, usize> {
    let mut limits = BTreeMap::from([(h.chat.id, h.turns.len())]);
    let mut origin = h.chat.forked_from.clone();
    let mut limit = h.turns.len();
    let mut cut = origin
        .as_ref()
        .and_then(|f| h.turns.iter().position(|t| t.id == f.turn));
    while let (Some(fork), Some(end)) = (origin, cut) {
        limit = limit.min(end + 1);
        if limits.contains_key(&fork.chat) {
            break;
        }
        limits.insert(fork.chat, limit);
        let Ok(parent) = app.0.chats.initial_info(fork.chat) else {
            break;
        };
        origin = parent.forked_from;
        cut = origin.as_ref().and_then(|f| {
            h.turns[..limit]
                .iter()
                .position(|t| t.id == f.turn)
                .or_else(|| {
                    // The fork may select an earlier turn than its parent's own fork point.
                    let saved = app.0.chats.history(fork.chat).ok()?;
                    let end = saved.turns.iter().position(|t| t.id == f.turn)?;
                    let common = saved.turns[..=end]
                        .iter()
                        .zip(&h.turns[..limit])
                        .take_while(|(a, b)| a.id == b.id)
                        .count();
                    common.checked_sub(1)
                })
        });
    }
    limits
}

pub(super) fn chat_files(app: &Workbench, history: &ChatHistory) -> HashMap<Uuid, Vec<Visible>> {
    let limits = inherited_limits(app, history);
    let index: HashMap<_, _> = history
        .turns
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id, i))
        .collect();
    let mut legacy: HashMap<Uuid, Vec<(Uuid, retention::Published)>> = HashMap::new();
    {
        let db = app.0.access.registry.lock().unwrap();
        for share in db.shares.values() {
            let Some(limit) = limits.get(&share.chat) else {
                continue;
            };
            for (id, file) in &share.published {
                if index.get(&file.turn).is_some_and(|i| i < limit) {
                    legacy
                        .entry(file.turn)
                        .or_default()
                        .push((*id, file.clone()));
                }
            }
        }
    }
    let mut files = HashMap::new();
    for turn in &history.turns {
        let mut seen = BTreeSet::new();
        let mut visible = Vec::new();
        for a in turn
            .attachments
            .iter()
            .chain(turn.steered_attachments.values().flatten())
        {
            if seen.insert(a.id) {
                visible.push(Visible::Chat(a.clone()));
            }
        }
        // Preserve the order of the original proposal, including pre-migration files.
        if let Some(mut published) = legacy.remove(&turn.id) {
            published.sort_by_key(|(_, f)| f.position);
            for (id, file) in published {
                let marker = format!(
                    "Attachment: .kar-attachments/{}/{id}-{}",
                    turn.id, file.name
                );
                if turn
                    .prompt()
                    .is_some_and(|p| p.lines().any(|l| l == marker))
                    && seen.insert(id)
                {
                    visible.push(Visible::Published(id, file));
                }
            }
        }
        if !visible.is_empty() {
            files.insert(turn.id, visible);
        }
    }
    files
}

pub(super) fn read_attachment(
    app: &Workbench,
    who: Principal,
    id: Uuid,
) -> AccessResult<(Vec<u8>, Option<String>)> {
    let chat = {
        let db = app.0.access.registry.lock().unwrap();
        let share = db
            .shares
            .get(&who.share)
            .filter(|s| s.active(app))
            .ok_or(AccessError::Invalid)?;
        if let Some(f) = share
            .proposals
            .values()
            .filter(|p| {
                p.author == who.guest && matches!(p.status, Decision::Pending | Decision::Sending)
            })
            .flat_map(|p| &p.files)
            .find(|f| f.id == id)
        {
            return STANDARD
                .decode(&f.base64)
                .map(|data| (data, f.mime.clone()))
                .map_err(|_| Error::Protocol("invalid saved guest attachment".into()).into());
        }
        share.chat
    };
    let history = app.0.chats.history(chat)?;
    let file = chat_files(app, &history)
        .into_values()
        .flatten()
        .find(|f| f.id() == id)
        .ok_or(AccessError::Missing)?;
    match file {
        Visible::Chat(recorded) => {
            let stored = app.0.chats.visible_attachment(chat, id)?;
            if recorded != stored {
                return Err(AccessError::Missing);
            }
            Ok((app.0.chats.verified_bytes(&stored)?, Some(stored.mime)))
        }
        Visible::Published(id, _) => Ok((
            std::fs::read(retention::published_path(app, id)).map_err(Error::Io)?,
            None,
        )),
    }
}
