use super::{
    CompactionFailure, CompactionStatus as Status, CompactionUnconfirmed as Unconfirmed,
    ManualCompactionReport as Report,
};
use crate::{Error, Event, Protocol, Record, Result, stream::Journal};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

/// Dropping the handle or timing out a wait leaves the native operation running.
pub struct ManualCompaction {
    updates: watch::Receiver<Report>,
    native: oneshot::Receiver<Result<Value>>,
}
impl ManualCompaction {
    pub fn latest(&self) -> Report {
        self.updates.borrow().clone()
    }

    pub async fn wait(&self, timeout: Option<Duration>) -> Report {
        let mut rx = self.updates.clone();
        let wait = async {
            loop {
                let report = rx.borrow_and_update().clone();
                if !report.status.is_pending() {
                    return report;
                }
                if rx.changed().await.is_err() {
                    return report.unconfirmed(Unconfirmed::ObservationFailed);
                }
            }
        };
        match timeout {
            None => wait.await,
            Some(limit) => tokio::time::timeout(limit, wait).await.unwrap_or_else(|_| {
                let report = self.latest();
                if report.status.is_pending() {
                    report.unconfirmed(Unconfirmed::Timeout)
                } else {
                    report
                }
            }),
        }
    }
    pub(crate) async fn native(self, timeout: Duration) -> Result<Value> {
        tokio::time::timeout(timeout, self.native)
            .await
            .map_err(|_| Error::Timeout("native compaction reply".into()))?
            .map_err(|_| Error::Closed("runtime stopped before replying".into()))?
    }
}

