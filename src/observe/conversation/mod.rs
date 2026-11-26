//! Fold public control attempts and native evidence without correlating by message text.
mod attempts;
mod evidence;
pub use attempts::*;
pub(crate) use attempts::{submitted_id, valid_carries};
pub use evidence::{EchoEvidence, InputEcho};

use super::turn_end::{body, control};
use crate::{Cursor, Direction, Event, ImageReference, InputOrigin, Record, RecordKind};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use uuid::Uuid;

/// Explicit input IDs survive a resume in the same native session and agent lineage. A request
/// without an input ID, or evidence without a native session, is local to its stream.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct ConversationKey {
    pub session_id: Option<String>,
    pub agent_path: Vec<String>,
    pub stream_id: Option<Uuid>,
    pub id: String,
}
impl ConversationKey {
    fn new(r: &Record, id: &str, request: bool) -> Self {
        Self {
            session_id: r.session_id.clone(),
            agent_path: r.agent_path.clone(),
            stream_id: (request || r.session_id.is_none()).then_some(r.stream_id),
            id: id.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationInput {
    pub key: ConversationKey,
    pub input_id: Option<String>,
    pub text: String,
    pub images: Vec<ImageReference>,
    pub origin: Option<InputOrigin>,
    /// Original claims are visible even if no held input satisfied them.
    pub carries: Vec<String>,
    pub state: DeliveryState,
    pub reason: Option<DropReason>,
    pub attempts: Vec<InputAttempt>,
    pub turn: Option<InputTurn>,
    pub handled: Option<Cursor>,
    pub observations: Vec<InputEcho>,
    #[serde(skip)]
    drop: Option<(usize, DropReason)>,
}

#[derive(Clone, Copy, Debug)]
struct AttemptAt {
    input: usize,
    attempt: usize,
}

/// Insertion-ordered inputs; raw request/response records retain their original stream and sequence.
#[derive(Default, Debug, Serialize)]
pub struct Conversation {
    inputs: Vec<ConversationInput>,
    unlinked: Vec<InputEcho>,
    cursors: HashMap<Uuid, u64>,
    #[serde(skip)]
    keys: HashMap<ConversationKey, usize>,
    // First entry owns the request. Following entries are held inputs carried by it.
    #[serde(skip)]
    requests: HashMap<ConversationKey, Vec<AttemptAt>>,
}

impl Conversation {
    pub fn inputs(&self) -> &[ConversationInput] {
        &self.inputs
    }
    pub fn get(&self, key: &ConversationKey) -> Option<&ConversationInput> {
        self.keys.get(key).map(|&i| &self.inputs[i])
    }
    /// Native observations without a host input ID remain available to the caller in journal order.
    pub fn unlinked(&self) -> &[InputEcho] {
        &self.unlinked
    }
    pub fn cursors(&self) -> &HashMap<Uuid, u64> {
        &self.cursors
    }

    fn find_or_insert(&mut self, key: ConversationKey, input_id: Option<&str>) -> usize {
        if let Some(&i) = self.keys.get(&key) {
            return i;
        }
        let i = self.inputs.len();
        self.keys.insert(key.clone(), i);
        self.inputs.push(ConversationInput {
            key,
            input_id: input_id.map(str::to_owned),
            text: String::new(),
            images: vec![],
            origin: None,
            carries: vec![],
            state: DeliveryState::Untracked,
            reason: None,
            attempts: vec![],
            turn: None,
            handled: None,
            observations: vec![],
            drop: None,
        });
        i
    }
    fn carried(&self, record: &Record, id: &str) -> Option<usize> {
        self.keys
            .get(&ConversationKey::new(record, id, false))
            .or_else(|| self.keys.get(&ConversationKey::new(record, id, true)))
            .copied()
    }

    fn request(&mut self, r: &Arc<Record>, kind: InputAttemptKind) {
        let input_id = submitted_id(r);
        let operation = r
            .control_request_id()
            .map(|id| ConversationKey::new(r, id, true));
        if operation
            .as_ref()
            .is_some_and(|op| self.requests.contains_key(op))
        {
            return;
        }
        let Some(key) = input_id
            .map(|id| ConversationKey::new(r, id, false))
            .or_else(|| operation.clone())
        else {
            return;
        };
        let mut related = Vec::new();
        if valid_carries(r, |id| {
            self.carried(r, id)
                .is_some_and(|i| self.inputs[i].claimable())
        }) {
            for id in &r.carries {
                if let Some(i) = self.carried(r, id)
                    && !related.contains(&i)
                {
                    related.push(i);
                }
            }
        }
        let owner = if kind == InputAttemptKind::Withdraw {
            let Some(&i) = self.keys.get(&key) else {
                return;
            };
            if self.inputs[i].attempts.is_empty() {
                return;
            }
            i
        } else {
            let i = self.find_or_insert(key, input_id);
            let input = &mut self.inputs[i];
            (input.text, input.images) = attempts::content(r);
            input.origin = r.origin.clone();
            input.carries = r.carries.clone();
            i
        };
        related.retain(|&i| i != owner);
        related.insert(0, owner);
        let mut targets = Vec::with_capacity(related.len());
        for i in related {
            let input = &mut self.inputs[i];
            targets.push(AttemptAt {
                input: i,
                attempt: input.attempts.len(),
            });
            input.attempts.push(InputAttempt {
                kind,
                request: r.clone(),
                state: AttemptState::Pending,
                response: None,
                reason: None,
                handled: None,
            });
            if kind != InputAttemptKind::Withdraw {
                input.turn = None;
                input.handled = None;
            }
            input.refresh();
        }
        if let Some(op) = operation {
            self.requests.insert(op, targets);
        }
    }
    fn decision(&mut self, r: &Arc<Record>) {
        let Some(id) = r.control_response_to() else {
            return;
        };
        let state = match control(r) {
            Some("accepted") => AttemptState::Accepted,
            Some("rejected") => AttemptState::Rejected,
            _ => return,
        };
        let Some(targets) = self.requests.get(&ConversationKey::new(r, id, true)) else {
            return;
        };
        for loc in targets {
            let input = &mut self.inputs[loc.input];
            let attempt = &mut input.attempts[loc.attempt];
            attempt.state = state;
            attempt.response = Some(r.clone());
            attempt.reason = body(r)["reason"].as_str().map(str::to_owned);
            input.refresh();
            if input.state == DeliveryState::Withdrawn {
                input.turn = None;
            }
        }
    }

    // A qualified late receipt belongs to that attempt even when the logical input was resubmitted.
    fn targets(&self, r: &Record, input: Option<&str>, request: Option<&str>) -> Vec<AttemptAt> {
        if let Some(id) = request {
            return self
                .requests
                .get(&ConversationKey::new(r, id, true))
                .filter(|targets| {
                    input.is_none_or(|input| {
                        targets
                            .iter()
                            .any(|t| self.inputs[t.input].input_id.as_deref() == Some(input))
                    })
                })
                .cloned()
                .unwrap_or_default();
        }
        let Some(&i) = input.and_then(|id| self.keys.get(&ConversationKey::new(r, id, false)))
        else {
            return vec![];
        };
        let Some((j, attempt)) = self.inputs[i].attempts.iter().enumerate().next_back() else {
            return vec![];
        };
        if attempt.request.stream_id != r.stream_id {
            return vec![];
        }
        self.peers(i, j)
    }
    fn peers(&self, i: usize, j: usize) -> Vec<AttemptAt> {
        let attempt = &self.inputs[i].attempts[j];
        attempt
            .request
            .control_request_id()
            .and_then(|id| {
                self.requests
                    .get(&ConversationKey::new(&attempt.request, id, true))
            })
            .cloned()
            .unwrap_or_else(|| {
                vec![AttemptAt {
                    input: i,
                    attempt: j,
                }]
            })
    }
    fn native(
        &mut self,
        r: &Record,
        input_id: Option<&str>,
        request_id: Option<&str>,
        signal: evidence::Signal,
    ) {
        for loc in self.targets(r, input_id, request_id) {
            let input = &mut self.inputs[loc.input];
            let latest = loc.attempt + 1 == input.attempts.len();
            match signal {
                evidence::Signal::Turn(phase) if latest => {
                    if phase == TurnPhase::Queued
                        || input.turn.as_ref().is_some_and(|t| {
                            t.phase == TurnPhase::Queued && t.cursor.stream_id == r.stream_id
                        })
                    {
                        input.turn = Some(InputTurn {
                            phase,
                            cursor: r.cursor(),
                        });
                    }
                }
                evidence::Signal::Handled if request_id.is_some() => {
                    input.attempts[loc.attempt].handled = Some(r.cursor());
                    if latest {
                        input.handled = Some(r.cursor());
                        input.turn = None;
                    }
                }
                evidence::Signal::Drop(reason)
                    if input.drop.is_none_or(|(old, _)| old <= loc.attempt + 1) =>
                {
                    input.drop = Some((loc.attempt + 1, reason));
                    input.refresh();
                    if latest {
                        input.turn = None;
                    }
                }
                _ => {}
            }
        }
    }

    fn echo(&mut self, r: &Record, echo: InputEcho) {
        let Some(id) = echo.input_id.as_deref() else {
            self.unlinked.push(echo);
            return;
        };
        let i = self.find_or_insert(ConversationKey::new(r, id, false), Some(id));
        let input = &mut self.inputs[i];
        if echo.native_message_id.is_some()
            && input.observations.iter().any(|o| {
                o.native_message_id == echo.native_message_id && o.evidence == echo.evidence
            })
        {
            return;
        }
        if input.attempts.is_empty() {
            input.text = echo.text.clone();
        }
        input.observations.push(echo);
        let Some(j) = input.attempts.len().checked_sub(1) else {
            return;
        };
        for loc in self.peers(i, j) {
            let target = &mut self.inputs[loc.input];
            if target.drop == Some((target.attempts.len(), DropReason::RuntimeExited))
                && loc.attempt + 1 == target.attempts.len()
            {
                target.drop = None;
                target.refresh();
            }
        }
    }
    fn exit(&mut self, r: &Record) {
        // A child exit settles its own descendants, never a sibling or the parent's queue.
        let witnessed: HashSet<_> = self
            .inputs
            .iter()
            .filter(|item| item.key.session_id == r.session_id && !item.observations.is_empty())
            .map(|input| input.key.agent_path.clone())
            .collect();
        for i in 0..self.inputs.len() {
            let input = &self.inputs[i];
            let Some(last) = input.attempts.last() else {
                continue;
            };
            if last.request.stream_id != r.stream_id
                || input.key.session_id != r.session_id
                || !input.key.agent_path.starts_with(&r.agent_path)
            {
                continue;
            }
            let witnessed = witnessed.contains(&input.key.agent_path);
            if input.awaits_echo(witnessed) {
                let input = &mut self.inputs[i];
                input.drop = Some((input.attempts.len(), DropReason::RuntimeExited));
                input.turn = None;
                input.refresh();
            }
        }
    }

    /// Consume one record. Re-delivery at or before a consumed cursor has no effect, including
    /// when the caller interleaves multiple resumed streams whose sequence numbers restart.
    pub fn apply(&mut self, r: &Arc<Record>) {
        if self
            .cursors
            .get(&r.stream_id)
            .is_some_and(|&seq| r.seq <= seq)
        {
            return;
        }
        self.cursors.insert(r.stream_id, r.seq);
        if let Some(kind) = InputAttemptKind::of(r) {
            self.request(r, kind);
        }
        if r.kind == RecordKind::Response {
            self.decision(r);
        }
        evidence::signals(r, |input, request, signal| {
            self.native(r, input, request, signal)
        });
        for echo in r.input_echoes() {
            self.echo(r, echo);
        }
        if r.events.iter().any(|e| matches!(e, Event::Exited { .. })) {
            self.exit(r);
        }
    }
}

/// Recompute the public view from recorded records in delivery order; no native runtime is opened.
pub fn conversation_of(records: &[Arc<Record>]) -> Conversation {
    let mut state = Conversation::default();
    for r in records {
        state.apply(r);
    }
    state
}

#[cfg(not(target_arch = "wasm32"))]
impl crate::Session {
    /// Read the same projection used for offline replay. Continue from this stream's cursor with
    /// `Session::subscribe` and apply subsequent records to this returned conversation.
    pub fn conversation(&self) -> Conversation {
        conversation_of(&self.records())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct ConversationSubscription {
    state: Conversation,
    records: crate::Subscription,
    initial: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl ConversationSubscription {
    /// The first view includes the full prefix. With an explicit cursor, only later records notify.
    pub async fn next(&mut self) -> Option<&Conversation> {
        if !std::mem::take(&mut self.initial) {
            self.state.apply(&self.records.next().await?);
        }
        Some(&self.state)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl crate::Session {
    pub fn observe_conversation(
        &self,
        cursor: Option<&Cursor>,
    ) -> crate::Result<ConversationSubscription> {
        let records = self.records();
        let head = records.last().map(|r| r.cursor());
        let boundary = cursor.or(head.as_ref());
        let prefix = if let Some(cursor) = boundary {
            let pos = usize::try_from(cursor.after_seq).map_err(|_| crate::Error::InvalidCursor)?;
            if records.get(pos).is_none_or(|r| r.cursor() != *cursor) {
                return Err(crate::Error::InvalidCursor);
            }
            &records[..=pos]
        } else {
            &[]
        };
        Ok(ConversationSubscription {
            state: conversation_of(prefix),
            records: self.subscribe(boundary)?,
            initial: cursor.is_none() && head.is_some(),
        })
    }
}

#[cfg(test)]
mod tests;
