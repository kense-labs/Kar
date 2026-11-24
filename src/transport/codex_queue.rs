//! Codex persists this queue. Only a matching user-message identity can attach a turn to a caller.
use super::Reply;
use crate::{
    Direction, Error, Event, InputReceipt, Outcome, RecordKind, Result, TurnReport, stream::Journal,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

pub(super) const ADD: &str = "thread/queue/add";

#[derive(Default)]
pub(super) struct NativeQueue {
    interrupted: bool,
    entries: HashMap<Uuid, Entry>,
    turns: HashMap<String, Uuid>,
    /// A pending native decision is keyed separately from the supplied client input ID.
    requests: HashMap<Uuid, Uuid>,
}

struct Entry {
    request: Uuid,
    ack: Option<Reply<()>>,
    done: Option<Reply<TurnReport>>,
    turn: Option<String>,
    messages: Vec<String>,
    partial: String,
    outcome: Option<Outcome>,
    failure: Option<crate::TurnFailure>,
    structured_output: Option<Value>,
}

impl NativeQueue {
    pub(super) fn interrupted(&self) -> bool {
        self.interrupted
    }

    pub(super) fn has_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    pub(super) fn active(&self, turn: Option<&str>) -> Option<(Uuid, Uuid)> {
        let id = *self.turns.get(turn?)?;
        Some((id, self.entries.get(&id)?.request))
    }

    pub(super) fn check(&self, id: Uuid, compacting: bool, schema: bool) -> Result<()> {
        if compacting {
            return Err(Error::Busy);
        }
        if self.interrupted {
            return Err(Error::Refused("Codex does not automatically consume queued input after interruption; use prompt to start a new turn".into()));
        }
        if schema {
            return Err(Error::Unsupported("Codex queued input cannot carry outputSchema; use prompt after the current turn ends".into()));
        }
        if self.entries.contains_key(&id) {
            return Err(Error::Refused(
                "Codex queue already has an outstanding input with this ID".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn sent(
        &mut self,
        id: Uuid,
        request: Uuid,
        ack: Reply<()>,
        done: Reply<TurnReport>,
    ) {
        self.requests.insert(request, id);
        self.entries.insert(
            id,
            Entry {
                request,
                ack: Some(ack),
                done: Some(done),
                turn: None,
                messages: vec![],
                partial: String::new(),
                outcome: None,
                failure: None,
                structured_output: None,
            },
        );
    }

    pub(super) fn reply(
        &mut self,
        journal: &Journal,
        request: Uuid,
        result: Result<Value>,
        secrets: &[String],
    ) {
        let Some(id) = self.requests.remove(&request) else {
            return;
        };
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        let accepted = match result {
            Ok(native)
                if native
                    .pointer("/queuedSubmission/clientUserMessageId")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<Uuid>().ok())
                    != Some(id) =>
            {
                // Keep the original RPC frame. A contradictory acknowledgement proves neither refusal nor non-delivery.
                Err(Error::Protocol(
                    "Codex did not confirm the requested queue input identity".into(),
                ))
            }
            Ok(mut native) => {
                if let Some(submission) = native.get_mut("queuedSubmission") {
                    crate::input::omit_image_bytes(submission);
                }
                super::redact(&mut native, secrets);
                let response = json!({"type":"accepted","requestId":request,"native":native});
                let events = if entry.turn.is_none() {
                    vec![Event::InputQueued {
                        input_id: id.to_string(),
                    }]
                } else {
                    vec![]
                };
                journal.push(
                    Direction::Host,
                    RecordKind::Response,
                    None,
                    vec![],
                    response,
                    events,
                );
                Ok(())
            }
            Err(mut error) => {
                match &mut error {
                    Error::Refused(reason) => {
                        super::redact_text(reason, secrets);
                        journal.control_decision(request, Some(("runtime_refused", reason)));
                    }
                    Error::Unsupported(reason) => {
                        journal.control_decision(request, Some(("unsupported", reason)))
                    }
                    _ => {}
                }
                Err(error)
            }
        };
        let failed = accepted.is_err();
        if let Some(ack) = entry.ack.take() {
            let _ = ack.send(accepted);
        }
        if failed {
            if let Some(turn) = self.entries.remove(&id).and_then(|e| e.turn) {
                self.turns.remove(&turn);
            }
        } else {
            self.finish(id);
        }
    }

    /// Associate only actual root notifications, even if they precede the queue/add response.
    pub(super) fn notice(
        &mut self,
        packet: &Value,
        events: &mut Vec<Event>,
    ) -> Option<(Uuid, Uuid)> {
        let params = &packet["params"];
        let method = packet["method"].as_str()?;
        match method {
            "turn/started" => self.interrupted = false,
            "turn/completed" => self.interrupted = params["turn"]["status"] == "interrupted",
            _ => {}
        }
        let turn = params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str())?;
        let item = &params["item"];
        if matches!(method, "item/started" | "item/completed")
            && item["type"] == "userMessage"
            && let Some(id) = item["clientId"]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
            && let Some(entry) = self.entries.get_mut(&id)
            && entry.turn.is_none()
        {
            entry.turn = Some(turn.to_owned());
            self.turns.insert(turn.to_owned(), id);
            if let Some(text) = crate::observe::input_text(&item["content"]) {
                events.push(Event::UserMessage { text });
            }
            events.push(Event::InputRemoved {
                input_id: id.to_string(),
            });
            events.push(Event::NativeInput {
                input_id: id,
                receipt: InputReceipt::Started,
            });
            return Some((id, entry.request));
        }
        None
    }

    pub(super) fn collect(&mut self, turn: &str, events: &[Event]) -> Option<Uuid> {
        let (id, _) = self.active(Some(turn))?;
        let entry = self.entries.get_mut(&id)?;
        for event in events {
            match event {
                Event::TextDelta { text } => entry.partial.push_str(text),
                Event::Message { text } => {
                    entry.partial.clear();
                    entry.messages.push(text.clone());
                }
                Event::TurnEnded { outcome } => entry.outcome = Some(outcome.clone()),
                Event::TurnFailure { failure } => entry.failure = Some(failure.clone()),
                Event::StructuredOutput { value } => entry.structured_output = Some(value.clone()),
                _ => {}
            }
        }
        Some(id)
    }

    /// Fulfil only after the terminal record and the queue admission are both observable.
    pub(super) fn finish(&mut self, id: Uuid) {
        let Some(entry) = self.entries.get(&id) else {
            return;
        };
        if entry.ack.is_some() || entry.outcome.is_none() {
            return;
        }
        let mut entry = self.entries.remove(&id).expect("matched queued submission");
        if let Some(turn) = &entry.turn {
            self.turns.remove(turn);
        }
        if !entry.partial.is_empty() {
            entry.messages.push(entry.partial);
        }
        if let Some(done) = entry.done.take() {
            let _ = done.send(Ok(TurnReport {
                outcome: entry.outcome.expect("native terminal event"),
                text: entry.messages.join("\n"),
                failure: entry.failure,
                structured_output: entry.structured_output,
                native_input_receipt: Some(InputReceipt::Started),
            }));
        }
    }

    pub(super) fn disconnected(&mut self, reason: &str) {
        self.requests.clear();
        self.turns.clear();
        for (_, mut entry) in self.entries.drain() {
            if let Some(ack) = entry.ack.take() {
                let _ = ack.send(Err(Error::Closed(reason.to_owned())));
            }
            if let Some(done) = entry.done.take() {
                let _ = done.send(Err(Error::Closed(reason.to_owned())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Input;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn late_admission_is_paired_once_and_only_the_matching_native_turn_completes() {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("root");
        let mut queue = NativeQueue::default();
        let input = Input::text("ordered".into());
        let id = Uuid::new_v4();
        let request = input.request_id;
        journal.record_queue_request(id, &input, &[], true);
        let (ack, mut acceptance) = oneshot::channel();
        let (done, mut terminal) = oneshot::channel();
        queue.sent(id, request, ack, done);
        let started = |turn, id| json!({"method":"item/started","params":{"turnId":turn,"item":{"type":"userMessage","clientId":id}}});
        assert!(
            queue
                .notice(&started("other", Uuid::new_v4()), &mut vec![])
                .is_none()
        );
        assert!(
            queue
                .collect(
                    "other",
                    &[Event::TurnEnded {
                        outcome: Outcome::Aborted
                    }]
                )
                .is_none()
        );
        let mut events = vec![];
        assert_eq!(
            queue.notice(&started("native-turn", id), &mut events),
            Some((id, request))
        );
        assert!(events.contains(&Event::NativeInput {
            input_id: id,
            receipt: InputReceipt::Started
        }));
        assert_eq!(
            queue.collect(
                "native-turn",
                &[
                    Event::Message {
                        text: "own result".into()
                    },
                    Event::TurnEnded {
                        outcome: Outcome::Completed
                    }
                ]
            ),
            Some(id)
        );
        queue.finish(id);
        assert!(matches!(
            terminal.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            acceptance.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        queue.reply(
            &journal,
            request,
            Ok(json!({"queuedSubmission":{"id":"q","clientUserMessageId":id}})),
            &[],
        );
        acceptance.await.unwrap().unwrap();
        let report = terminal.await.unwrap().unwrap();
        assert_eq!(report.text, "own result");
        assert_eq!(report.outcome, Outcome::Completed);
        assert!(!queue.has_entries());
        let receipt = journal.control_receipt(&request.to_string()).unwrap();
        assert!(receipt.response.events.is_empty());
        assert_eq!(
            receipt.response.native["native"]["queuedSubmission"]["id"],
            "q"
        );
        assert_eq!(journal.records().len(), 2);
    }

    #[tokio::test]
    async fn mismatched_queue_identity_and_disconnect_leave_admission_unconfirmed() {
        for disconnect in [true, false] {
            let mut queue = NativeQueue::default();
            let journal = Journal::new(None).unwrap();
            journal.bind_root("root");
            let input = Input::text("once".into());
            let id = Uuid::new_v4();
            let request = input.request_id;
            journal.record_queue_request(id, &input, &[], true);
            let (ack, received) = oneshot::channel();
            let (done, _) = oneshot::channel();
            queue.sent(id, request, ack, done);
            if disconnect {
                queue.disconnected("transport closed");
            } else {
                queue.reply(
                    &journal,
                    request,
                    Ok(json!({"queuedSubmission":{"id":"q","clientUserMessageId":Uuid::new_v4()}})),
                    &[],
                );
            }
            assert!(received.await.unwrap().is_err());
            assert!(journal.control_receipt(&request.to_string()).is_none());
        }
    }
}
