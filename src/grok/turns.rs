use crate::{Error, Event, InputReceipt, Outcome, Result, SteerStatus, TurnFailure};
use serde_json::Value;
use std::collections::HashSet;
use uuid::Uuid;

pub(crate) const INTERJECT: &str = "_x.ai/interject";

pub(crate) fn interjection_reply(result: &mut Result<Value>) -> SteerStatus {
    let Ok(body) = result else {
        return SteerStatus::Refused;
    };
    if let Some(error) = body.get("error").filter(|v| !v.is_null()) {
        *result = Err(Error::Refused(
            error["message"]
                .as_str()
                .unwrap_or("Grok refused the interjection")
                .into(),
        ));
        return SteerStatus::Refused;
    }
    if body
        .pointer("/result/status")
        .or_else(|| body.get("status"))
        .and_then(Value::as_str)
        == Some("queued")
    {
        return SteerStatus::Queued;
    }
    *result = Err(Error::Protocol(
        "Grok returned an unknown interjection result".into(),
    ));
    SteerStatus::Unconfirmed
}

struct NativeRun {
    prompt: String,
    id: Uuid,
    published: bool,
    aborted: bool,
}

#[derive(Default)]
pub(crate) struct Turns {
    pub root: Option<String>,
    sent: HashSet<String>,
    host_prompt: Option<String>,
    host_input: Option<Uuid>,
    host_receipt: Option<InputReceipt>,
    queue_events: HashSet<String>,
    running: Option<String>,
    own: Option<NativeRun>,
    ended: HashSet<String>,
    last_end: Option<String>,
}

impl Turns {
    pub fn name(&mut self, input: Option<Uuid>) -> String {
        let id = Uuid::new_v4().to_string();
        self.sent.insert(id.clone());
        self.host_prompt = Some(id.clone());
        self.host_input = input;
        self.host_receipt = None;
        id
    }

    pub fn host_ended(&mut self, events: &mut Vec<Event>) {
        self.host_prompt = None;
        if self.host_receipt.take() == Some(InputReceipt::Queued)
            && let Some(input_id) = self.host_input
        {
            events.push(Event::InputRemoved {
                input_id: input_id.to_string(),
            });
        }
    }

    fn adopt(&mut self, id: String) {
        self.own = Some(NativeRun {
            prompt: id,
            id: Uuid::new_v4(),
            published: false,
            aborted: false,
        });
        self.last_end = None;
    }

    pub fn active_prompt(&self) -> Option<&str> {
        self.own
            .as_ref()
            .filter(|r| r.published)
            .map(|r| r.prompt.as_str())
            .or(self.host_prompt.as_deref())
    }

    pub fn owner(&self) -> Option<Uuid> {
        self.own.as_ref().filter(|r| r.published).map(|r| r.id)
    }

    pub fn abort(&mut self) {
        if let Some(run) = self.own.as_mut().filter(|r| r.published) {
            run.aborted = true;
        }
    }

    pub fn aborting(&self) -> bool {
        self.own.as_ref().is_some_and(|r| r.published && r.aborted)
    }

    pub fn publish(&mut self, events: &mut Vec<Event>) {
        if let Some(run) = self.own.as_mut().filter(|r| !r.published) {
            run.published = true;
            events.push(Event::NativeTurnStarted {
                id: run.id,
                native_turn: run.prompt.clone(),
            });
            events.push(Event::TurnStarted);
        }
    }

    pub fn live(&self, params: &Value) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| params["sessionId"].as_str() == Some(root))
            && params["_meta"]["isReplay"] != true
    }

    pub fn observe(&mut self, packet: &Value, host_busy: bool, events: &mut Vec<Event>) {
        let p = &packet["params"];
        if !self.live(p) {
            return;
        }
        match packet["method"].as_str() {
            Some("_x.ai/queue/changed") => {
                if let Some(key) = p["_meta"]["eventId"].as_str().filter(|s| !s.is_empty())
                    && !self.queue_events.insert(key.into())
                {
                    return;
                }
                if self.host_receipt.is_none()
                    && let (Some(host), Some(input_id)) = (&self.host_prompt, self.host_input)
                    && p["entries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|entry| entry["id"].as_str() == Some(host))
                {
                    self.host_receipt = Some(InputReceipt::Queued);
                    events.push(Event::InputQueued {
                        input_id: input_id.to_string(),
                    });
                    events.push(Event::NativeInput {
                        input_id,
                        receipt: InputReceipt::Queued,
                    });
                }
                let running = p["runningPromptId"].as_str().filter(|id| !id.is_empty());
                if self.running.as_deref() == running {
                    return;
                }
                self.running = running.map(str::to_owned);
                let Some(id) = running else { return };
                if self.sent.contains(id) {
                    if self.host_prompt.as_deref() != Some(id)
                        || self.host_receipt == Some(InputReceipt::Started)
                    {
                        return;
                    }
                    let was_queued = self.host_receipt == Some(InputReceipt::Queued);
                    self.host_receipt = Some(InputReceipt::Started);
                    if let Some(input_id) = self.host_input {
                        if was_queued {
                            events.push(Event::InputRemoved {
                                input_id: input_id.to_string(),
                            });
                        }
                        events.push(Event::NativeInput {
                            input_id,
                            receipt: InputReceipt::Started,
                        });
                    }
                    events.push(Event::TurnStarted);
                } else if self.own.is_none() {
                    self.adopt(id.into());
                    if !host_busy {
                        self.publish(events);
                    }
                }
            }
            Some("_x.ai/session_notification") => {
                let update = &p["update"];
                let Some(id) = update["prompt_id"].as_str().filter(|id| !id.is_empty()) else {
                    return;
                };
                if update["sessionUpdate"] != "turn_completed" || self.sent.contains(id) {
                    return;
                }
                if let Some(key) = p["_meta"]["eventId"].as_str().filter(|s| !s.is_empty()) {
                    if !self.ended.insert(key.into()) {
                        return;
                    }
                } else if self.last_end.as_deref() == Some(id) {
                    return;
                }
                self.last_end = Some(id.into());
                if self.running.as_deref() == Some(id) {
                    self.running = None;
                }
                if self.own.as_ref().is_some_and(|r| r.prompt != id) {
                    return;
                }
                self.own.take();
                // A queue snapshot for the next execution can precede this terminal notification.
                if let Some(next) = self.running.clone().filter(|id| !self.sent.contains(id)) {
                    self.adopt(next);
                }
                if host_busy {
                    return;
                }
                let outcome = match update["stop_reason"].as_str() {
                    Some("cancelled") => Outcome::Aborted,
                    Some("error") => {
                        let reason = update["agent_result"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("Grok reported an error");
                        events.push(Event::TurnFailure {
                            failure: TurnFailure::new(crate::failure::prose(reason), reason),
                        });
                        Outcome::Failed
                    }
                    _ => Outcome::Completed,
                };
                events.push(Event::TurnEnded { outcome });
            }
            _ => {}
        }
    }
}
