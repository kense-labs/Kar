//! Follow one input across native admission, adapter queue dispatch and host withdrawal.
use super::{ObservedTurnEnd, Reader, body, control};
use crate::{
    Cursor, Direction, Event, FailureClass, InputReceipt, Outcome, Record, RecordKind, Result,
    TurnFailure,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// A handled input ended at its receipt without producing a model turn.
#[derive(Clone, Debug)]
pub enum ObservedInputEnd {
    Turn(ObservedTurnEnd),
    Handled {
        request_id: String,
        input_id: Option<String>,
        record: Arc<Record>,
    },
}

fn failed(record: &Arc<Record>, reason: &str) -> ObservedInputEnd {
    let failure = record
        .turn_failure()
        .cloned()
        .unwrap_or_else(|| TurnFailure::new(FailureClass::Unknown, reason));
    ObservedInputEnd::Turn(ObservedTurnEnd::observed(
        record,
        Outcome::Failed,
        Some(failure),
    ))
}
fn aborted(record: &Arc<Record>) -> ObservedInputEnd {
    ObservedInputEnd::Turn(ObservedTurnEnd::observed(record, Outcome::Aborted, None))
}

#[derive(Default)]
struct Queue {
    requests: HashMap<String, String>,
    withdrawals: HashMap<String, String>,
    // A valid dispatch names only inputs explicitly held by the adapter or already withdrawn.
    claimable: HashSet<String>,
}
impl Queue {
    fn observe(&mut self, record: &Record) {
        if record.kind == RecordKind::Request
            && record.direction == Direction::ToRuntime
            && let Some(id) = record.control_request_id()
        {
            match control(record) {
                Some("queue") => {
                    let key = body(record)["inputId"].as_str().unwrap_or(id);
                    self.requests.insert(id.to_owned(), key.to_owned());
                }
                Some("withdraw") => {
                    if let Some(key) = body(record)["inputId"].as_str() {
                        self.withdrawals.insert(id.to_owned(), key.to_owned());
                    }
                }
                _ => {}
            }
        }
        if let Some(id) = record.control_response_to() {
            let accepted = control(record) == Some("accepted");
            if let Some(key) = self.requests.remove(id)
                && accepted
                && body(record)["queued"] == "adapter"
            {
                self.claimable.insert(key);
            }
            if let Some(key) = self.withdrawals.remove(id)
                && accepted
            {
                self.claimable.insert(key);
            }
        }
    }
}

#[derive(Default)]
struct Attempt {
    requests: HashSet<String>,
    withdrawal: Option<String>,
    dispatch: Option<String>,
    admission: Option<String>,
    adapter_queued: bool,
    native_queued: bool,
    superseded: bool,
}

struct InputReader<'a> {
    turns: Reader<'a>,
    input: &'a str,
    queue: Queue,
    attempt: Attempt,
}

impl<'a> InputReader<'a> {
    fn new(
        records: &[Arc<Record>],
        root: &'a str,
        cursor: Option<&Cursor>,
        input: &'a str,
    ) -> Result<Self> {
        Ok(Self {
            turns: Reader::new(records, root, cursor)?,
            input,
            queue: Queue::default(),
            attempt: Attempt::default(),
        })
    }

