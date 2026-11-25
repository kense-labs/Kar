//! Completion membership and its pending prompt share the same locked inbox replacement.
//! The native admission path takes this lock before recording Begin and rechecks the text.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WakePolicy {
    Always,
    #[default]
    SettledOnly,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    #[default]
    Pending,
    Claimed,
    Delivered,
    Acknowledged,
    Disposed,
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Completion {
    pub state: Disposition,
    pub wake_policy: WakePolicy,
    pub message_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_messages: Vec<Uuid>,
    pub observed_by_run_id: Option<Uuid>,
    pub queue_status: Option<InputStatus>,
    pub native_received: bool,
    pub reason: Option<String>,
}
impl Completion {
    pub(crate) fn settled(&self) -> bool {
        matches!(
            self.state,
            Disposition::Delivered | Disposition::Acknowledged | Disposition::Disposed
        )
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CompletionCommand {
    WakePolicy { completion_wake: WakePolicy },
    Acknowledge { observed_by_run_id: Option<Uuid> },
    Dispose,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    completion: Completion,
    parent_run: Option<Uuid>,
    text: String,
}
#[derive(Default, Serialize, Deserialize)]
pub(super) struct Ledger {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    stopped_runs: BTreeSet<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    stopped_tasks: BTreeSet<Uuid>,
    tasks: BTreeMap<Uuid, Saved>,
    commands: BTreeMap<Uuid, (Uuid, CompletionCommand, Completion)>,
}
pub(crate) struct CompletionSource {
    pub task: Uuid,
    pub parent_run: Option<Uuid>,
    pub terminal: bool,
    pub wake: Uuid,
    pub policy: WakePolicy,
    pub text: String,
    pub dispose: Option<&'static str>,
    pub legacy_reported: bool,
}
impl Ledger {
    pub(super) fn protects(&self, id: Uuid) -> bool {
        self.tasks
            .values()
            .any(|r| r.completion.message_id == Some(id))
    }
    fn prompt(&self, id: Uuid) -> Option<String> {
        let parts: Vec<_> = self
            .tasks
            .values()
            .filter(|r| {
                r.completion.message_id == Some(id) && r.completion.state == Disposition::Claimed
            })
            .map(|r| r.text.as_str())
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }
}
impl Inbox {
    // Only edit an entry that the actor has not claimed. A pending claim is reduced or withdrawn
    // atomically with its task resolution. Once started, keep the bytes and message ID unchanged.
    fn refresh_completion_prompt(
        &mut self,
        state: &super::super::log::State,
        id: Uuid,
    ) -> Result<()> {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            if e.status(state) != InputStatus::Pending {
                return Ok(());
            }
            if let Some(text) = self.delegations.prompt(id) {
                crate::input::check_text(&text, crate::input::text_limit(&state.info.runtime))?;
                if text != e.text {
                    e.initial_text.get_or_insert_with(|| e.text.clone());
                    e.text = text;
                }
            } else {
                e.withdrawn = true;
            }
        } else if let Some(text) = self.delegations.prompt(id) {
            self.admit(state, id, text, vec![])?;
            let pos = self
                .entries
                .iter()
                .position(|e| e.id == id)
                .expect("new wake");
            let e = self.entries.remove(pos);
            let at = self
                .entries
                .iter()
                .position(|e| {
                    e.status(state) == InputStatus::Pending && !self.delegations.protects(e.id)
                })
                .unwrap_or(self.entries.len());
            self.entries.insert(at, e);
        }
        Ok(())
    }
}
impl ChatStore {
    pub(crate) fn delegation_completion(
        &self,
        chat: Uuid,
        src: CompletionSource,
        command: Option<(Uuid, CompletionCommand)>,
        observe: Option<Uuid>,
    ) -> Result<Completion> {
        let _dispatch = self.stop_fence(chat, false)?;
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        if state.pending_rewind.is_some() {
            return Err(Error::Busy);
        }
        let mut inbox = self.read_inbox(&state.info)?;
        if let Some((id, action)) = &command
            && let Some((t, a, r)) = inbox.delegations.commands.get(id)
        {
            return if (*t, a) == (src.task, action) {
                Ok(r.clone())
            } else {
                Err(Error::Refused("delegation command ID was reused".into()))
            };
        }
        let original = inbox.delegations.tasks.get(&src.task).cloned();
        let mut row = original.clone().unwrap_or_else(|| Saved {
            parent_run: src.parent_run,
            text: src.text.clone(),
            completion: Completion {
                wake_policy: src.policy,
                ..Default::default()
            },
        });
        if src.terminal {
            row.text = src.text.clone();
        }
        let c = &mut row.completion;
        // An older Kar process may have admitted its per-task wake before saving its delivered flag.
        if c.state == Disposition::Pending
            && c.previous_messages.is_empty()
            && let Some(e) = inbox.entries.iter().find(|e| e.id == src.wake)
        {
            if e.text == src.text && e.attachments.is_empty() {
                c.state = Disposition::Claimed;
                c.message_id = Some(src.wake);
            } else {
                c.state = Disposition::Disposed;
                c.reason = Some("existing completion ID has a different payload".into());
            }
        }
        let previous = c.message_id;
        if let Some(id) = c.message_id
            && let Some(e) = inbox.entries.iter().find(|e| e.id == id)
        {
            let status = e.status(&state);
            c.queue_status = Some(status);
            if matches!(status, InputStatus::Sent | InputStatus::Duplicate)
                && state.find(id).is_some_and(|t| {
                    t.native_turn_id.is_some()
                        || t.status == TurnStatus::Completed
                        || t.pi_input_timestamp_ms.is_some()
                        || matches!(
                            t.native_input_receipt,
                            Some(
                                crate::InputReceipt::Queued
                                    | crate::InputReceipt::Started
                                    | crate::InputReceipt::Handled
                                    | crate::InputReceipt::Duplicate
                            )
                        )
                })
            {
                c.native_received = true;
                if c.state == Disposition::Claimed
                    && state
                        .find(id)
                        .is_some_and(|t| t.status != TurnStatus::Running)
                {
                    c.state = Disposition::Delivered;
                }
            }
            if status == InputStatus::Cancelled
                && c.state == Disposition::Claimed
                && !c.native_received
            {
                c.previous_messages.push(id);
                c.message_id = None;
                c.state = Disposition::Pending;
            }
            if matches!(
                status,
                InputStatus::Withdrawn
                    | InputStatus::Refused
                    | InputStatus::Cancelled
                    | InputStatus::TooLarge
            ) && c.state == Disposition::Claimed
            {
                c.state = Disposition::Disposed;
                c.reason = Some(format!("queue {status:?}"));
            }
        }
        let change = command.as_ref().map(|(_, c)| c);
        match change {
            Some(CompletionCommand::WakePolicy { completion_wake }) => {
                if *completion_wake == c.wake_policy {
                    return Err(Error::Refused(
                        "task already uses this completion wake policy".into(),
                    ));
                }
                c.wake_policy = *completion_wake
            }
            Some(CompletionCommand::Dispose) => {
                c.state = Disposition::Disposed;
                c.observed_by_run_id = None;
                c.reason = Some("explicit disposition".into());
            }
            _ => {}
        }
        let observer = match change {
            Some(CompletionCommand::Acknowledge {
                observed_by_run_id: Some(id),
            }) => {
                if !src.terminal
                    || (state.active() != Some(*id) && c.observed_by_run_id != Some(*id))
                {
                    return Err(Error::Refused(
                        "the completed result needs its observing parent turn".into(),
                    ));
                }
                Some(*id)
            }
            _ => observe.filter(|id| state.active() == Some(*id) && src.terminal),
        };
        if let Some(id) = observer
            && !matches!(c.state, Disposition::Disposed | Disposition::Acknowledged)
        {
            c.state = Disposition::Acknowledged;
            c.observed_by_run_id = Some(id);
        }
        if !c.settled() {
            let parent = src
                .parent_run
                .and_then(|id| state.turns.iter().find(|t| t.id == id));
            let parent_stopped = inbox.delegations.stopped_tasks.contains(&src.task)
                || src
                    .parent_run
                    .is_some_and(|id| inbox.delegations.stopped_runs.contains(&id));
            let dispose = src.dispose.or_else(|| {
                if !src.terminal {
                    None
                } else if c.message_id.is_none() && src.legacy_reported {
                    Some("legacy report without verified receipt")
                } else if src.parent_run.is_none() && c.message_id.is_none() {
                    Some("no spawning parent turn")
                } else if src.parent_run.is_some() && parent.is_none() {
                    Some("spawning turn no longer in this context")
                } else if parent_stopped
                    || parent.is_some_and(|t| t.status == TurnStatus::Aborted)
                {
                    Some("spawning parent stopped")
                } else {
                    None
                }
            });
            if let Some(reason) = dispose {
                c.state = Disposition::Disposed;
                c.reason = Some(reason.into());
            } else if src.terminal
                && c.state == Disposition::Pending
                && (c.wake_policy == WakePolicy::Always
                    || parent.is_none_or(|t| t.status != TurnStatus::Running))
            {
                let msgs: std::collections::BTreeSet<_> = inbox
                    .delegations
                    .tasks
                    .values()
                    .filter(|other| other.parent_run == src.parent_run)
                    .filter_map(|other| other.completion.message_id)
                    .collect();
                let mut pending = None;
                let mut inflight = false;
                for id in msgs {
                    let Some(e) = inbox.entries.iter().find(|e| e.id == id) else {
                        continue;
                    };
                    match e.status(&state) {
                        InputStatus::Pending => {
                            let len = e
                                .text
                                .len()
                                .saturating_add(2)
                                .saturating_add(src.text.len());
                            if crate::input::text_limit(&state.info.runtime)
                                .is_some_and(|n| len > n)
                            {
                                inflight = true;
                            } else {
                                pending = Some(id);
                            }
                        }
                        InputStatus::Sending => inflight = true,
                        InputStatus::Uncertain if !e.uncertainty_acknowledged => inflight = true,
                        InputStatus::Sent
                            if state
                                .find(id)
                                .is_some_and(|t| t.status == TurnStatus::Interrupted)
                                && inbox.delegations.tasks.values().any(|r| {
                                    r.completion.message_id == Some(id)
                                        && r.completion.state == Disposition::Claimed
                                        && !r.completion.native_received
                                }) =>
                        {
                            inflight = true
                        }
                        _ if state
                            .find(id)
                            .is_some_and(|t| t.status == TurnStatus::Running) =>
                        {
                            inflight = true
                        }
                        _ => {}
                    }
                }
                if !inflight {
                    c.state = Disposition::Claimed;
                    let generation = c.previous_messages.len().to_be_bytes();
                    let fresh = if c.previous_messages.is_empty() {
                        src.wake
                    } else {
                        super::super::stable_id([
                            src.task.as_bytes().as_slice(),
                            b"delegated successor",
                            &generation,
                        ])
                    };
                    c.message_id = Some(pending.unwrap_or(fresh));
                    c.queue_status = Some(InputStatus::Pending);
                }
            }
        }
        if matches!(c.state, Disposition::Disposed | Disposition::Acknowledged)
            && c.queue_status == Some(InputStatus::Pending)
            && c.message_id.is_some()
        {
            // A later native receipt for the remaining siblings must never acknowledge the removed text.
            c.previous_messages.push(c.message_id.take().unwrap());
            c.queue_status = Some(InputStatus::Withdrawn);
        }
        let result = c.clone();
        if original.as_ref() != Some(&row) || command.is_some() {
            inbox.delegations.tasks.insert(src.task, row);
            if let Some(id) = result.message_id.or(previous) {
                inbox.refresh_completion_prompt(&state, id)?;
            }
            if let Some((id, action)) = command {
                inbox
                    .delegations
                    .commands
                    .insert(id, (src.task, action, result.clone()));
            }
            self.save_inbox(&inbox)?;
        }
        Ok(result)
    }

