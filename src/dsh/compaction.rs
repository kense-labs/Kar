//! Correlate the command receipt with the root session's durable transaction.
use crate::{
    CompactionFailure, CompactionStatus as Status, CompactionUnconfirmed as Unconfirmed, Cursor,
    Direction, Error, Event, ManualCompaction, Protocol, Record, RecordKind, Result,
    compaction::Operation, stream::Journal,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::sync::oneshot;

#[derive(Default)]
struct Evidence {
    transaction: Option<String>,
    summary: Option<u64>,
    end: Option<(Cursor, bool)>,
    done: Option<(String, Option<u64>, Cursor)>,
}

pub(super) struct Pending {
    pub cancel_requested: bool,
    op: Operation,
    native: oneshot::Sender<Result<Value>>,
    commands: HashMap<String, Evidence>,
}
impl Pending {
    pub fn start(journal: &Journal, args: &Value) -> (Self, ManualCompaction) {
        journal.push(
            Direction::ToRuntime,
            RecordKind::Request,
            None,
            vec![],
            json!({"method":"commands/execute","args":args}),
            vec![Event::ManualCompactionRequested],
        );
        let request = journal
            .latest_record()
            .expect("just recorded compaction request");
        let (native, receive) = oneshot::channel();
        let (op, handle) = Operation::from_request(Protocol::Dsh, 0, &request, receive);
        (
            Self {
                op,
                cancel_requested: false,
                native,
                commands: HashMap::new(),
            },
            handle,
        )
    }

    pub fn can_abort(&self) -> bool {
        self.commands
            .values()
            .any(|trace| trace.transaction.is_some() && trace.end.is_none())
    }
    pub fn observe(&mut self, record: &Record) {
        let event = &record.native["event"];
        let data = &event["data"];
        let Some(kind) = event["type"].as_str() else {
            return;
        };
        if kind == "command/run"
            && data["name"] == "compact"
            && let Some(id) = data["commandId"].as_str()
        {
            self.commands.entry(id.into()).or_default();
            return;
        }
        let Some(id) = data["sourceCommandId"]
            .as_str()
            .or(data["commandId"].as_str())
        else {
            return;
        };
        let Some(trace) = self.commands.get_mut(id) else {
            return;
        };
        if kind == "command/done" {
            if let Some(result) = data["kind"].as_str() {
                trace.done = Some((
                    result.into(),
                    data["sourceEventSeq"].as_u64(),
                    record.cursor(),
                ));
            }
            return;
        }
        let Some(tx) = data["compactionId"].as_str() else {
            return;
        };
        if !data["turn"].is_null() {
            return;
        }
        if kind == "compaction/start" {
            trace.transaction = Some(tx.into());
            trace.summary = None;
            trace.end = None;
        } else if trace.transaction.as_deref() == Some(tx) {
            match kind {
                "compaction/summary" => trace.summary = event["seq"].as_u64(),
                "compaction/end" => trace.end = Some((record.cursor(), data["error"].is_null())),
                _ => {}
            }
        }
    }

    pub fn finish(mut self, journal: &Journal, reply: Result<Value>) {
        let payload = reply.as_ref().ok().cloned().unwrap_or(Value::Null);
        journal.push(
            Direction::FromRuntime,
            RecordKind::Response,
            None,
            vec![],
            payload.clone(),
            vec![],
        );
        self.op.report.response = journal.latest_record().map(|r| r.cursor());
        self.op.report.status = match &reply {
            Ok(v) if v.is_null() => Status::Rejected {
                code: crate::CompactionRejection::RuntimeRefused,
                reason: "DSH does not provide /compact".into(),
            },
            Ok(v) => self.status(v).unwrap_or(Status::Unconfirmed {
                code: Unconfirmed::ObservationFailed,
            }),
            Err(Error::Refused(e)) => Status::Rejected {
                code: crate::CompactionRejection::RuntimeRefused,
                reason: e.clone(),
            },
            Err(_) => Status::Unconfirmed {
                code: Unconfirmed::ObservationFailed,
            },
        };
        self.op.publish(journal);
        let _ = self.native.send(reply);
    }

    fn status(&self, reply: &Value) -> Option<Status> {
        let trace = self.commands.get(reply["commandId"].as_str()?)?;
        let (kind, summary, done) = trace.done.as_ref()?;
        let result = &reply["result"];
        if result["kind"] != *kind || result["sourceEventSeq"].as_u64() != *summary {
            return None;
        }
        match kind.as_str() {
            "success" if summary.is_none() && trace.transaction.is_none() => Some(Status::Failed {
                code: CompactionFailure::NotCompacted,
                reason: "DSH found no compactable history".into(),
                frame: done.clone(),
            }),
            "success" if summary.is_some() && *summary == trace.summary => {
                let (cursor, ok) = trace.end.as_ref()?;
                ok.then(|| Status::Completed {
                    frame: cursor.clone(),
                })
            }
            "error"
                if self.cancel_requested
                    && result["text"] == "Compaction cancelled."
                    && trace.end.as_ref().is_some_and(|(_, ok)| !ok) =>
            {
                Some(Status::Aborted {
                    frame: trace.end.as_ref()?.0.clone(),
                })
            }
            "error" => Some(Status::Failed {
                code: CompactionFailure::NativeFailed,
                reason: result["text"]
                    .as_str()
                    .unwrap_or("DSH compaction failed")
                    .into(),
                frame: done.clone(),
            }),
            _ => None,
        }
    }

    pub fn close(mut self, journal: &Journal, code: Unconfirmed) {
        self.op.report = self.op.report.unconfirmed(code);
        self.op.publish(journal);
        let _ = self.native.send(Err(Error::Closed(
            "DSH compaction connection closed".into(),
        )));
    }
}
