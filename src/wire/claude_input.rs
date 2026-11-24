use crate::{Event, FailureClass, InputReceipt, SteerStatus, TurnFailure};
use serde_json::Value;
use uuid::Uuid;

pub(crate) fn result_id(frame: &Value) -> Option<Uuid> {
    if frame["type"] != "result" || !frame["parent_tool_use_id"].is_null() {
        return None;
    }
    Uuid::parse_str(frame["uuid"].as_str()?).ok()
}

/// Native result packets can acknowledge several inputs in one model loop.
pub(crate) fn model_inputs(frame: &Value) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for value in frame["user_message_uuids"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(frame.get("user_message_uuid"))
    {
        if let Some(id) = value.as_str().and_then(|s| Uuid::parse_str(s).ok())
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    ids
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Written,
    Queued,
    Started,
    Executing,
    Settled,
}

pub(crate) struct Input {
    pub(crate) id: Uuid,
    stage: Stage,
    unclaimed_init: bool,
}

impl Input {
    pub(crate) fn written(id: Uuid) -> Self {
        Self {
            id,
            stage: Stage::Written,
            unclaimed_init: false,
        }
    }

    pub(crate) fn unstarted(&self) -> bool {
        matches!(self.stage, Stage::Written | Stage::Queued)
    }

    /// A queued prompt can wait behind a spontaneous notification. A result
    /// may also list several UUIDs when the prompt joins a native tool loop.
    fn includes(&self, frame: &Value) -> Option<bool> {
        let mut found_valid = false;
        let mut malformed =
            !frame["user_message_uuids"].is_null() && !frame["user_message_uuids"].is_array();
        let primary = frame.get("user_message_uuid").filter(|v| !v.is_null());
        let ids = frame["user_message_uuids"].as_array();
        for value in primary.into_iter().chain(ids.into_iter().flatten()) {
            match value.as_str().and_then(|s| Uuid::parse_str(s).ok()) {
                Some(id) if id == self.id => return Some(true),
                Some(_) => found_valid = true,
                None => malformed = true,
            }
        }
        (found_valid && !malformed).then_some(false)
    }

    pub(crate) fn observe(&mut self, frame: &Value, events: &mut Vec<Event>) {
        if self.unstarted()
            && matches!(
                frame["type"].as_str(),
                Some("assistant" | "stream_event" | "result")
            )
            && self.includes(frame) == Some(true)
        {
            // This UUID is already driving a model call, even on a runtime that
            // reports its started receipt later or omits the lifecycle signal.
            self.stage = Stage::Executing;
        }
        if frame["type"] == "system" && frame["subtype"] == "init" && self.stage == Stage::Written {
            self.unclaimed_init = true;
        }
        let (id, state) = match frame["type"].as_str() {
            Some("command_lifecycle") => (&frame["command_uuid"], frame["state"].as_str()),
            Some("user") if frame["isReplay"] == true => (&frame["uuid"], Some("replay")),
            _ => return,
        };
        if id.as_str().and_then(|s| Uuid::parse_str(s).ok()) != Some(self.id) {
            return;
        }
        let receipt = match (self.stage, state) {
            (Stage::Written, Some("queued")) => {
                self.stage = Stage::Queued;
                InputReceipt::Queued
            }
            (Stage::Written | Stage::Queued | Stage::Executing, Some("started")) => {
                self.stage = Stage::Started;
                InputReceipt::Started
            }
            (Stage::Written, Some("completed" | "replay")) => InputReceipt::Duplicate,
            (Stage::Written | Stage::Queued, Some("cancelled")) => InputReceipt::Cancelled,
            (_, Some("completed" | "cancelled")) => {
                self.stage = Stage::Settled;
                return;
            }
            _ => return,
        };
        events.push(Event::NativeInput {
            input_id: self.id,
            receipt,
        });
        if receipt.settled_outcome().is_none() {
            return;
        }
        self.stage = Stage::Settled;
        events.push(Event::InputDropped {
            input_id: self.id.to_string(),
        });
        if receipt == InputReceipt::Duplicate {
            events.push(Event::TurnFailure {
                failure: TurnFailure::new(
                    FailureClass::InvalidRequest,
                    "Claude ignored an input UUID already used in this session",
                ),
            });
        }
    }
}

struct Steer {
    owner: Uuid,
    text: String,
    status: SteerStatus,
    execution: Option<Uuid>,
}
impl Steer {
    fn event(&self, id: Uuid) -> Event {
        Event::SteeringInput {
            input_id: id,
            chat_turn: Some(self.owner),
            text: self.text.clone(),
            status: self.status,
        }
    }
    fn waiting(&self) -> bool {
        self.status.is_waiting()
    }
}

#[derive(Default)]
pub(crate) struct Steering {
    entries: std::collections::HashMap<Uuid, Steer>,
    lifecycle: bool,
}
#[derive(Default)]
pub(crate) struct SteeringObservation {
    pub updates: Vec<Event>,
    pub host_step: bool,
    pub activity: Option<Uuid>,
    pub new_turn: Option<(Uuid, Uuid)>,
    pub moved: Vec<Event>,
}
impl Steering {
    pub fn submit(&mut self, id: Uuid, owner: Uuid, text: String) {
        self.entries.insert(
            id,
            Steer {
                owner,
                text,
                status: SteerStatus::Submitted,
                execution: None,
            },
        );
    }
    pub fn submitted(&self, packet: &Value) -> Option<Event> {
        let id = Uuid::parse_str(packet["uuid"].as_str()?).ok()?;
        self.entries.get(&id).map(|s| s.event(id))
    }
    pub fn waiting(&self) -> bool {
        self.lifecycle && self.entries.values().any(Steer::waiting)
    }
    pub fn ended(&mut self, group: Uuid) {
        self.entries
            .retain(|_, entry| entry.execution != Some(group));
    }
    pub fn disconnect(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.entries.drain().filter_map(|(id, mut steer)| {
            if !steer.waiting() {
                return None;
            }
            steer.status = SteerStatus::Unconfirmed;
            Some(steer.event(id))
        })
    }
    pub fn observe(
        &mut self,
        frame: &Value,
        host: Option<Uuid>,
        notice: Option<Uuid>,
    ) -> SteeringObservation {
        let mut out = SteeringObservation::default();
        let (signal, ids) = match frame["type"].as_str() {
            Some("command_lifecycle") => {
                let Some(id) = frame["command_uuid"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                else {
                    return out;
                };
                self.lifecycle = true;
                (frame["state"].as_str().unwrap_or_default(), vec![id])
            }
            Some("user") if frame["isReplay"] == true => {
                let Some(id) = frame["uuid"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
                    return out;
                };
                ("replay", vec![id])
            }
            Some("assistant" | "stream_event" | "result") if !self.entries.is_empty() => {
                ("model", model_inputs(frame))
            }
            _ => return out,
        };
        for id in ids {
            let Some(entry) = self.entries.get_mut(&id) else {
                continue;
            };
            let prior = entry.status;
            match signal {
                "queued" if prior == SteerStatus::Submitted => entry.status = SteerStatus::Queued,
                "cancelled" if entry.waiting() => entry.status = SteerStatus::Cancelled,
                "replay" | "completed" if self.lifecycle && prior == SteerStatus::Submitted => {
                    entry.status = SteerStatus::Refused
                }
                "started" | "model" if entry.waiting() => {
                    let current = notice.or(host);
                    if current == Some(entry.owner) {
                        entry.status = SteerStatus::Accepted;
                        entry.execution = current;
                    } else {
                        entry.status = SteerStatus::FollowUp;
                        let target = notice.or(out.activity).unwrap_or(id);
                        if current.is_none() && out.activity.is_none() {
                            out.new_turn = Some((id, entry.owner));
                        } else {
                            out.moved.push(Event::SteeringMoved {
                                input_id: id,
                                from: entry.owner,
                                to: target,
                            });
                        }
                        entry.execution = Some(target);
                    }
                }
                "completed" if prior == SteerStatus::Queued => entry.status = SteerStatus::Accepted,
                _ => {}
            }
            if signal == "started" || signal == "model" {
                if entry.status == SteerStatus::FollowUp {
                    out.activity = entry.execution;
                } else if host.is_some() && entry.execution == host {
                    out.host_step = true;
                }
            }
            if prior != entry.status {
                out.updates.push(entry.event(id));
                if matches!(entry.status, SteerStatus::Cancelled | SteerStatus::Refused) {
                    out.updates.push(Event::InputDropped {
                        input_id: id.to_string(),
                    });
                }
            }
            if matches!(entry.status, SteerStatus::Cancelled | SteerStatus::Refused)
                || (signal == "completed" && entry.execution.is_none())
            {
                self.entries.remove(&id);
            }
        }
        out
    }
}

/// Group native work that starts before Claude takes the queued host input.
/// The identifier names a local observation, independent of vendor input IDs.
#[derive(Default)]
pub(crate) struct Notifications {
    active: Option<Uuid>,
    tools: std::collections::HashMap<String, Uuid>,
    interrupted: Option<Uuid>,
}
impl Notifications {
    pub(crate) fn id(&self) -> Option<Uuid> {
        self.active
    }
    pub(crate) fn active(&self) -> bool {
        self.active.is_some()
    }
    pub(crate) fn interrupt_target(
        &self,
        input: Option<&Input>,
        cancel_queued: bool,
    ) -> Option<Uuid> {
        self.active.or_else(|| {
            input
                .filter(|i| i.stage != Stage::Queued || cancel_queued)
                .map(|i| i.id)
        })
    }
    pub(crate) fn acknowledge_interrupt(&mut self, target: Option<Uuid>) {
        if target == self.active {
            self.interrupted = target;
        }
    }
    pub(crate) fn was_interrupted(&self, events: &[Event]) -> bool {
        self.interrupted.is_some_and(|wanted| {
            events
                .iter()
                .any(|e| matches!(e, Event::NotificationActivity { id } if *id == wanted))
        })
    }
    pub(crate) fn child(&self, ancestry: &[String], events: &mut Vec<Event>) {
        if let Some(id) = ancestry.iter().find_map(|p| self.tools.get(p)).copied() {
            events.insert(0, Event::NotificationActivity { id });
        }
    }
    pub(crate) fn track_tools(&mut self, events: &[Event]) {
        let owner = events.iter().find_map(|e| match e {
            Event::NotificationActivity { id } => Some(*id),
            _ => None,
        });
        for e in events {
            if let Event::ToolStarted { id, .. } | Event::ToolInput { id, .. } = e {
                if let Some(group) = owner {
                    self.tools.insert(id.clone(), group);
                } else {
                    self.tools.remove(id);
                }
            }
        }
    }

    pub(crate) fn observe(
        &mut self,
        input: Option<&Input>,
        frame: &Value,
        steering: &SteeringObservation,
        events: &mut Vec<Event>,
    ) {
        if let Some(id) = steering.activity {
            self.active = (frame["type"] != "result").then_some(id);
            events.insert(0, Event::NotificationActivity { id });
            return;
        }
        if let Some((id, receipt)) = events.iter().find_map(|e| match e {
            Event::NativeInput { input_id, receipt } => Some((*input_id, *receipt)),
            _ => None,
        }) {
            match receipt {
                InputReceipt::Started => {
                    if let Some(group) = self.active.take() {
                        events.push(Event::NotificationJoined {
                            id: group,
                            input_id: id,
                        });
                    }
                }
                InputReceipt::Queued if input.is_some_and(|i| i.unclaimed_init) => {
                    self.active.get_or_insert_with(Uuid::new_v4);
                }
                _ => {}
            }
            return;
        }
        let kind = frame["type"].as_str().unwrap_or_default();
        // Replayed user inputs, child receipts and control replies do not
        // indicate an independent model turn.
        let model_activity = matches!(kind, "assistant" | "stream_event" | "result" | "user")
            || (kind == "system" && frame["subtype"] == "init");
        if !model_activity || frame["isReplay"] == true {
            return;
        }
        let belongs = if steering.host_step {
            Some(true)
        } else {
            input.and_then(|i| i.includes(frame))
        };
        if belongs == Some(true) {
            if let (Some(id), Some(input)) = (self.active.take(), input) {
                events.push(Event::NotificationJoined {
                    id,
                    input_id: input.id,
                });
            }
            return;
        }
        // An idle CLI can start a model call without any host write. A startup
        // init or keepalive by itself is insufficient evidence of model activity.
        let idle_model = input.is_none()
            && (kind == "assistant"
                || (kind == "stream_event" && frame["event"]["type"] == "message_start")
                || result_id(frame).is_some());
        if self.active.is_none()
            && !idle_model
            && !input.is_some_and(|i| i.stage == Stage::Queued)
            && !(kind == "result" && belongs == Some(false))
        {
            return;
        }
        let id = *self.active.get_or_insert_with(Uuid::new_v4);
        events.insert(0, Event::NotificationActivity { id });
        if kind == "result" {
            self.active = None;
        }
    }
}