    pub(crate) fn record_parent_force_stop(
        &self,
        chat: Uuid,
        parent_run: Option<Uuid>,
        tasks: &[Uuid],
    ) -> Result<BTreeMap<Uuid, Completion>> {
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let mut inbox = self.read_inbox(&state.info)?;
        let mut dirty = false;
        if let Some(run) = parent_run {
            dirty |= inbox.delegations.stopped_runs.insert(run);
        }
        for &task in tasks {
            dirty |= inbox.delegations.stopped_tasks.insert(task);
        }
        let mut affected_messages = BTreeSet::new();
        let mut disposed = BTreeMap::new();
        for (&task_id, row) in &mut inbox.delegations.tasks {
            let matches_stop = inbox.delegations.stopped_tasks.contains(&task_id)
                || row
                    .parent_run
                    .is_some_and(|id| inbox.delegations.stopped_runs.contains(&id));
            if !matches_stop || row.completion.settled() || row.completion.native_received {
                continue;
            }
            if let Some(id) = row.completion.message_id {
                if let Some(e) = inbox.entries.iter().find(|e| e.id == id) {
                    let status = e.status(&state);
                    row.completion.queue_status = Some(status);
                    if status != InputStatus::Pending {
                        continue;
                    }
                }
                affected_messages.insert(id);
                row.completion.previous_messages.push(id);
                row.completion.message_id = None;
                row.completion.queue_status = Some(InputStatus::Withdrawn);
            }
            row.completion.state = Disposition::Disposed;
            row.completion.reason = Some("spawning parent stopped".into());
            disposed.insert(task_id, row.completion.clone());
            dirty = true;
        }
        for id in affected_messages {
            inbox.refresh_completion_prompt(&state, id)?;
            dirty = true;
        }
        if dirty {
            self.save_inbox(&inbox)?;
        }
        Ok(disposed)
    }
}