pub(crate) struct Operation {
    protocol: Protocol,
    grok: bool,
    command: Option<StreamCommand>,
    id: u64,
    pub report: Report,
    updates: watch::Sender<Report>,
    turn: Option<String>,
    item: Option<String>,
    manual_start: bool,
    boundary: Option<Status>,
    terminal: Option<Status>,
}
impl Operation {
    pub fn from_request(
        protocol: Protocol,
        id: u64,
        request: &Record,
        native: oneshot::Receiver<Result<Value>>,
    ) -> (Self, ManualCompaction) {
        let report = request
            .manual_compaction()
            .expect("outbound request carries a manual compaction marker");
        let grok = request.native["method"] == "_x.ai/compact_conversation";
        let command = request.native["uuid"]
            .as_str()
            .map(|uuid| StreamCommand::new(uuid.into()));
        let (updates, rx) = watch::channel(report.clone());
        (
            Self {
                protocol,
                grok,
                command,
                id,
                report,
                updates,
                turn: None,
                item: None,
                manual_start: false,
                boundary: None,
                terminal: None,
            },
            ManualCompaction {
                updates: rx,
                native,
            },
        )
    }
    pub fn codex_turn(&self) -> Option<&str> {
        self.turn.as_deref()
    }
    pub fn publish(&self, journal: &Journal) {
        journal.host_event(Event::ManualCompaction {
            report: self.report.clone(),
        });
        self.updates.send_replace(self.report.clone());
    }
    fn failed(code: CompactionFailure, reason: &str, frame: &Record) -> Status {
        Status::Failed {
            code,
            reason: reason.into(),
            frame: frame.cursor(),
        }
    }
    pub fn observe(&mut self, frame: &Record) -> bool {
        if self.protocol == Protocol::KimiKap {
            // KAP may deliver started and ended together in a turns snapshot.
            for marker in frame
                .compactions()
                .filter(|m| m.trigger.as_deref().is_none_or(|t| t == "manual"))
            {
                if marker.state == "started" {
                    self.item = marker.compaction_id;
                    self.manual_start = true;
                    continue;
                }
                if self.manual_start && self.item != marker.compaction_id {
                    continue;
                }
                self.report.status = match marker.state.as_str() {
                    "completed" => Status::Completed {
                        frame: frame.cursor(),
                    },
                    "aborted" => Status::Aborted {
                        frame: frame.cursor(),
                    },
                    "failed" => Self::failed(
                        CompactionFailure::NativeFailed,
                        "Kimi reported compaction failure",
                        frame,
                    ),
                    _ => continue,
                };
                return true;
            }
            return false;
        }
        if let Some(command) = &mut self.command {
            let status = if self.protocol == Protocol::Qoder {
                command.observe_qoder(frame)
            } else {
                command.observe_claude(frame)
            };
            if status.is_some() || (command.started && self.report.response.is_none()) {
                self.report.response.get_or_insert_with(|| frame.cursor());
                if let Some(status) = status {
                    self.report.status = status;
                }
                return true;
            }
            return false;
        }
        let v = &frame.native;
        let reply = crate::wire::response_id(self.protocol, v)
            .and_then(|id| id.as_u64().or_else(|| id.as_str()?.parse().ok()))
            == Some(self.id);
        if reply {
            if self.protocol != Protocol::Pi || !self.manual_start {
                self.report.response = Some(frame.cursor());
            }
            if self.grok {
                self.report.status = match frame.compactions().next().map(|r| r.state).as_deref() {
                    Some("completed") => Status::Completed {
                        frame: frame.cursor(),
                    },
                    Some("aborted") => Status::Aborted {
                        frame: frame.cursor(),
                    },
                    Some("failed") => Self::failed(
                        CompactionFailure::NativeFailed,
                        v["error"]["message"]
                            .as_str()
                            .unwrap_or("Grok reported compaction failure"),
                        frame,
                    ),
                    _ if v.get("error").is_some() => Status::Rejected {
                        reason: v["error"]["message"]
                            .as_str()
                            .unwrap_or("Grok refused compaction")
                            .into(),
                    },
                    _ => Status::Unconfirmed {
                        code: Unconfirmed::ObservationFailed,
                    },
                };
                return true;
            }
            if let Err(err) = crate::wire::result(self.protocol, v) {
                let reason = err.to_string();
                self.report.status = if self.manual_start || self.protocol == Protocol::Acp {
                    Self::failed(CompactionFailure::NativeFailed, &reason, frame)
                } else {
                    Status::Rejected { reason }
                };
            } else if self.protocol == Protocol::Factory {
                let result = &v["result"];
                self.report.status = match (
                    result["newSessionId"].as_str(),
                    result["removedCount"].as_u64(),
                ) {
                    (Some(id), _) if Some(id) != frame.session_id.as_deref() => Self::failed(
                        CompactionFailure::NativeFailed,
                        "Factory compacted a different session",
                        frame,
                    ),
                    (_, Some(0)) => Self::failed(
                        CompactionFailure::NotCompacted,
                        "Factory removed no messages",
                        frame,
                    ),
                    (Some(_), Some(_)) => Status::Completed {
                        frame: frame.cursor(),
                    },
                    _ => Status::Unconfirmed {
                        code: Unconfirmed::ObservationFailed,
                    },
                };
            } else if self.protocol == Protocol::Acp {
                self.report.status = self.boundary.take().unwrap_or(Status::Unconfirmed {
                    code: Unconfirmed::ObservationFailed,
                });
            } else if self.protocol == Protocol::Codex {
                self.report.status = self.terminal.take().unwrap_or(Status::Accepted);
            }
            return true;
        }

        match self.protocol {
            Protocol::Codex => self.codex(frame),
            Protocol::Pi | Protocol::Acp => {
                let Some(boundary) = frame
                    .compactions()
                    .find(|r| r.trigger.as_deref() == Some("manual"))
                else {
                    return false;
                };
                if boundary.state == "started" {
                    if self.manual_start
                        || (self.protocol == Protocol::Acp && boundary.compaction_id.is_none())
                    {
                        return false;
                    }
                    self.item = boundary.compaction_id;
                    self.manual_start = true;
                    if self.protocol == Protocol::Pi {
                        self.report.response = Some(frame.cursor());
                        self.report.status = Status::Accepted;
                        return true;
                    }
                    return false;
                }
                if !self.manual_start || self.item != boundary.compaction_id {
                    return false;
                }
                let status = match boundary.state.as_str() {
                    "completed" => Status::Completed {
                        frame: frame.cursor(),
                    },
                    "aborted" => Status::Aborted {
                        frame: frame.cursor(),
                    },
                    "failed" => Self::failed(
                        CompactionFailure::NativeFailed,
                        boundary
                            .reason
                            .as_deref()
                            .unwrap_or("native compaction failed"),
                        frame,
                    ),
                    _ => return false,
                };
                if self.protocol == Protocol::Pi {
                    self.report.status = status;
                    true
                } else {
                    self.boundary.get_or_insert(status);
                    false
                }
            }
            _ => false,
        }
    }
    fn codex(&mut self, frame: &Record) -> bool {
        let v = &frame.native;
        let p = &v["params"];
        if self.terminal.is_some()
            || p["threadId"].as_str().is_none()
            || p["threadId"].as_str() != frame.session_id.as_deref()
        {
            return false;
        }
        if p["item"]["type"] == "contextCompaction" {
            let (Some(id), Some(turn)) = (p["item"]["id"].as_str(), p["turnId"].as_str()) else {
                return false;
            };
            if v["method"] == "item/started" && self.turn.as_deref().is_none_or(|t| t == turn) {
                self.turn = Some(turn.into());
                if self.item.as_deref() != Some(id) {
                    self.boundary = None;
                }
                self.item = Some(id.into());
            } else if v["method"] == "item/completed"
                && self.turn.as_deref() == Some(turn)
                && self.item.as_deref() == Some(id)
            {
                self.boundary = Some(Status::Completed {
                    frame: frame.cursor(),
                });
            }
            return false;
        }
        if v["method"] != "turn/completed"
            || self.turn.is_none()
            || self.turn.as_deref() != p["turn"]["id"].as_str()
        {
            return false;
        }
        let end = match p["turn"]["status"].as_str() {
            Some("interrupted") => Status::Aborted {
                frame: frame.cursor(),
            },
            Some("failed") => Self::failed(
                CompactionFailure::NativeFailed,
                "Codex reported compaction turn failure",
                frame,
            ),
            Some("completed") if self.boundary.is_some() => self.boundary.take().unwrap(),
            _ => Self::failed(
                CompactionFailure::NotCompacted,
                "Codex ended the turn without the matching completed compaction item",
                frame,
            ),
        };
        if self.report.response.is_none() {
            self.terminal = Some(end);
            return false;
        }
        self.report.status = end;
        true
    }
}

