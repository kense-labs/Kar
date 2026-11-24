//! Read a root turn's boundary from the same ordered records used for replay and live delivery.
mod input;
use crate::{
    Cursor, Direction, Error, Event, FailureClass, Outcome, Record, RecordKind, Result, TurnFailure,
};
pub use input::{ObservedInputEnd, input_end_after};
use std::{collections::HashSet, sync::Arc};

/// The original terminal evidence. Release ends observation even when the native process continues.
#[derive(Clone, Debug)]
pub struct ObservedTurnEnd {
    pub outcome: Outcome,
    pub failure: Option<TurnFailure>,
    pub record: Arc<Record>,
}

impl ObservedTurnEnd {
    fn observed(record: &Arc<Record>, outcome: Outcome, failure: Option<TurnFailure>) -> Self {
        Self {
            record: record.clone(),
            outcome,
            failure,
        }
    }
}

pub(super) fn body(record: &Record) -> &serde_json::Value {
    record
        .upstream
        .as_ref()
        .and_then(|r| r.get("body"))
        .unwrap_or(&record.native)
}

pub(super) fn control(record: &Record) -> Option<&str> {
    let value = body(record);
    value["kind"]
        .as_str()
        .or_else(|| value["control"].as_str())
        .or_else(|| value["type"].as_str())
}

#[derive(Default)]
struct Stop {
    active: bool,
    prompt: Option<String>,
    aborts: HashSet<String>,
    stopped: bool,
}

impl Stop {
    fn control(&mut self, record: &Record) {
        if record.kind == RecordKind::Request && record.direction == Direction::ToRuntime {
            match control(record) {
                Some("abort") if self.active => {
                    if let Some(id) = record.control_request_id() {
                        self.aborts.insert(id.to_owned());
                    }
                }
                Some("dispose") if self.active => self.stopped = true,
                _ if record.prompt.is_some() || control(record) == Some("prompt") => {
                    if !self.active {
                        self.prompt = record.control_request_id().map(str::to_owned);
                    }
                    self.active = true;
                }
                _ => {}
            }
        }
        if let Some(id) = record.control_response_to() {
            if self.aborts.remove(id) && control(record) == Some("accepted") {
                self.stopped = true;
            }
            if self.prompt.as_deref() == Some(id) && control(record) == Some("rejected") {
                *self = Self::default();
            }
        }
    }

    fn end(&self, record: &Arc<Record>, class: FailureClass, reason: &str) -> ObservedTurnEnd {
        if self.stopped {
            ObservedTurnEnd::observed(record, Outcome::Aborted, None)
        } else {
            ObservedTurnEnd::observed(
                record,
                Outcome::Failed,
                Some(TurnFailure::new(class, reason)),
            )
        }
    }
}

struct Reader<'a> {
    root: &'a str,
    after: Option<u64>,
    stop: Stop,
    released: bool,
}

impl<'a> Reader<'a> {
    fn new(records: &[Arc<Record>], root: &'a str, cursor: Option<&Cursor>) -> Result<Self> {
        if let Some(cursor) = cursor {
            let index = usize::try_from(cursor.after_seq).map_err(|_| Error::InvalidCursor)?;
            if records.get(index).is_none_or(|r| r.cursor() != *cursor) {
                return Err(Error::InvalidCursor);
            }
        }
        Ok(Self {
            root,
            after: cursor.map(|c| c.after_seq),
            stop: Stop::default(),
            released: false,
        })
    }
    fn read(&mut self, record: &Arc<Record>) -> Option<ObservedTurnEnd> {
        if !record.agent_path.is_empty() || record.session_id.as_deref() != Some(self.root) {
            return None;
        }
        if !self.released {
            self.stop.control(record);
        }
        for event in &record.events {
            let boundary = match event {
                Event::Exited { reason } => {
                    self.released = true;
                    self.stop.end(record, FailureClass::RuntimeExited, reason)
                }
                _ if self.released => continue,
                Event::Released => {
                    self.released = true;
                    self.stop.end(
                        record,
                        FailureClass::Unknown,
                        "session released; native work may continue",
                    )
                }
                Event::TurnEnded { outcome } => ObservedTurnEnd::observed(
                    record,
                    outcome.clone(),
                    record.turn_failure().cloned(),
                ),
                Event::TurnStarted
                | Event::PromptDispatched
                | Event::NativeTurnStarted { .. }
                | Event::NotificationActivity { .. } => {
                    self.stop.active = true;
                    continue;
                }
                _ => continue,
            };
            self.stop = Stop::default();
            if self.after.is_none_or(|seq| record.seq > seq) {
                return Some(boundary);
            }
        }
        None
    }
}

