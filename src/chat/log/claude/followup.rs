//! Apply a recovered group of steering inputs in one durable update.
use super::super::State;
use crate::{
    Error, Protocol, Result, SteerStatus,
    chat::{ChatEdit, ChatFrame, ChatTurn, Item, Role, TurnStatus},
};
use std::collections::HashSet;
use uuid::Uuid;

pub(in crate::chat::log) struct FollowUp {
    origin: ChatTurn,
    pub turn: ChatTurn,
}

impl State {
    pub(in crate::chat::log) fn claude_group(
        &self,
        from: Uuid,
        inputs: &[Uuid],
        at: u64,
    ) -> Option<FollowUp> {
        if self.info.protocol != Protocol::Claude {
            return None;
        }
        let id = *inputs.first()?;
        let latest = self.turns.last()?;
        if latest.id != from && latest.steered_from != Some(from) {
            return None;
        }
        let mut origin = self.find(from)?.clone();
        if origin.native_continuation || origin.steered_from.is_some() {
            return None;
        }
        let mut turn = match self.find(id) {
            Some(turn)
                if latest.id == id
                    && turn.steered_from == Some(from)
                    && matches!(turn.status, TurnStatus::Running | TurnStatus::Interrupted) =>
            {
                turn.clone()
            }
            Some(_) => return None,
            None if latest.id != from && latest.status == TurnStatus::Running => return None,
            None if self.notifications.iter().any(|n| n.activity.id == id) => return None,
            None => {
                let mut t = ChatTurn::new(id, at, &self.info.native_id, None);
                t.native_turn_id = Some(id.to_string());
                t.native_continuation = false;
                t.steered_from = Some(from);
                t.input_sent = true;
                t
            }
        };
        let mut seen = HashSet::new();
        let mut last_origin_index = 0;
        for (index, key) in inputs.iter().enumerate() {
            let status = origin.steered_inputs.get_mut(key)?;
            if !seen.insert(*key)
                || !matches!(
                    status,
                    SteerStatus::Unconfirmed
                        | SteerStatus::Submitted
                        | SteerStatus::Queued
                        | SteerStatus::FollowUp
                )
            {
                return None;
            }
            let native = key.to_string();
            let locate = |t: &ChatTurn| {
                let mut positions = t.items.iter().enumerate().filter_map(|(i, item)| {
                    (item.source.path.is_empty()
                        && item.source.session_id.as_deref() == Some(&self.info.native_id)
                        && matches!(&item.item, Item::Message {role: Role::User, provider_id: Some(id), ..} if *id == native)).then_some(i)
                });
                let first = positions.next();
                positions.next().is_none().then_some(first)
            };
            *status = SteerStatus::FollowUp;
            match (locate(&origin)?, locate(&turn)?) {
                (Some(at), None) if at >= last_origin_index && index <= turn.items.len() => {
                    last_origin_index = at;
                    turn.items.insert(index, origin.items.remove(at));
                }
                (None, Some(at)) if at == index => {}
                _ => return None,
            }
            if index != 0 {
                turn.steered_inputs.insert(*key, SteerStatus::Accepted);
            }
        }
        if turn
            .steered_inputs
            .keys()
            .any(|id| origin.steered_inputs.contains_key(id) && !seen.contains(id))
        {
            return None;
        }
        if origin.status == TurnStatus::Running {
            origin.finish(TurnStatus::Interrupted, &self.info.native_id);
        }
        if turn.status == TurnStatus::Running {
            turn.finish(TurnStatus::Interrupted, &self.info.native_id);
        }
        Some(FollowUp { origin, turn })
    }
}

impl FollowUp {
    pub(in crate::chat::log) fn changed(&self, state: &State) -> bool {
        Some(&self.origin) != state.find(self.origin.id)
            || Some(&self.turn) != state.find(self.turn.id)
    }
    pub(in crate::chat::log) fn apply(
        mut self,
        state: &mut State,
        frame: &ChatFrame,
    ) -> Result<()> {
        if frame.native_turn.as_deref() != Some(self.turn.id.to_string().as_str())
            || frame.edits.is_empty()
            || frame.edits.iter().any(|edit| {
                !matches!(
                    edit,
                    ChatEdit::Text {
                        role: Role::Assistant | Role::Reasoning,
                        key: Some(_),
                        final_text: true,
                        ..
                    } | ChatEdit::ToolSnapshot { .. }
                )
            })
            || frame.morph_progress.is_some()
            || !frame.source.path.is_empty()
            || frame.source.session_id.as_deref() != Some(state.info.native_id.as_str())
            || self
                .turn
                .transcript_adds_content(&state.info.native_id, &frame.edits)
                .is_none()
        {
            return Err(Error::Protocol(
                "Claude follow-up transcript conflicts with its recorded turn".into(),
            ));
        }
        self.turn.apply_claude_history(frame, &state.info.native_id);
        let parent = state.ids[&self.origin.id];
        state.turns[parent] = self.origin;
        if let Some(index) = state.ids.get(&self.turn.id) {
            state.turns[*index] = self.turn;
        } else {
            // No working-tree snapshot exists for a native turn discovered after the crash.
            state.ids.insert(self.turn.id, state.turns.len());
            state.turns.push(self.turn);
        }
        Ok(())
    }
}
