//! Keep pre-interrupt observations, RPC replies and native ends tied to their turn.
use super::{AbortReply, Journal};
use crate::resources::turn::{Baseline, Evidence, failed};
use crate::{Error, TurnProcessCleanup};
use std::collections::{BTreeMap, HashMap};
use tokio::task::JoinSet;

type Observation = Result<Evidence, String>;
#[derive(Default)]
struct Attempt {
    reader: Option<tokio::task::Id>,
    waiting: Vec<AbortReply>,
    observation: Option<Observation>,
    requests: BTreeMap<u64, bool>,
    ended: Option<String>,
}
pub(super) struct Guard {
    root: Option<u32>,
    journal: Journal,
    attempts: HashMap<String, Attempt>,
    reading: JoinSet<Observation>,
    ending: JoinSet<()>,
}
impl Guard {
    pub(super) fn new(root: Option<u32>, journal: Journal) -> Self {
        Self {
            root,
            journal,
            attempts: HashMap::new(),
            reading: JoinSet::new(),
            ending: JoinSet::new(),
        }
    }

    pub(super) fn request(
        &mut self,
        turn: &str,
        baseline: Option<Baseline>,
        tx: AbortReply,
    ) -> Option<AbortReply> {
        use std::collections::hash_map::Entry;
        let attempt = match self.attempts.entry(turn.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let task = self.reading.spawn(Evidence::observe(self.root, baseline));
                entry.insert(Attempt {
                    reader: Some(task.id()),
                    ..Attempt::default()
                })
            }
        };
        if attempt.ended.is_some() {
            tx.finish(
                &self.journal,
                Err(Error::Refused(
                    "turn ended before the interrupt was sent".into(),
                )),
            );
        } else if attempt.observation.is_some() {
            return Some(tx);
        } else {
            attempt.waiting.push(tx);
        }
        None
    }

    pub(super) fn reading(&self) -> bool {
        !self.reading.is_empty()
    }
    pub(super) async fn prepared(&mut self) -> Option<(String, Vec<AbortReply>)> {
        let (reader, observation) = match self.reading.join_next_with_id().await? {
            Ok(result) => result,
            Err(error) => (
                error.id(),
                Err(format!("pre-interrupt observation failed: {error}")),
            ),
        };
        // An old observation may settle after the native turn ended.
        let (turn, attempt) = self
            .attempts
            .iter_mut()
            .find(|(_, a)| a.reader == Some(reader))?;
        attempt.reader = None;
        attempt.observation = Some(observation);
        Some((turn.clone(), std::mem::take(&mut attempt.waiting)))
    }
    pub(super) fn sent(&mut self, turn: &str, request: u64) {
        if let Some(attempt) = self.attempts.get_mut(turn) {
            attempt.requests.insert(request, false);
        }
    }
    pub(super) fn reply(&mut self, id: u64, accepted: bool, session: Option<String>) {
        let Some((turn, attempt)) = self
            .attempts
            .iter_mut()
            .find(|(_, a)| a.requests.contains_key(&id))
        else {
            return;
        };
        if accepted {
            attempt.requests.insert(id, true);
        } else {
            attempt.requests.remove(&id);
        }
        let turn = turn.clone();
        self.settle(&turn, session);
    }
    pub(super) fn ended(&mut self, turn: Option<&str>, status: &str, session: Option<String>) {
        if let Some(turn) = turn {
            self.end_one(turn, status, session);
        } else {
            for turn in self.attempts.keys().cloned().collect::<Vec<_>>() {
                self.end_one(&turn, "unknown turn", session.clone());
            }
        }
    }
    fn end_one(&mut self, turn: &str, status: &str, session: Option<String>) {
        if let Some(attempt) = self.attempts.get_mut(turn) {
            attempt.ended.get_or_insert_with(|| status.to_owned());
            for tx in attempt.waiting.drain(..) {
                tx.finish(
                    &self.journal,
                    Err(Error::Refused(
                        "turn ended before the interrupt was sent".into(),
                    )),
                );
            }
            self.settle(turn, session);
        }
    }
    fn settle(&mut self, turn: &str, session: Option<String>) {
        let Some(attempt) = self.attempts.get(turn) else {
            return;
        };
        if attempt.requests.is_empty() && attempt.waiting.is_empty() {
            self.attempts.remove(turn);
            return;
        }
        let Some(status) = attempt.ended.clone() else {
            return;
        };
        let Some((&id, _)) = attempt.requests.iter().find(|(_, accepted)| **accepted) else {
            return;
        };
        let observation = self.attempts.remove(turn).and_then(|a| a.observation);
        self.publish(
            turn.to_owned(),
            id,
            session,
            match (status.as_str(), observation) {
                ("interrupted", Some(observation)) => observation,
                _ => Err(format!("turn ended with status {status}")),
            },
        );
    }
    fn publish(
        &mut self,
        turn_id: String,
        request_id: u64,
        session: Option<String>,
        observation: Observation,
    ) {
        let journal = self.journal.clone();
        while self.ending.try_join_next().is_some() {}
        self.ending.spawn(async move {
            let result = match observation {
                Ok(evidence) => evidence.end().await,
                Err(reason) => failed(reason),
            };
            let cleanup = TurnProcessCleanup {
                turn_id: turn_id.clone(),
                request_id,
                result,
            };
            cleanup.publish(&journal, session);
        });
    }
    pub(super) async fn exited(&mut self, session: Option<String>) {
        self.reading.abort_all();
        for (turn, attempt) in std::mem::take(&mut self.attempts) {
            for tx in attempt.waiting {
                tx.finish(
                    &self.journal,
                    Err(Error::Closed("app-server closed before interrupt".into())),
                );
            }
            if let Some((&id, _)) = attempt.requests.first_key_value() {
                self.publish(
                    turn,
                    id,
                    session.clone(),
                    Err("app-server closed before turn cleanup could be confirmed".into()),
                );
            }
        }
        while self.ending.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, ProcessCleanupResult};
    use tokio::sync::oneshot;

    async fn issue(guard: &mut Guard, turn: &str, request: u64) {
        let (tx, _rx) = oneshot::channel();
        assert!(
            guard
                .request(
                    turn,
                    None,
                    AbortReply {
                        request: uuid::Uuid::new_v4(),
                        client: tx
                    }
                )
                .is_none()
        );
        let (observed, waiters) = guard.prepared().await.unwrap();
        assert_eq!(observed, turn);
        assert_eq!(waiters.len(), 1);
        guard.sent(turn, request);
    }

    #[tokio::test]
    async fn matches_the_end_and_accepted_interrupt_in_either_order() {
        let journal = Journal::new(None).unwrap();
        let mut guard = Guard::new(None, journal.clone());
        issue(&mut guard, "old", 31).await;
        guard.ended(Some("old"), "interrupted", Some("root".into()));
        issue(&mut guard, "next", 41).await;
        guard.reply(41, true, Some("root".into()));
        assert!(journal.records().is_empty());
        guard.reply(31, true, Some("root".into()));
        guard.ended(Some("next"), "completed", Some("root".into()));
        issue(&mut guard, "refused", 51).await;
        guard.reply(51, false, Some("root".into()));
        guard.ended(Some("refused"), "interrupted", Some("root".into()));
        let (tx, rx) = oneshot::channel();
        let late = journal.request_abort();
        guard.request(
            "too late",
            None,
            AbortReply {
                request: late,
                client: tx,
            },
        );
        guard.ended(Some("too late"), "completed", Some("root".into()));
        assert!(rx.await.unwrap().is_err());
        guard.exited(Some("root".into())).await;
        let records = journal.records();
        assert!(
            records
                .iter()
                .any(|r| r.native["requestId"] == late.to_string()
                    && r.native["code"] == "no_active_turn")
        );
        assert_eq!(records.len(), 4);
        for (id, turn, reason) in [(31, "old", None), (41, "next", Some("completed"))] {
            let record = records
                .iter()
                .find(|r| r.native["requestId"] == id)
                .unwrap();
            assert_eq!(record.span_id.as_deref(), Some(turn));
            let [Event::TurnProcessCleanup { cleanup }] = &record.events[..] else {
                panic!("missing cleanup");
            };
            assert!(
                matches!(&cleanup.result, ProcessCleanupResult::Failed { reason: actual, signalled: 0, foreign: 0 } if reason.is_none_or(|s| actual.contains(s)))
            );
        }
    }
}
