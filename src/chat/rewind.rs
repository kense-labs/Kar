//! Rewind the conversation under its existing Kar chat ID. Checkpoint position belongs
//! to the working files and can therefore point to an archived turn after this operation.
mod workspace;
pub(in crate::chat) use workspace::completed_reservation;

use super::{
    ChatStore, ChatTurn, ChatUpdate, ForkMode, ForkPolicy, Handoff, HandoffStatus, InputStatus,
    ProviderSession, TurnStatus, fork, log::State,
};
use crate::{Error, Result, SessionOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindRequest {
    /// Idempotency key for this operation.
    pub id: Uuid,
    /// Keep this completed turn and everything preceding it.
    pub turn: Uuid,
    #[serde(default)]
    pub expected_head: Option<Uuid>,
    #[serde(default)]
    pub policy: ForkPolicy,
    #[serde(default)]
    pub restore_files: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextRewind {
    pub id: Uuid,
    pub chat: Uuid,
    pub turn: Uuid,
    pub previous_head: Uuid,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<super::FileChange>,
    pub expected_head: Option<Uuid>,
    pub policy: ForkPolicy,
    #[serde(default)]
    pub restore_files: bool,
    /// None when the selected turn already was the head.
    pub mode: Option<ForkMode>,
    pub native_id: String,
    /// Historical items excluded by the portable context budget.
    pub omitted_items: usize,
    pub removed_turns: Vec<Uuid>,
    pub removed_inputs: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub queue_status: BTreeMap<Uuid, InputStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_handoffs: Vec<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedRewind {
    pub receipt: ContextRewind,
    pub binding: Option<Box<Handoff>>,
}

impl ContextRewind {
    pub(crate) fn check(&self, request: &RewindRequest) -> Result<()> {
        if self.turn == request.turn
            && self.expected_head == request.expected_head
            && self.policy == request.policy
            && self.restore_files == request.restore_files
        {
            Ok(())
        } else {
            Err(Error::Refused(
                "rewind UUID belongs to a different request".into(),
            ))
        }
    }
}

impl State {
    pub(super) fn resumable_handoffs(&self) -> &[Handoff] {
        // A provider used before a rewind may still remember the discarded suffix.
        let floor = self
            .handoffs
            .iter()
            .rposition(|h| {
                self.rewinds
                    .iter()
                    .any(|r| r.mode.is_some() && r.id == h.request.id)
            })
            .map_or(0, |i| i + 1);
        &self.handoffs[floor..]
    }

    pub(super) fn apply_rewind(
        &mut self,
        receipt: ContextRewind,
        binding: Option<Handoff>,
    ) -> Result<()> {
        let index = self
            .turns
            .iter()
            .position(|t| t.id == receipt.turn)
            .ok_or_else(|| Error::Protocol("rewind references a missing turn".into()))?;
        let keep = index + 1;
        if receipt.chat != self.info.id
            || self.turns.last().map(|t| t.id) != Some(receipt.previous_head)
            || self.rewinds.iter().any(|r| r.id == receipt.id)
            || !self.turns[keep..]
                .iter()
                .map(|t| t.id)
                .eq(receipt.removed_turns.iter().copied())
            || binding.is_some() != receipt.mode.is_some()
            || binding.is_none() != receipt.removed_turns.is_empty()
        {
            return Err(Error::Protocol(
                "inconsistent conversation rewind record".into(),
            ));
        }
        if let Some(h) = binding {
            let target = h
                .target
                .as_ref()
                .ok_or_else(|| Error::Protocol("rewind has no target session".into()))?;
            if target.native_id != receipt.native_id
                || h.through_turn != keep
                || h.request.id != receipt.id
                || h.status != HandoffStatus::Ready
            {
                return Err(Error::Protocol(
                    "rewind target differs from its receipt".into(),
                ));
            }
            for turn in self.turns.drain(keep..) {
                self.archived_turns.insert(turn.id, turn);
            }
            fork::trim_followups(&mut self.turns, self.info.protocol);
            self.inherited_len = self.inherited_len.min(keep);
            self.handoffs.retain(|old| old.through_turn < keep);
            target.activate(&mut self.info);
            self.handoffs.push(h);
            self.notifications.clear();
            self.pending_requests.clear();
            self.tasks.clear();
            self.manual_compaction = None;
            self.last_compaction = None;
            self.goal = None;
            self.context_usage = None;
        }
        if receipt.restore_files {
            let Some(pending) = &self.pending_rewind else {
                return Err(Error::Protocol("missing prepared workspace rewind".into()));
            };
            if pending.receipt.id != receipt.id || pending.receipt.turn != receipt.turn {
                return Err(Error::Protocol("workspace rewind identity changed".into()));
            }
            self.position = Some(super::CheckpointPoint { turn: receipt.turn, before: false });
            self.restoring = None;
            self.pending_rewind = None;
        }
        self.rewinds.push(receipt);
        Ok(())
    }
}

impl ChatStore {
    pub async fn rewind(&self, chat: Uuid, request: RewindRequest) -> Result<ContextRewind> {
        self.rewind_configured(chat, request, |_, _| Ok(())).await
    }

    pub(crate) async fn rewind_configured(
        &self,
        chat: Uuid,
        request: RewindRequest,
        configure: impl FnOnce(&str, &mut SessionOptions) -> Result<()>,
    ) -> Result<ContextRewind> {
        let mut log = self.open_controller(chat).await?;
        let state = &log.state;
        if let Some(previous) = state.rewinds.iter().find(|r| r.id == request.id) {
            previous.check(&request)?;
            return Ok(previous.clone());
        }
        if let Some(pending) = state.pending_rewind.clone() {
            if pending.receipt.id != request.id {
                return Err(Error::Refused(format!("retry workspace rewind {}", pending.receipt.id)));
            }
            pending.receipt.check(&request)?;
            return self.finish_workspace_rewind(&mut log, pending);
        }
        self.require_no_pending_fork(chat)?;
        if state.needs_controller()
            || state.restoring.is_some()
            || state.pending_handoff().is_some()
        {
            return Err(Error::Busy);
        }
        if state.handoffs.iter().any(|h| h.request.id == request.id)
            || state
                .rewinds
                .iter()
                .any(|r| r.retired_handoffs.contains(&request.id))
        {
            return Err(Error::Refused(
                "command UUID belongs to a provider handoff".into(),
            ));
        }
        let index = state
            .turns
            .iter()
            .position(|t| t.id == request.turn)
            .ok_or_else(|| {
                Error::Refused("selected turn is outside the current conversation".into())
            })?;
        let keep = index + 1;
        if state.turns[index].status != TurnStatus::Completed {
            return Err(Error::Refused(
                "select a completed turn to rewind the conversation".into(),
            ));
        }
        let head = state.turns.last().unwrap().id;
        if request
            .expected_head
            .is_some_and(|expected| expected != head)
        {
            return Err(Error::Refused(
                "conversation head changed before rewind".into(),
            ));
        }
        let removed_turns: Vec<_> = state.turns[keep..].iter().map(|t| t.id).collect();
        let removed_inputs: Vec<_> = state.turns[keep..]
            .iter()
            .flat_map(|t| std::iter::once(t.id).chain(t.steered_inputs.keys().copied()))
            .chain(
                state
                    .notifications
                    .iter()
                    .filter(|_| !removed_turns.is_empty())
                    .flat_map(|n| {
                        std::iter::once(n.activity.id)
                            .chain(n.activity.steered_inputs.keys().copied())
                    }),
            )
            .collect();
        let mut receipt = ContextRewind {
            id: request.id,
            chat,
            turn: request.turn,
            expected_head: request.expected_head,
            previous_head: head,
            files: Vec::new(),
            policy: request.policy,
            restore_files: request.restore_files,
            mode: None,
            native_id: state.info.native_id.clone(),
            omitted_items: 0,
            removed_turns,
            removed_inputs,
            queue_status: BTreeMap::new(),
            retired_handoffs: Vec::new(),
        };
        let _checkout = state.info.pin_worktree()?;
        if request.restore_files {
            self.preflight_workspace_rewind(&mut log, request.turn)?;
        }
        let state = &log.state;
        let mut opened = None;
        let binding = if receipt.removed_turns.is_empty() {
            None
        } else {
            let current = &state.info;
            if current.workspace.canonicalize()? != current.workspace {
                return Err(Error::Refused(
                    "saved workspace resolves to a different directory".into(),
                ));
            }
            let source =
                fork::source::Launch::prepare(self, state, request.turn, current.options())?.owner;
            let mut opts = current.options();
            configure(&current.runtime, &mut opts)?;
            opts.remove_env("KAR_WORKBENCH_TOKEN")?;
            opts.cwd = current.workspace.clone();
            opts.resume = None;
            let same_provider = source.runtime == current.runtime
                && source.protocol == current.protocol
                && source.program == current.program
                && source.active_instance() == current.active_instance()
                && source.workspace == current.workspace;
            let mut cut: ChatTurn = state.turns[index].clone();
            let native_owner = if same_provider { &source } else { current };
            let (session, mode) = fork::open_context_cut(
                self,
                native_owner,
                &mut cut,
                &mut opts,
                same_provider,
                request.policy,
            )
            .await?;
            if session.id() == current.native_id
                || session.id() == source.native_id
                || !session.capabilities().resume
            {
                session.close().await?;
                return Err(Error::Refused(
                    "rewind requires a separate recoverable provider session".into(),
                ));
            }
            let mut target = ProviderSession::current(current);
            target.native_id = session.id().into();
            let mut h = Handoff::for_fork(current, &state.turns[..keep])?;
            h.request.id = request.id;
            h.from = ProviderSession::current(&source);
            h.status = HandoffStatus::Ready;
            h.target = Some(target);
            if mode == ForkMode::Native {
                h.context.clear();
                h.omitted_items = 0;
            }
            receipt.retired_handoffs = state
                .handoffs
                .iter()
                .filter(|h| h.through_turn >= keep)
                .map(|h| h.request.id)
                .collect();
            receipt.native_id = session.id().into();
            receipt.mode = Some(mode);
            receipt.omitted_items = h.omitted_items;
            opened = Some(session);
            Some(Box::new(h))
        };
        let update = if request.restore_files {
            ChatUpdate::RewindPrepared { pending: Box::new(PreparedRewind { receipt, binding }) }
        } else {
            ChatUpdate::ContextRewound { rewind: Box::new(receipt), binding }
        };
        let outcome = self.commit_idle_transition(&mut log, update);
        // A lost connection while closing this disposable process cannot undo the durable decision.
        // The next owner resumes the recorded target with fresh workbench credentials.
        if let Some(session) = opened {
            let _ = session.close().await;
        }
        outcome?;
        if let Some(pending) = log.state.pending_rewind.clone() {
            return self.finish_workspace_rewind(&mut log, pending);
        }
        Ok(log.state.rewinds.last().unwrap().clone())
    }
}
