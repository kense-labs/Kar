//! Adopt native text history with the provider's original binding, without launching a runtime.
use crate::{
    ApprovalMode, Error, Result,
    chat::{ChatInfo, ChatStore, ChatTurn, Item, Role, TimelineItem, TurnStatus, log},
    observe::Agent,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::File, path::PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Origin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<crate::provider_instance::HistoryInstance>,
    pub project: Option<Uuid>,
    pub runtime: String,
    pub native_id: String,
    pub transcript: PathBuf,
    pub stamp: String,
    pub title: String,
    pub omitted_messages: usize,
}

impl Origin {
    pub fn instance_id(&self) -> &str {
        self.instance
            .as_ref()
            .map_or(self.runtime.as_str(), |v| v.id.as_str())
    }
}
pub(crate) struct Message {
    pub role: Role,
    pub text: String,
    pub at: u64,
    pub provider_id: Option<String>,
}
pub(crate) struct Transcript {
    pub origin: Origin,
    pub workspace: PathBuf,
    pub model: Option<String>,
    pub messages: Vec<Message>,
}

fn identifier(parts: &[&[u8]]) -> Uuid {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.finalize()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

impl Transcript {
    pub fn id(&self) -> Uuid {
        identifier(&[
            b"kar-native-import/1",
            self.origin.instance_id().as_bytes(),
            self.origin.native_id.as_bytes(),
        ])
    }

    fn turns(self, id: Uuid) -> Vec<ChatTurn> {
        let root = &self.origin.native_id;
        let mut turns = Vec::<ChatTurn>::new();
        for (index, m) in self.messages.into_iter().enumerate() {
            if m.role == Role::User || turns.is_empty() {
                let turn_id = identifier(&[id.as_bytes(), &(index as u64).to_be_bytes()]);
                let mut turn = ChatTurn::new(turn_id, m.at, root, None);
                turn.input_sent = m.role == Role::User;
                turn.native_continuation = m.role != Role::User;
                turn.status = TurnStatus::Interrupted;
                turns.push(turn);
            }
            let turn = turns.last_mut().expect("created for first message");
            if m.role == Role::Assistant {
                turn.status = TurnStatus::Completed;
                turn.finished_ms = Some(m.at);
            }
            turn.items.push(TimelineItem {
                source: Agent {
                    session_id: Some(root.clone()),
                    path: vec![],
                },
                item: Item::Message {
                    role: m.role,
                    text: m.text,
                    provider_id: m.provider_id,
                    complete: true,
                },
            });
        }
        turns
    }
}

pub(crate) enum ImportStatus {
    Created,
    AlreadyPresent,
    Changed,
}

fn messages(turns: &[ChatTurn]) -> impl Iterator<Item = (Role, &str)> {
    turns.iter().flat_map(|turn| {
        turn.items.iter().filter_map(|item| match &item.item {
            Item::Message { role, text, .. } => Some((*role, text.as_str())),
            _ => None,
        })
    })
}
impl ChatStore {
    pub(crate) fn adopt_native(&self, data: Transcript) -> Result<ImportStatus> {
        let id = data.id();
        let rt = crate::runtime(&data.origin.runtime)?;
        let pin = crate::workbench_git::pin_checkout(&data.workspace)?;
        let info = ChatInfo {
            id,
            workspace: data.workspace.clone(),
            runtime: rt.id,
            protocol: rt.protocol,
            native_id: data.origin.native_id.clone(),
            created_ms: data
                .messages
                .first()
                .ok_or_else(|| Error::Refused("empty native transcript".into()))?
                .at,
            program: data
                .origin
                .instance
                .as_ref()
                .and_then(|i| i.program.clone())
                .unwrap_or(rt.program),
            model: data.model.clone(),
            provider_instance: None,
            imported: Some(data.origin.clone()),
            worktree_id: pin.as_ref().map(|(id, _)| *id),
            settings: Default::default(),
            unset_env: ["KAR_WORKBENCH_TOKEN".into()].into_iter().collect(),
            approvals: ApprovalMode::Deny,
            forked_from: None,
        };
        let turns = data.turns(id);
        let lease = self.lease(id, true)?;
        lease.try_lock().map_err(|err| match err {
            std::fs::TryLockError::WouldBlock => Error::Busy,
            std::fs::TryLockError::Error(e) => e.into(),
        })?;
        let target = self.path(id);
        if target.try_exists()? {
            let mut file = File::open(target)?;
            let saved = log::read(&mut file, id)?.0;
            let same = saved.info.workspace == info.workspace
                && saved.info.imported.as_ref().is_some_and(|x| {
                    x.project == info.imported.as_ref().and_then(|o| o.project)
                        && x.runtime == info.runtime
                        && x.native_id == info.native_id
                })
                && messages(&saved.turns).eq(messages(&turns));
            return Ok(if same {
                ImportStatus::AlreadyPresent
            } else {
                ImportStatus::Changed
            });
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        log::header(tmp.as_file_mut(), &info)?;
        log::append_locked(tmp.as_file_mut(), &log::ChatUpdate::Imported { turns })?;
        tmp.persist_noclobber(target).map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(ImportStatus::Created)
    }
}
