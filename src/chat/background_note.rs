//! Durable recovery prompts for native work whose controller disappeared after its turn.
use super::{ChatTurn, TurnStatus, log::State};
use crate::{BackgroundExit, BackgroundKind, InputReceipt, NativeTask};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

pub(super) const CONTINUE: &str = "Continue the interrupted background work.";
const LABEL_LIMIT: usize = 160;
const NOTE_LIMIT: usize = 10;

/// The exact context sent on the wire and the native task occurrences covered by it.
/// The journal stores this in the same admission record as the original input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundNote {
    pub text: String,
    pub occurrences: Vec<Uuid>,
    pub automatic: bool,
}

impl BackgroundNote {
    pub(super) fn prompt(&self, original: &str) -> String {
        if self.automatic {
            self.text.clone()
        } else {
            format!("{}\n\n{}", self.text, original)
        }
    }
}

pub(crate) fn stable_id(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Uuid {
    let mut hash = Sha256::new();
    for part in parts {
        let bytes = part.as_ref();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.finalize()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

fn occurrence(task: &NativeTask, source: Uuid) -> Option<Uuid> {
    let start = task.started_at.as_ref()?;
    Some(stable_id([
        task.source.session_id.as_deref()?.as_bytes(),
        source.as_bytes(),
        task.id.as_bytes(),
        start.stream_id.as_bytes(),
        &start.after_seq.to_be_bytes(),
    ]))
}

fn compact(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.encode_utf16().count() <= LABEL_LIMIT {
        return text;
    }
    let mut used = 0;
    let mut label: String = text
        .chars()
        .take_while(|c| {
            used += c.len_utf16();
            used < LABEL_LIMIT
        })
        .collect();
    label.push('…');
    label
}

fn label(task: &NativeTask, work: &crate::BackgroundWork) -> String {
    let description = work
        .description
        .as_deref()
        .or(work.command.as_deref())
        .map(compact)
        .filter(|s| !s.is_empty());
    if let Some(parent) = &work.parent_task_id {
        let suffix = format!(" (subagent {})", compact(parent));
        return format!(
            "subagent: {}",
            compact(&format!(
                "{}{suffix}",
                description.as_deref().unwrap_or("background task")
            ))
        );
    }
    let kind = match work.kind {
        BackgroundKind::Command => "shell",
        BackgroundKind::Monitor => "monitor",
    };
    let label = match description {
        Some(desc) => format!("{desc} (id {})", task.id),
        None => task.id.clone(),
    };
    format!("{kind}: {}", compact(&label))
}

pub(super) struct Delivery {
    pub note: BackgroundNote,
    pub input: Uuid,
}

impl State {
    pub(super) fn background_delivery(&self, automatic: bool) -> Option<Delivery> {
        if self.info.protocol != crate::Protocol::Claude {
            return None;
        }
        if !self
            .tasks
            .iter()
            .any(|task| task.background_exit().is_some())
        {
            return None;
        }
        if self.pending_handoff().is_some() || self.restoring.is_some() {
            return None;
        }
        // A rewound workspace must never autonomously dispatch an input into the old model head.
        if automatic
            && self
                .position
                .is_some_and(|p| p.before || self.turns.last().is_some_and(|t| t.id != p.turn))
        {
            return None;
        }
        let mut claimed = HashSet::new();
        for turn in &self.turns {
            if let Some(note) = &turn.background_note {
                // An ambiguous admission may already have reached the model. Only
                // an explicit refusal releases an ordinary user's later retry.
                if automatic || turn.native_input_receipt != Some(InputReceipt::Refused) {
                    claimed.extend(note.occurrences.iter().copied());
                }
            }
        }
        let mut ids = Vec::new();
        let mut lines = Vec::new();
        let mut server = false;
        let mut provider = false;
        let mut wake = false;
        for task in &self.tasks {
            let Some(cause) = task.background_exit() else {
                continue;
            };
            if task.source.session_id.as_deref() != Some(self.info.native_id.as_str())
                || !task.source.path.is_empty()
            {
                continue;
            }
            let Some(work) = task.background_work.as_ref() else {
                continue;
            };
            let Some(source) = work.turn_id.and_then(|id| self.find(id)) else {
                continue;
            };
            if source.status != TurnStatus::Completed {
                continue;
            }
            let Some(key) = occurrence(task, source.id) else {
                continue;
            };
            if claimed.contains(&key) {
                continue;
            }
            if cause == BackgroundExit::HostClosed {
                continue;
            }
            wake |= work.awaited;
            ids.push(key);
            if lines.len() < NOTE_LIMIT {
                lines.push(format!("- {}", label(task, work)));
            }
            match cause {
                BackgroundExit::ProviderExit => provider = true,
                BackgroundExit::ServerRestart => server = true,
                BackgroundExit::HostClosed => unreachable!(),
            }
        }
        if ids.is_empty() || automatic && !wake {
            return None;
        }
        let cause = match (server, provider) {
            (true, true) => "Kar restarted and a provider process exited",
            (true, false) => "Kar restarted",
            _ => "the provider process exited",
        };
        if ids.len() > lines.len() {
            lines.push(format!("- and {} more", ids.len() - lines.len()));
        }
        let text = format!(
            "Background work stopped before completion because {cause}. Resume or restart the work you still need and wait for its result.\n{}",
            lines.join("\n")
        );
        ids.sort_unstable();
        let input = stable_id(
            std::iter::once(self.info.id.as_bytes().as_slice())
                .chain(ids.iter().map(|id| id.as_bytes().as_slice())),
        );
        if automatic && self.find(input).is_some() {
            return None;
        }
        Some(Delivery {
            input,
            note: BackgroundNote {
                occurrences: ids,
                text,
                automatic,
            },
        })
    }
}

impl ChatTurn {
    pub(crate) fn host_message(&self) -> bool {
        !self.native_continuation && !self.background_note.as_ref().is_some_and(|n| n.automatic)
    }
}

impl super::ChatStore {
    pub(crate) fn needs_background_wake(&self, chat: Uuid) -> crate::Result<bool> {
        let state = self.read_state(std::fs::File::open(self.path(chat))?, chat)?;
        Ok(state.background_delivery(true).is_some())
    }
}
