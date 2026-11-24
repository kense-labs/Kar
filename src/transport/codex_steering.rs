//! Track native receipt and native observation separately for the addressed Codex turn.
use super::SteeringReceipt;
use crate::{Event, SteerStatus};
use serde_json::Value;
use uuid::Uuid;

#[derive(PartialEq)]
enum Admission {
    Pending,
    Received,
    Unconfirmed,
}

struct Entry {
    request: Uuid,
    receipt: SteeringReceipt,
    turn: String,
    admission: Admission,
    echoed: bool,
    interrupted_at_end: Option<bool>,
}

#[derive(Default)]
pub(super) struct Inputs(Vec<Entry>);
impl Inputs {
    pub(super) fn contains(&self, target: &str, input: Uuid) -> bool {
        self.0
            .iter()
            .any(|e| e.turn == target && e.receipt.id == input)
    }

    pub(super) fn submit(&mut self, turn: &str, request: Uuid, receipt: SteeringReceipt) {
        self.0.push(Entry {
            request,
            receipt,
            turn: turn.into(),
            admission: Admission::Pending,
            echoed: false,
            interrupted_at_end: None,
        });
    }

    pub(super) fn echoed(&self, request: Option<Uuid>) -> bool {
        self.0
            .iter()
            .any(|e| Some(e.request) == request && e.echoed)
    }

    pub(super) fn confirms(&self, request: Uuid, body: &Value) -> bool {
        self.0
            .iter()
            .any(|e| e.request == request && body["turnId"] == e.turn)
    }

    pub(super) fn unconfirmed(&mut self, request: Uuid, events: &mut Vec<Event>) {
        self.0.retain_mut(|e| {
            if e.request != request {
                return true;
            }
            e.admission = Admission::Unconfirmed;
            if !e.echoed {
                events.push(e.receipt.event(SteerStatus::Unconfirmed));
            }
            e.interrupted_at_end.is_none()
        });
    }

    pub(super) fn reply(&mut self, request: Uuid, accepted: bool, events: &mut Vec<Event>) {
        self.0.retain_mut(|e| {
            if e.request != request {
                return true;
            }
            if accepted {
                e.admission = Admission::Received;
            }
            if !e.echoed {
                if accepted && e.interrupted_at_end == Some(true) {
                    e.drop_unread(events);
                } else {
                    events.push(e.receipt.event(if accepted {
                        SteerStatus::Received
                    } else {
                        SteerStatus::Refused
                    }));
                }
            }
            accepted && e.interrupted_at_end.is_none()
        });
    }

    pub(super) fn notice(&mut self, packet: &Value, events: &mut Vec<Event>) {
        let p = &packet["params"];
        if packet["method"] == "item/started"
            && p["item"]["type"] == "userMessage"
            && let (Some(turn), Some(input)) = (
                p["turnId"].as_str(),
                p["item"]["clientId"]
                    .as_str()
                    .and_then(|id| id.parse::<Uuid>().ok()),
            )
            && let Some(entry) = self.0.iter_mut().find(|e| {
                e.turn == turn
                    && e.receipt.id == input
                    && e.interrupted_at_end.is_none()
                    && !e.echoed
            })
        {
            entry.echoed = true;
            events.push(entry.receipt.event(SteerStatus::Accepted));
        }
        if packet["method"] == "turn/completed"
            && let (Some(turn), Some(status)) =
                (p["turn"]["id"].as_str(), p["turn"]["status"].as_str())
            && matches!(status, "completed" | "failed" | "interrupted")
        {
            let mut settled = Vec::new();
            self.0.retain_mut(|e| {
                if e.turn != turn || e.interrupted_at_end.is_some() {
                    return true;
                }
                e.interrupted_at_end = Some(status == "interrupted");
                if e.admission == Admission::Received && !e.echoed && status == "interrupted" {
                    e.drop_unread(&mut settled);
                }
                e.admission == Admission::Pending
            });
            let terminal = events
                .iter()
                .position(|e| matches!(e, Event::TurnEnded { .. }))
                .unwrap_or(events.len());
            events.splice(terminal..terminal, settled);
        }
    }
}
impl Entry {
    fn drop_unread(&self, events: &mut Vec<Event>) {
        events.push(self.receipt.event(SteerStatus::Cancelled));
        events.push(Event::InputDropped {
            input_id: self.receipt.id.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_interrupted_turn_settles_before_or_after_its_steer_reply() {
        for reply_first in [false, true] {
            let mut inputs = Inputs::default();
            let request = Uuid::new_v4();
            let id = Uuid::new_v4();
            let owner = Uuid::new_v4();
            inputs.submit(
                "target",
                request,
                SteeringReceipt {
                    id,
                    owner: Some(owner),
                    text: "once".into(),
                },
            );
            let mut events = vec![];
            if reply_first {
                inputs.reply(request, true, &mut events);
            }
            let end = |turn| json!({"method":"turn/completed","params":{"turn":{"id":turn,"status":"interrupted"}}});
            inputs.notice(&end("other"), &mut events);
            inputs.notice(&end("target"), &mut events);
            if !reply_first {
                inputs.reply(request, true, &mut events);
            }
            inputs.notice(&end("target"), &mut events);
            assert_eq!(
                events
                    .iter()
                    .filter(
                        |e| matches!(e,Event::InputDropped {input_id} if *input_id==id.to_string())
                    )
                    .count(),
                1
            );
            assert!(events.iter().any(|e| matches!(e, Event::SteeringInput {
                input_id, chat_turn:Some(target), status:SteerStatus::Cancelled, ..
            } if *input_id==id && *target==owner)));
            assert!(!inputs.contains("target", id));
        }
    }

    #[test]
    fn an_unconfirmed_reply_keeps_the_uuid_reserved_until_native_observation() {
        let mut inputs = Inputs::default();
        let id = Uuid::new_v4();
        let request = Uuid::new_v4();
        inputs.submit(
            "t",
            request,
            SteeringReceipt {
                id,
                owner: None,
                text: "work".into(),
            },
        );
        let mut events = vec![];
        assert!(!inputs.confirms(request, &json!({"turnId":"other"})));
        inputs.unconfirmed(request, &mut events);
        assert!(inputs.contains("t", id));
        let echo = json!({"method":"item/started","params":{"turnId":"t","item":{"type":"userMessage","clientId":id}}});
        inputs.notice(&echo, &mut events);
        inputs.notice(&echo, &mut events);
        inputs.notice(
            &json!({"method":"turn/completed","params":{"turn":{"id":"t","status":"interrupted"}}}),
            &mut events,
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    Event::SteeringInput {
                        status: SteerStatus::Accepted,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::InputDropped { .. }))
        );
        assert!(!inputs.contains("t", id));
    }
}