/// Return the first root turn boundary strictly after the cursor in a complete recording.
pub fn turn_end_after(
    records: &[Arc<Record>],
    root: &str,
    cursor: Option<&Cursor>,
) -> Result<Option<ObservedTurnEnd>> {
    let mut reader = Reader::new(records, root, cursor)?;
    Ok(records.iter().find_map(|record| reader.read(record)))
}

#[cfg(not(target_arch = "wasm32"))]
impl crate::Session {
    /// Observe a root turn without submitting input. The cursor belongs to this session stream.
    pub async fn await_turn_end(&self, cursor: Option<&Cursor>) -> Result<ObservedTurnEnd> {
        let records = self.records();
        let mut reader = Reader::new(&records, self.id(), cursor)?;
        observe_live(self, records, |record| reader.read(record)).await
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn observe_live<T>(
    session: &crate::Session,
    records: Vec<Arc<Record>>,
    mut read: impl FnMut(&Arc<Record>) -> Option<T>,
) -> Result<T> {
    if let Some(end) = records.iter().find_map(&mut read) {
        return Ok(end);
    }
    let mut live = session.subscribe(records.last().map(|r| r.cursor()).as_ref())?;
    drop(records);
    while let Some(record) = live.next().await {
        if let Some(end) = read(&record) {
            return Ok(end);
        }
    }
    Err(Error::Closed(
        "stream closed without a later observation boundary".into(),
    ))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::{Protocol, Session, SessionOptions, stream::Journal};
    use serde_json::{Value, json};

    pub(super) fn session() -> (Session, Journal) {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("root");
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let session = Session::remote(
            Protocol::Codex,
            SessionOptions::new("."),
            tx,
            journal.clone(),
            "root".into(),
            Value::Null,
        );
        (session, journal)
    }

    fn frame(journal: &Journal, session: &str, path: Vec<String>, event: Event) -> Arc<Record> {
        journal
            .append(
                Direction::FromRuntime,
                RecordKind::Frame,
                Some(session.into()),
                path,
                None,
                Value::Null,
                vec![event],
            )
            .unwrap()
    }

    #[tokio::test]
    async fn retained_and_live_observers_require_a_root_boundary_and_a_matching_stop() {
        for accepted in [true, false] {
            let (session, journal) = session();
            let cursor = frame(&journal, "root", vec![], Event::TurnStarted).cursor();
            let watch = session.await_turn_end(Some(&cursor));
            tokio::pin!(watch);
            tokio::select! {
                biased;
                result = &mut watch => panic!("early result: {result:?}"),
                () = std::future::ready(()) => {}
            }
            let end = Event::TurnEnded {
                outcome: Outcome::Completed,
            };
            frame(&journal, "child", vec![], end.clone());
            frame(&journal, "root", vec!["task".into()], end);
            let abort = journal.request_abort();
            journal.control_decision(uuid::Uuid::new_v4(), None);
            journal.control_decision(abort, (!accepted).then_some(("no_active_turn", "refused")));
            assert!(
                turn_end_after(&session.records(), "root", Some(&cursor))
                    .unwrap()
                    .is_none()
            );
            journal.host_event(Event::Released);
            let live = watch.await.unwrap();
            let replay = turn_end_after(&session.records(), "root", Some(&cursor))
                .unwrap()
                .unwrap();
            assert_eq!(live.record.cursor(), replay.record.cursor());
            assert_eq!(
                live.outcome,
                if accepted {
                    Outcome::Aborted
                } else {
                    Outcome::Failed
                }
            );
            assert_eq!(
                live.failure.map(|f| f.class),
                (!accepted).then_some(FailureClass::Unknown)
            );
            frame(
                &journal,
                "root",
                vec![],
                Event::TurnEnded {
                    outcome: Outcome::Completed,
                },
            );
            assert!(
                turn_end_after(&session.records(), "root", Some(&replay.record.cursor()))
                    .unwrap()
                    .is_none()
            );
            journal.host_event(Event::Exited {
                reason: "worker exited".into(),
            });
            let exit = session
                .await_turn_end(Some(&replay.record.cursor()))
                .await
                .unwrap();
            assert_eq!(exit.failure.unwrap().class, FailureClass::RuntimeExited);
        }
    }

    #[test]
    fn checks_cursor_stream_and_keeps_the_native_result_after_a_dispose() {
        let (_, journal) = session();
        journal.control_request(json!({"type":"prompt","text":"work"}));
        journal.request_dispose();
        frame(
            &journal,
            "root",
            vec![],
            Event::TurnEnded {
                outcome: Outcome::Completed,
            },
        );
        let records = journal.records();
        let end = turn_end_after(&records, "root", None).unwrap().unwrap();
        assert_eq!(end.outcome, Outcome::Completed);
        let foreign = Cursor {
            stream_id: uuid::Uuid::new_v4(),
            after_seq: 0,
        };
        assert!(matches!(
            turn_end_after(&records, "root", Some(&foreign)),
            Err(Error::InvalidCursor)
        ));
    }
}