struct StreamCommand {
    uuid: String,
    started: bool,
    changed: bool,
    boundary: Option<crate::Cursor>,
    failure: Option<crate::Cursor>,
}
impl StreamCommand {
    fn new(uuid: String) -> Self {
        Self {
            uuid,
            started: false,
            changed: false,
            boundary: None,
            failure: None,
        }
    }
    // Qoder's result has no input UUID. Only the matching command lifecycle settles it.
    fn observe_qoder(&mut self, frame: &Record) -> Option<Status> {
        let v = &frame.native;
        if v["type"] == "command_lifecycle" && v["command_uuid"] == self.uuid {
            match v["state"].as_str()? {
                "started" => self.started = true,
                state @ ("completed" | "cancelled") => {
                    return Some(if let Some(boundary) = self.boundary.take() {
                        Status::Completed { frame: boundary }
                    } else if state == "cancelled" {
                        Status::Aborted {
                            frame: frame.cursor(),
                        }
                    } else if let Some(failed) = self.failure.take() {
                        Status::Failed {
                            code: CompactionFailure::NativeFailed,
                            reason: "Qoder reported a compact command failure".into(),
                            frame: failed,
                        }
                    } else {
                        Operation::failed(
                            CompactionFailure::NotCompacted,
                            "Qoder closed the command without a manual compaction boundary",
                            frame,
                        )
                    });
                }
                _ => {}
            }
        } else if self.started {
            if v["type"] == "result" {
                self.failure = (v["is_error"] == true).then(|| frame.cursor());
            } else if frame
                .compactions()
                .any(|c| c.state == "completed" && c.trigger.as_deref() == Some("manual"))
            {
                self.boundary = Some(frame.cursor());
            }
        }
        None
    }
    fn observe_claude(&mut self, frame: &Record) -> Option<Status> {
        let v = &frame.native;
        if v["type"] == "command_lifecycle" && v["command_uuid"] == self.uuid {
            match v["state"].as_str() {
                Some("started") => self.started = true,
                Some("cancelled") if !self.started => {
                    return Some(Status::Aborted {
                        frame: frame.cursor(),
                    });
                }
                _ => {}
            }
        }
        if v["type"] == "system" {
            match v["subtype"].as_str() {
                Some("commands_changed") if self.boundary.is_none() => self.changed = true,
                Some("compact_boundary")
                    if self.started && v["compact_metadata"]["trigger"] == "manual" =>
                {
                    self.boundary = Some(frame.cursor())
                }
                Some("status") if self.started && v["compact_result"] == "failed" => {
                    self.failure = Some(frame.cursor())
                }
                _ => {}
            }
        }
        if v["type"] != "result" || v["user_message_uuid"] != self.uuid {
            return None;
        }
        Some(if self.changed {
            Operation::failed(
                CompactionFailure::CommandChanged,
                "Claude commands changed before the compaction boundary",
                frame,
            )
        } else if let Some(failed) = self.failure.take() {
            Status::Failed {
                code: CompactionFailure::NativeFailed,
                reason: "Claude reported compaction failure".into(),
                frame: failed,
            }
        } else if v["is_error"] == true {
            Operation::failed(
                CompactionFailure::NativeFailed,
                "Claude reported compact command failure",
                frame,
            )
        } else if let Some(boundary) = self.boundary.take() {
            Status::Completed { frame: boundary }
        } else {
            Operation::failed(
                CompactionFailure::NotCompacted,
                "Claude returned a result without a matching manual compaction boundary",
                frame,
            )
        })
    }
}
