//! The inbox lock and atomic rename used by prompt admission also protect queue edits.
use super::{ChatStore, Entry, Inbox, InputStatus};
use crate::{Error, Result, chat::log::State};
use serde::{Deserialize, Serialize};
use std::fs::File;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueAction {
    Hold,
    Resume,
    Start {
        input: Uuid,
    },
    Edit {
        input: Uuid,
        text: String,
    },
    Replace {
        input: Uuid,
        text: String,
        attachments: Vec<Uuid>,
    },
    Reorder {
        input: Uuid,
        before: Option<Uuid>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct QueueReceipt {
    pub command_id: Uuid,
    pub held: bool,
    pub next: Option<Uuid>,
}
#[derive(Serialize, Deserialize)]
pub(super) struct SavedCommand {
    action: QueueAction,
    receipt: QueueReceipt,
}

impl Inbox {
    pub(super) fn next<'a>(&'a self, state: &State) -> Option<&'a Entry> {
        self.entries.iter().find(|entry| {
            (!self.held || self.released == Some(entry.id))
                && entry.status(state) == InputStatus::Pending
        })
    }

    fn position(&self, state: &State, id: Uuid) -> Result<usize> {
        self.entries
            .iter()
            .position(|e| {
                e.id == id
                    && state.find(id).is_none()
                    && !e.withdrawn
                    && e.promotion.as_ref().is_none_or(|p| p.returned_to_queue)
            })
            .ok_or_else(|| Error::Refused("input is no longer queued".into()))
    }
}

impl ChatStore {
    pub fn queue_control(
        &self,
        chat: Uuid,
        command_id: Uuid,
        action: QueueAction,
    ) -> Result<QueueReceipt> {
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let mut inbox = self.read_inbox(&state.info)?;
        if inbox.promotions.contains_key(&command_id) {
            return Err(Error::Refused(
                "queue command ID belongs to a different request".into(),
            ));
        }
        if let Some(prior) = inbox.commands.get(&command_id) {
            return if prior.action == action {
                Ok(prior.receipt.clone())
            } else {
                Err(Error::Refused(
                    "queue command ID belongs to a different request".into(),
                ))
            };
        }
        match &action {
            QueueAction::Hold => {
                inbox.held = true;
                inbox.released = None;
            }
            QueueAction::Resume => {
                inbox.held = false;
                inbox.released = None;
            }
            QueueAction::Start { input } => {
                if state.active().is_some() {
                    return Err(Error::Busy);
                }
                let index = inbox.position(&state, *input)?;
                let entry = inbox.entries.remove(index);
                crate::input::check_text(
                    &entry.text,
                    crate::input::text_limit(&state.info.runtime),
                )?;
                let head = inbox
                    .entries
                    .iter()
                    .position(|e| e.status(&state) == InputStatus::Pending)
                    .unwrap_or(inbox.entries.len());
                inbox.entries.insert(head, entry);
                inbox.released = Some(*input);
            }
            QueueAction::Edit { input, text } | QueueAction::Replace { input, text, .. } => {
                let index = inbox.position(&state, *input)?;
                crate::input::check_text(text, crate::input::text_limit(&state.info.runtime))?;
                let entry = &mut inbox.entries[index];
                if let QueueAction::Replace { attachments, .. } = &action {
                    let refs = self.attachments(chat, attachments)?;
                    if refs != entry.attachments {
                        entry
                            .initial_attachments
                            .get_or_insert_with(|| entry.attachments.clone());
                        entry.attachments = refs;
                    }
                }
                if entry.text != *text {
                    entry.initial_text.get_or_insert_with(|| entry.text.clone());
                    entry.text.clone_from(text);
                }
            }
            QueueAction::Reorder { input, before } => {
                let index = inbox.position(&state, *input)?;
                let entry = inbox.entries.remove(index);
                let dest = if let Some(before) = before {
                    inbox.position(&state, *before)?
                } else {
                    inbox.entries.len()
                };
                inbox.entries.insert(dest, entry);
            }
        }
        let receipt = QueueReceipt {
            command_id,
            held: inbox.held,
            next: inbox.next(&state).map(|e| e.id),
        };
        inbox.commands.insert(
            command_id,
            SavedCommand {
                action,
                receipt: receipt.clone(),
            },
        );
        self.save_inbox(&inbox)?;
        Ok(receipt)
    }

    pub(crate) fn queue_view(&self, chat: Uuid) -> Result<(bool, Vec<super::QueuedInput>)> {
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let inbox = self.read_inbox(&state.info)?;
        Ok((
            inbox.held,
            inbox.entries.iter().map(|e| e.receipt(&state)).collect(),
        ))
    }
}
