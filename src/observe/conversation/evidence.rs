use super::*;
use crate::{InputReceipt, SteerStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoEvidence {
    Acknowledged,
    TurnItem,
    Conversation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputEcho {
    pub cursor: Cursor,
    pub session_id: Option<String>,
    pub agent_path: Vec<String>,
    pub input_id: Option<String>,
    pub native_message_id: Option<String>,
    pub turn_id: Option<String>,
    pub text: String,
    pub evidence: EchoEvidence,
}

fn text(content: &Value) -> String {
    content.as_str().map(str::to_owned).unwrap_or_else(|| {
        content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect()
    })
}
impl Record {
    fn echo(
        &self,
        evidence: EchoEvidence,
        input: Option<&str>,
        message: Option<&str>,
        turn: Option<&str>,
        text: String,
    ) -> InputEcho {
        InputEcho {
            cursor: self.cursor(),
            session_id: self.session_id.clone(),
            agent_path: self.agent_path.clone(),
            input_id: input.map(str::to_owned),
            native_message_id: message.map(str::to_owned),
            turn_id: turn.map(str::to_owned),
            text,
            evidence,
        }
    }

    /// Only native inbound user-message evidence can create an observation. In particular,
    /// writing a prompt to the worker does not acknowledge it.
    pub fn input_echoes(&self) -> Vec<InputEcho> {
        if self.elided && !self.elided_input_echoes.is_empty() {
            return self.elided_input_echoes.clone();
        }
        if self.direction != Direction::FromRuntime || self.kind != RecordKind::Frame {
            return vec![];
        }
        if self.upstream.is_some() {
            return body(self)["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["kind"] == "user_message")
                .filter_map(|e| {
                    Some(self.echo(
                        serde_json::from_value(e["evidence"].clone()).ok()?,
                        e["inputId"].as_str(),
                        e["nativeMessageId"].as_str(),
                        e["turnId"].as_str(),
                        e["input"].as_str()?.into(),
                    ))
                })
                .collect();
        }
        let v = &self.native;
        let params = &v["params"];
        let item = &params["item"];
        if matches!(
            v["method"].as_str(),
            Some("item/started" | "item/completed")
        ) && item["type"] == "userMessage"
            && item["content"].is_array()
        {
            // A completed item with no native ID cannot be deduplicated against its started item.
            if v["method"] == "item/completed" && item["id"].as_str().is_none() {
                return vec![];
            }
            return vec![self.echo(
                EchoEvidence::TurnItem,
                item["clientId"].as_str(),
                item["id"].as_str(),
                params["turnId"].as_str(),
                text(&item["content"]),
            )];
        }
        if v["type"] == "user"
            && v["isReplay"] == true
            && let Some(id) = v["uuid"].as_str()
            && let Some(content) = v.pointer("/message/content")
        {
            return vec![self.echo(
                EchoEvidence::Acknowledged,
                Some(id),
                Some(id),
                None,
                text(content),
            )];
        }
        let event = &v["event"];
        if event["type"] == "user/message" && event["data"]["source"]["kind"] == "user" {
            let id = event["data"]["source"]["rpcId"].as_str();
            return vec![self.echo(
                EchoEvidence::TurnItem,
                id,
                id,
                self.span_id.as_deref(),
                text(&event["data"]["content"]),
            )];
        }
        let item = &v["item"];
        if item["type"] == "user_message"
            && let Some(value) = item["text"].as_str()
        {
            // One Kimi item may bundle several prompts. Its combined text identifies no single one.
            let id = item["prompt_ids"].as_array().and_then(|ids| {
                if let [id] = ids.as_slice() {
                    id.as_str()
                } else {
                    None
                }
            });
            return vec![self.echo(
                EchoEvidence::TurnItem,
                id,
                item["id"].as_str(),
                self.span_id.as_deref(),
                value.into(),
            )];
        }
        let msg = &params["notification"]["message"];
        if v["method"] == "droid.session_notification"
            && msg["role"] == "user"
            && self
                .events
                .iter()
                .any(|e| matches!(e, Event::UserMessage { .. }))
        {
            let id = msg["id"].as_str();
            return vec![self.echo(
                EchoEvidence::TurnItem,
                id,
                id,
                self.span_id.as_deref(),
                text(&msg["content"]),
            )];
        }
        if v["type"] == "message_start" && v["message"]["role"] == "user" {
            return vec![self.echo(
                EchoEvidence::Conversation,
                None,
                None,
                None,
                text(&v["message"]["content"]),
            )];
        }
        if self.frame_type() == Some("history/input") && v["kind"] == "input" {
            let message = v["payload"]["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .or_else(|| v["id"].as_u64().map(|id| format!("cybion-record-{id}")));
            if let Some(text) = self.events.iter().find_map(|e| match e {
                Event::UserMessage { text } => Some(text),
                _ => None,
            }) {
                return vec![self.echo(
                    EchoEvidence::Conversation,
                    None,
                    message.as_deref(),
                    self.span_id.as_deref(),
                    text.clone(),
                )];
            }
        }
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::UserMessage { text } => Some(self.echo(
                    EchoEvidence::Conversation,
                    None,
                    None,
                    self.span_id.as_deref(),
                    text.clone(),
                )),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
pub(super) enum Signal {
    Turn(TurnPhase),
    Handled,
    Drop(DropReason),
}

pub(super) fn signals(r: &Record, mut apply: impl FnMut(Option<&str>, Option<&str>, Signal)) {
    if r.upstream.is_some() {
        for e in body(r)["events"].as_array().into_iter().flatten() {
            let sig = match e["kind"].as_str() {
                Some("input_queued") => Signal::Turn(TurnPhase::Queued),
                Some("turn_active") => Signal::Turn(TurnPhase::Active),
                Some("input_handled") => Signal::Handled,
                Some("input_dropped") => match e["reason"].as_str() {
                    Some("runtime_refused") => Signal::Drop(DropReason::RuntimeRefused),
                    Some("turn_interrupted") => Signal::Drop(DropReason::TurnInterrupted),
                    Some("runtime_exited") => Signal::Drop(DropReason::RuntimeExited),
                    _ => continue,
                },
                _ => continue,
            };
            apply(e["inputId"].as_str(), e["requestId"].as_str(), sig);
        }
        return;
    }
    let request = r.response_to.as_deref();
    for event in &r.events {
        match event {
            Event::NativeInput { input_id, receipt } => {
                let sig = match receipt {
                    InputReceipt::Queued => Signal::Turn(TurnPhase::Queued),
                    InputReceipt::Started => Signal::Turn(TurnPhase::Active),
                    InputReceipt::Handled => Signal::Handled,
                    InputReceipt::Cancelled => Signal::Drop(DropReason::TurnInterrupted),
                    // A direct Pi admission refusal is followed by a rejected control response.
                    // An already accepted attempt has a separate, explicit InputDropped event.
                    InputReceipt::Refused => continue,
                    InputReceipt::Duplicate => Signal::Drop(DropReason::RuntimeRefused),
                };
                apply(Some(&input_id.to_string()), request, sig);
            }
            Event::InputQueued { input_id } if body(r)["queued"] != "adapter" => {
                apply(Some(input_id), request, Signal::Turn(TurnPhase::Queued))
            }
            Event::InputDropped { input_id } => {
                if r.direction == Direction::Host {
                    apply(
                        Some(input_id),
                        request,
                        Signal::Drop(DropReason::DeliveryUnconfirmed),
                    );
                    continue;
                }
                // NativeInput::Cancelled and InputDropped can describe the same disposition.
                let interrupted = r.input_drop_reason(input_id) == Some("turn_interrupted")
                    || r.events.iter().any(|e| match e {
                        Event::SteeringInput {
                            input_id: id,
                            status: SteerStatus::Cancelled,
                            ..
                        }
                        | Event::NativeInput {
                            input_id: id,
                            receipt: InputReceipt::Cancelled,
                        } => id.to_string() == *input_id,
                        _ => false,
                    });
                apply(
                    Some(input_id),
                    request,
                    Signal::Drop(if interrupted {
                        DropReason::TurnInterrupted
                    } else {
                        DropReason::RuntimeRefused
                    }),
                );
            }
            Event::SteeringInput {
                input_id,
                status: SteerStatus::Handled,
                ..
            } => apply(Some(&input_id.to_string()), request, Signal::Handled),
            Event::TurnStarted if request.is_some() => {
                apply(None, request, Signal::Turn(TurnPhase::Active))
            }
            _ => {}
        }
    }
}