    fn request(&mut self, record: &Record, before: bool) {
        let Some(id) = record.control_request_id() else {
            return;
        };
        let op = control(record);
        let submitted = record
            .prompt
            .as_ref()
            .map(|p| p.input_id.as_str())
            .or_else(|| body(record)["inputId"].as_str());
        if op == Some("withdraw") {
            if submitted == Some(self.input) && !self.attempt.superseded {
                self.attempt.withdrawal = Some(id.to_owned());
            }
            return;
        }
        if record.prompt.is_none() && !matches!(op, Some("prompt" | "steer" | "queue")) {
            return;
        }
        let valid_carry = !record.carries.is_empty()
            && record.carries.iter().all(|key| {
                self.queue.claimable.contains(key)
                // A native Kar dispatch reuses its queued UUID. Voyage claims have separate carrier identity.
                && (record.upstream.is_none() || submitted != Some(key.as_str()))
            });
        if valid_carry {
            if !self.attempt.superseded
                && record
                    .carries
                    .iter()
                    .any(|key| key == self.input || self.attempt.requests.contains(key))
            {
                self.attempt.requests.insert(id.to_owned());
                self.attempt.dispatch = Some(id.to_owned());
            }
            for key in &record.carries {
                self.queue.claimable.remove(key);
            }
        }
        if submitted == Some(self.input) && !self.attempt.requests.contains(id) {
            if before || self.attempt.requests.is_empty() {
                self.attempt = Attempt::default();
                self.attempt.requests.insert(id.to_owned());
            } else {
                self.attempt.superseded = true;
            }
        }
        if self.attempt.requests.contains(id) {
            if record.prompt.as_ref().is_some_and(|p| p.native_admission)
                || body(record)["admission"] == "native"
                || record.native["method"] == "turn/start"
            {
                self.attempt.admission = Some(id.to_owned());
            }
            if self.attempt.dispatch.as_deref() == Some(id)
                && record.events.contains(&Event::PromptDispatched)
            {
                self.attempt.adapter_queued = false;
            }
        }
    }

    fn response(&mut self, record: &Arc<Record>, before: bool) -> Option<ObservedInputEnd> {
        let id = record.control_response_to()?;
        let accepted = control(record) == Some("accepted");
        if self.attempt.withdrawal.as_deref() == Some(id) && accepted {
            return Some(aborted(record));
        }
        if self.attempt.requests.contains(id) {
            let admission = self.attempt.admission.as_deref() == Some(id);
            if admission {
                self.attempt.admission = None;
            }
            if accepted {
                if body(record)["queued"] == "adapter" {
                    self.attempt.adapter_queued = true;
                } else if self.attempt.dispatch.as_deref() == Some(id) {
                    self.attempt.adapter_queued = false;
                }
            } else if control(record) == Some("rejected") {
                if admission || self.attempt.dispatch.as_deref() == Some(id) {
                    return Some(failed(
                        record,
                        body(record)["reason"]
                            .as_str()
                            .unwrap_or("runtime refused the input"),
                    ));
                }
                if before {
                    self.attempt = Attempt::default();
                }
            }
        }
        None
    }

    fn matches(&self, input: Option<&str>, request: Option<&str>) -> bool {
        match request {
            Some(id) => self.attempt.requests.contains(id),
            None => !self.attempt.superseded && input == Some(self.input),
        }
    }

