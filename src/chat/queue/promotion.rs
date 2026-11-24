//! Reserve a queued UUID before steering. A lost connection is reconciled from the target turn.
use super::{InputStatus, QueuedInput};
use crate::{
    Error, Result,
    chat::{ChatStore, log::State},
};
use serde::{Deserialize, Serialize};
use std::fs::File;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SteerTarget {
    pub turn: Uuid,
    #[serde(default)]
    pub request_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuePromotion {
    pub command_id: Uuid,
    pub target: SteerTarget,
    pub submitted: bool,
    #[serde(default)]
    pub native_acknowledged: bool,
    pub returned_to_queue: bool,
    pub error: Option<String>,
}
impl QueuePromotion {
    pub(super) fn status(&self, input: Uuid, state: &State) -> Option<InputStatus> {
        use crate::SteerStatus::*;
        let observed = state
            .find(self.target.turn)
            .and_then(|t| t.steered_inputs.get(&input));
        Some(match observed {
            Some(Accepted | Received | Handled | FollowUp) => InputStatus::Sent,
            None if self.native_acknowledged => InputStatus::Sent,
            Some(Cancelled) => InputStatus::Cancelled,
            Some(Refused) if !self.returned_to_queue => InputStatus::Refused,
            _ if self.returned_to_queue => return None,
            Some(Submitted | Queued) if state.active() == Some(self.target.turn) => {
                InputStatus::Sending
            }
            _ if observed.is_none()
                && self.submitted
                && state.active() == Some(self.target.turn) =>
            {
                InputStatus::Sending
            }
            _ => InputStatus::Uncertain,
        })
    }
}
pub(crate) enum Claim {
    Deliver(crate::input::Input),
    Prior(QueuedInput),
}

impl ChatStore {
    pub(in crate::chat) fn promoted_message(
        &self,
        info: &super::ChatInfo,
        turn: Uuid,
        input: Uuid,
    ) -> Result<Option<(String, Vec<super::super::Attachment>)>> {
        if !self.inbox_path(info.id).try_exists()? {
            return Ok(None);
        }
        // read_inbox opens an immutable, atomically published snapshot. The actor already
        // owns the chat journal mutex here; it must not wait for a producer's inbox lock.
        Ok(self
            .read_inbox(info)?
            .entries
            .into_iter()
            .find(|e| e.id == input && e.promotion.as_ref().is_some_and(|p| p.target.turn == turn))
            .map(|e| (e.text, e.attachments)))
    }
    pub(crate) fn claim_queue_steer(
        &self,
        chat: Uuid,
        input: Uuid,
        cmd: Uuid,
        target: SteerTarget,
        online: bool,
    ) -> Result<Claim> {
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let mut inbox = self.read_inbox(&state.info)?;
        if inbox.commands.contains_key(&cmd) {
            return Err(Error::Refused(
                "queue command ID belongs to a different request".into(),
            ));
        }
        let workspace_pending=inbox.workspace_gate.is_some();
        let entry = inbox
            .entries
            .iter_mut()
            .find(|e| e.id == input)
            .ok_or_else(|| Error::Refused("unknown queued input".into()))?;
        if let Some(prior) = inbox.promotions.get(&cmd) {
            if *prior != (input, target) {
                return Err(Error::Refused(
                    "queue promotion ID belongs to a different request".into(),
                ));
            }
            if let Some(receipt) = inbox.returned_promotions.get(&cmd) {
                return Ok(Claim::Prior(receipt.clone()));
            }
            if !entry
                .promotion
                .as_ref()
                .is_some_and(|p| p.command_id == cmd)
            {
                return Err(Error::Refused(
                    "historical promotion receipt unavailable".into(),
                ));
            }
            return Ok(Claim::Prior(entry.receipt(&state)));
        }
        if workspace_pending {return Err(Error::Busy);}
        if !online || state.active() != Some(target.turn) {
            return Err(crate::transport::AbortRequest::stale());
        }
        if entry.status(&state) != InputStatus::Pending
            || entry
                .promotion
                .as_ref()
                .is_some_and(|p| !p.returned_to_queue)
        {
            return Err(Error::Refused("only a pending input can be steered".into()));
        }
        let payload = self.attached_input(chat, entry.text.clone(), &entry.attachments)?;
        if let Some(previous) = entry.promotion.as_ref().filter(|p| p.returned_to_queue) {
            inbox
                .returned_promotions
                .entry(previous.command_id)
                .or_insert_with(|| entry.receipt(&state));
        }
        entry.promotion = Some(QueuePromotion {
            command_id: cmd,
            target,
            submitted: false,
            native_acknowledged: false,
            returned_to_queue: false,
            error: None,
        });
        inbox.promotions.insert(cmd, (input, target));
        self.save_inbox(&inbox)?;
        Ok(Claim::Deliver(payload))
    }

    pub(crate) fn finish_queue_steer(
        &self,
        chat: Uuid,
        input: Uuid,
        cmd: Uuid,
        native_acknowledged: bool,
        result: Result<()>,
    ) -> Result<QueuedInput> {
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let mut inbox = self.read_inbox(&state.info)?;
        let entry = inbox
            .entries
            .iter_mut()
            .find(|e| e.id == input)
            .ok_or_else(|| Error::Protocol("claimed queue input disappeared".into()))?;
        let promotion = entry
            .promotion
            .as_mut()
            .filter(|p| p.command_id == cmd)
            .ok_or_else(|| Error::Protocol("queue promotion changed during dispatch".into()))?;
        match result {
            Ok(()) => {
                promotion.submitted = true;
                promotion.native_acknowledged = native_acknowledged;
            }
            Err(error) => {
                // These failures are decided locally before any native write. A transport error
                // or an uncorrelated provider reply cannot authorize a second delivery.
                promotion.returned_to_queue = matches!(
                    &error,
                    Error::Unsupported(_) | Error::Busy | Error::InputTooLarge { .. }
                ) || matches!(&error,Error::Refused(reason) if reason == crate::transport::AbortRequest::STALE);
                promotion.error = Some(error.to_string());
            }
        }
        let receipt = entry.receipt(&state);
        if receipt
            .promotion
            .as_ref()
            .is_some_and(|p| p.returned_to_queue)
        {
            inbox.returned_promotions.insert(cmd, receipt.clone());
        }
        self.save_inbox(&inbox)?;
        Ok(receipt)
    }
}
