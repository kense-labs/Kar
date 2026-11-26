//! A merge reserves context for the target's next admitted message. The Begin record
//! binds the exact context to an input so recovery can reproduce the native prompt.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergedContext {
    pub id: Uuid,
    pub source: Uuid,
    pub turn: Uuid,
    pub base: Uuid,
    pub text: String,
}
impl MergedContext {
    pub(in crate::chat) fn prompt(&self, user: &str) -> String {
        format!(
            "Context merged from chat {} through turn {}:\n{}\n\nCurrent user message:\n{}",
            self.source, self.turn, self.text, user
        )
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Transfer {
    note: MergedContext,
    pub(super) bound: Option<Uuid>,
    superseded_by: Option<Uuid>,
}
impl Transfer {
    fn delivered(&self, state: &super::super::log::State) -> Option<Uuid> {
        self.bound.or_else(|| {
            state
                .turns
                .iter()
                .find(|t| {
                    t.merged_context
                        .as_ref()
                        .is_some_and(|n| n.id == self.note.id)
                })
                .map(|t| t.id)
        })
    }
    fn pending(&self, state: &super::super::log::State) -> bool {
        self.superseded_by.is_none() && self.delivered(state).is_none()
    }
}
impl Inbox {
    pub(super) fn next_merge(&self, state: &super::super::log::State) -> Option<&MergedContext> {
        self.context_transfers
            .values()
            .find(|t| t.pending(state))
            .map(|t| &t.note)
    }
}
impl ChatStore {
    pub(crate) fn prepare_merge(&self, chat: Uuid, note: MergedContext) -> Result<()> {
        let _guard = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let mut inbox = self.read_inbox(&state.info)?;
        if let Some(old) = inbox.context_transfers.get(&note.id) {
            if old.note == note {
                return Ok(());
            }
            return Err(Error::Refused(
                "context transfer ID was already used".into(),
            ));
        }
        if state.active().is_some()
            || inbox.workspace_gate.is_some()
            || inbox.entries.iter().any(|e| {
                matches!(
                    e.status(&state),
                    InputStatus::Pending | InputStatus::Sending | InputStatus::Uncertain
                )
            })
        {
            return Err(Error::Busy);
        }
        if let Some(old) = inbox
            .context_transfers
            .values_mut()
            .find(|t| t.pending(&state))
        {
            if old.note.source != note.source {
                return Err(Error::Refused(
                    "another fork's merge is waiting for the next message".into(),
                ));
            }
            old.superseded_by = Some(note.id);
        }
        inbox.context_transfers.insert(
            note.id,
            Transfer {
                note,
                bound: None,
                superseded_by: None,
            },
        );
        self.save_inbox(&inbox)
    }
    pub(crate) fn context_transfer_status(
        &self,
        chat: Uuid,
    ) -> Result<BTreeMap<Uuid, serde_json::Value>> {
        let _guard = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let inbox = self.read_inbox(&state.info)?;
        Ok(inbox.context_transfers.into_iter().map(|(id,r)|{
            let turn=r.delivered(&state);
            let status=if r.superseded_by.is_some(){"superseded"}else if turn.is_some(){"consumed"}else{"pending"};
            (id,serde_json::json!({"source_thread_id":r.note.source,"target_thread_id":chat,"turn_id":r.note.turn,"base_turn":r.note.base,"status":status,"input_id":turn,"superseded_by":r.superseded_by}))
        }).collect())
    }
    pub(in crate::chat) fn next_contexts(
        &self,
        state: &super::super::log::State,
    ) -> Result<(Option<MergedContext>, Option<String>)> {
        let inbox = self.read_inbox(&state.info)?;
        Ok((inbox.next_merge(state).cloned(), inbox.app_snapshot(state)?))
    }
}