    fn voyage_frame(&mut self, record: &Arc<Record>) -> Option<ObservedInputEnd> {
        for event in record
            .upstream
            .as_ref()?
            .pointer("/body/events")?
            .as_array()?
        {
            let input = event["inputId"].as_str();
            let request = event["requestId"].as_str();
            match event["kind"].as_str() {
                Some("input_handled") => {
                    let Some(id) = request.filter(|id| self.attempt.requests.contains(*id)) else {
                        continue;
                    };
                    return Some(ObservedInputEnd::Handled {
                        request_id: id.into(),
                        input_id: input.map(str::to_owned),
                        record: record.clone(),
                    });
                }
                Some("input_dropped") if self.matches(input, request) => {
                    return Some(if event["reason"] == "turn_interrupted" {
                        aborted(record)
                    } else {
                        let class = serde_json::from_value(event["failure"].clone())
                            .unwrap_or(FailureClass::Unknown);
                        let reason = event["message"]
                            .as_str()
                            .unwrap_or("runtime refused the input");
                        ObservedInputEnd::Turn(ObservedTurnEnd::observed(
                            record,
                            Outcome::Failed,
                            Some(TurnFailure::new(class, reason)),
                        ))
                    });
                }
                Some("input_queued") if self.matches(input, None) => {
                    self.attempt.native_queued = true
                }
                Some("turn_active") => {
                    if self.matches(input, None) {
                        self.attempt.native_queued = false;
                    }
                    if self.attempt.admission.is_some()
                        && self.attempt.admission == record.response_to
                    {
                        self.attempt.admission = None;
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn frame(&mut self, record: &Arc<Record>) -> Option<ObservedInputEnd> {
        if record.upstream.is_some() {
            return self.voyage_frame(record);
        }
        let request = record.response_to.as_deref();
        for event in &record.events {
            if let Event::NativeInput { input_id, receipt } = event
                && self.matches(Some(&input_id.to_string()), request)
            {
                match receipt {
                    InputReceipt::Queued => self.attempt.native_queued = true,
                    InputReceipt::Started => {
                        self.attempt.native_queued = false;
                        self.attempt.admission = None;
                    }
                    InputReceipt::Cancelled => return Some(aborted(record)),
                    InputReceipt::Refused | InputReceipt::Duplicate => {
                        return Some(failed(record, "runtime refused the input"));
                    }
                    InputReceipt::Handled => {
                        if let Some(id) = request {
                            return Some(ObservedInputEnd::Handled {
                                request_id: id.into(),
                                input_id: Some(input_id.to_string()),
                                record: record.clone(),
                            });
                        }
                    }
                }
            }
        }
        for event in &record.events {
            match event {
                Event::InputQueued { input_id }
                    if body(record)["queued"] != "adapter"
                        && self.matches(Some(input_id), request) =>
                {
                    self.attempt.native_queued = true
                }
                Event::InputDropped { input_id } if self.matches(Some(input_id), request) => {
                    if record.input_drop_reason(input_id) == Some("turn_interrupted") {
                        return Some(aborted(record));
                    }
                    return Some(failed(record, "runtime dropped the input"));
                }
                Event::TurnStarted if request.is_some() && self.matches(None, request) => {
                    self.attempt.native_queued = false;
                    self.attempt.admission = None;
                }
                _ => {}
            }
        }
        None
    }

    fn read(&mut self, record: &Arc<Record>) -> Option<ObservedInputEnd> {
        if record.session_id.as_deref() != Some(self.turns.root) || !record.agent_path.is_empty() {
            return None;
        }
        let before = self.turns.after.is_some_and(|seq| record.seq <= seq);
        let was_released = self.turns.released;
        let unscoped = self.turns.read(record);
        if was_released {
            return unscoped.map(ObservedInputEnd::Turn);
        }
        self.queue.observe(record);
        let specific = match record.kind {
            RecordKind::Request if record.direction == Direction::ToRuntime => {
                self.request(record, before);
                None
            }
            RecordKind::Response => self.response(record, before).or_else(|| self.frame(record)),
            RecordKind::Frame => self.frame(record),
            _ => None,
        };
        if let Some(end) = specific {
            self.attempt = Attempt::default();
            return (!before).then_some(end);
        }
        let independent = record.events.iter().any(|e| {
            matches!(
                e,
                Event::NotificationActivity { .. } | Event::NativeTurnStarted { .. }
            )
        });
        let waiting = self.attempt.adapter_queued
            || self.attempt.native_queued
            || self.attempt.admission.is_some()
            || self.attempt.superseded
            || (independent && !self.attempt.requests.is_empty());
        if !waiting
            && before
            && record
                .events
                .iter()
                .any(|e| matches!(e, Event::TurnEnded { .. }))
        {
            self.attempt = Attempt::default();
        }
        unscoped
            .filter(|_| self.turns.released || !waiting)
            .map(ObservedInputEnd::Turn)
    }
}

/// Read the next boundary for an input identity, preserving native handled receipts and their request IDs.
pub fn input_end_after(
    records: &[Arc<Record>],
    root: &str,
    cursor: Option<&Cursor>,
    input: &str,
) -> Result<Option<ObservedInputEnd>> {
    let mut reader = InputReader::new(records, root, cursor, input)?;
    Ok(records.iter().find_map(|record| reader.read(record)))
}

#[cfg(not(target_arch = "wasm32"))]
impl crate::Session {
    /// Wait for a native input result without submitting or interrupting work.
    pub async fn await_input_end(
        &self,
        cursor: Option<&Cursor>,
        input: &str,
    ) -> Result<ObservedInputEnd> {
        let records = self.records();
        let mut reader = InputReader::new(&records, self.id(), cursor, input)?;
        super::observe_live(self, records, |record| reader.read(record)).await
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::stream::{Journal, RecordStamp};
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn from_voyage(entries: Vec<Value>) -> Vec<Arc<Record>> {
        let mut text =
            json!({"kind":"header","format":"kar-voyage/3","sessionId":"root"}).to_string();
        for (seq, mut entry) in entries.into_iter().enumerate() {
            entry["seq"] = seq.into();
            entry["receivedAt"] = seq.into();
            entry["agentPath"] = json!([]);
            entry["sessionId"] = "root".into();
            text.push('\n');
            text.push_str(&json!({"kind":"record","record":entry}).to_string());
        }
        let mut records = vec![];
        crate::voyage::import(std::io::Cursor::new(text), |r| {
            records.push(Arc::new(r));
            Ok(())
        })
        .unwrap();
        records
    }

    fn request(id: &str, op: &str, fields: Value) -> Value {
        let mut body = fields;
        body["kind"] = op.into();
        json!({"kind":"request","direction":"toRuntime","id":id,"body":body})
    }
    fn respond(id: &str, body: Value) -> Value {
        json!({"kind":"response","requestId":id,"body": body})
    }
    fn frame(events: Value) -> Value {
        json!({"kind":"frame","body":{"type":"event","native":{},"events":events}})
    }

    #[test]
    fn invalid_claim_cannot_drop_a_held_input_and_a_valid_dispatch_can() {
        let entries = vec![
            request("q", "queue", json!({"input":"work","inputId":"same"})),
            respond("q", json!({"kind":"accepted","queued":"adapter"})),
            frame(json!([{"kind":"turn_ended","outcome":{"kind":"completed"}}])),
            request(
                "bad",
                "prompt",
                json!({"input":"bad claim","carries":["same","absent"]}),
            ),
            respond("bad", json!({"kind":"rejected","reason":"invalid claim"})),
            request(
                "d",
                "prompt",
                json!({"input":"real claim","carries":["same"]}),
            ),
            respond(
                "d",
                json!({"kind":"rejected","reason":"runtime refused the queued input"}),
            ),
        ];
        let records = from_voyage(entries);
        let cursor = Some(records[0].cursor());
        assert!(
            input_end_after(&records[..6], "root", cursor.as_ref(), "same")
                .unwrap()
                .is_none()
        );
        let Some(ObservedInputEnd::Turn(end)) =
            input_end_after(&records, "root", cursor.as_ref(), "same").unwrap()
        else {
            panic!("missing dispatch refusal")
        };
        assert_eq!(end.record.seq, 6);
        assert_eq!(
            end.failure.unwrap().reason,
            "runtime refused the queued input"
        );
    }

    #[test]
    fn a_scoped_wait_uses_the_old_attempt_for_a_late_drop_and_the_new_one_after_a_settled_cursor() {
        let records = from_voyage(vec![
            request("old", "queue", json!({"input":"original","inputId":"same"})),
            respond("old", json!({"kind":"accepted","queued":"adapter"})),
            request("retry", "queue", json!({"input":"again","inputId":"same"})),
            respond("retry", json!({"kind":"accepted","queued":"adapter"})),
            request("w", "withdraw", json!({"inputId":"same"})),
            respond("w", json!({"kind":"accepted"})),
            frame(
                json!([{"kind":"input_dropped","requestId":"old","inputId":"same","reason":"runtime_refused","message":"old refused"}]),
            ),
            request(
                "native",
                "prompt",
                json!({"input":"extension","inputId":"same","admission":"native"}),
            ),
            frame(json!([{"kind":"turn_ended","outcome":{"kind":"completed"}}])),
            frame(json!([{"kind":"input_handled","requestId":"native","inputId":"same"}])),
        ]);
        let cursor = records[0].cursor();
        assert!(
            input_end_after(&records[..6], "root", Some(&cursor), "same")
                .unwrap()
                .is_none()
        );
        let Some(ObservedInputEnd::Turn(end)) =
            input_end_after(&records, "root", Some(&cursor), "same").unwrap()
        else {
            panic!()
        };
        assert_eq!(end.failure.unwrap().reason, "old refused");
        let Some(ObservedInputEnd::Handled {
            request_id, record, ..
        }) = input_end_after(&records, "root", Some(&records[6].cursor()), "same").unwrap()
        else {
            panic!()
        };
        assert_eq!(request_id, "native");
        assert_eq!(record.seq, 9);
    }

    #[test]
    fn a_carried_native_input_stays_queued_after_host_acceptance_until_its_own_start() {
        let records = from_voyage(vec![
            request("q", "queue", json!({"input":"wait","inputId":"id"})),
            respond("q", json!({"kind":"accepted","queued":"adapter"})),
            request(
                "d",
                "prompt",
                json!({"input":"carrier","carries":["id"],"admission":"native"}),
            ),
            frame(json!([{"kind":"input_queued","inputId":"id"}])),
            respond("d", json!({"kind":"accepted"})),
            frame(
                json!([{"kind":"turn_active","inputId":"other"},{"kind":"turn_ended","outcome":{"kind":"aborted"}}]),
            ),
            frame(json!([{"kind":"turn_active","inputId":"id"}])),
            frame(json!([{"kind":"turn_ended","outcome":{"kind":"completed"}}])),
        ]);
        let cursor = records[0].cursor();
        assert!(
            input_end_after(&records[..7], "root", Some(&cursor), "id")
                .unwrap()
                .is_none()
        );
        let Some(ObservedInputEnd::Turn(end)) =
            input_end_after(&records, "root", Some(&cursor), "id").unwrap()
        else {
            panic!("missing owned end")
        };
        assert_eq!(end.outcome, Outcome::Completed);
        assert_eq!(end.record.seq, 7);
    }

    #[tokio::test]
    async fn a_live_observer_uses_the_native_admission_frame_and_request_uuid() {
        let (session, journal) = super::super::tests::session();
        let request = Uuid::new_v4();
        let id = Uuid::new_v4();
        journal.push(
            RecordStamp::from(Direction::ToRuntime).with_prompt_request(id, true, request, None),
            RecordKind::Request,
            None,
            vec![],
            Value::Null,
            vec![Event::PromptDispatched],
        );
        let cursor = journal.latest_record().unwrap().cursor();
        let input = id.to_string();
        let wait = session.await_input_end(Some(&cursor), &input);
        tokio::pin!(wait);
        tokio::select! {
            biased;
            result = &mut wait => panic!("early result: {result:?}"),
            () = std::future::ready(()) => {}
        }
        journal.host_event(Event::TurnEnded {
            outcome: Outcome::Completed,
        });
        append_receipt(&journal, id, Uuid::new_v4());
        assert!(
            input_end_after(&session.records(), "root", Some(&cursor), &input)
                .unwrap()
                .is_none()
        );
        append_receipt(&journal, id, request);
        let ObservedInputEnd::Handled {
            record,
            request_id,
            input_id,
        } = wait.await.unwrap()
        else {
            panic!()
        };
        assert_eq!(record.seq, 3);
        assert_eq!(request_id, request.to_string());
        assert_eq!(input_id.as_deref(), Some(input.as_str()));
    }

    fn append_receipt(journal: &Journal, input_id: Uuid, request: Uuid) {
        let mut stamp = RecordStamp::from(Direction::FromRuntime);
        stamp.response_to = Some(request.to_string());
        journal.push(
            stamp,
            RecordKind::Frame,
            None,
            vec![],
            Value::Null,
            vec![Event::NativeInput {
                input_id,
                receipt: InputReceipt::Handled,
            }],
        );
    }
}
