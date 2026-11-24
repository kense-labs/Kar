use super::*;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAttemptKind {
    Prompt,
    Steer,
    Queue,
    Withdraw,
}
impl InputAttemptKind {
    pub(crate) fn of(record: &Record) -> Option<Self> {
        if record.kind != RecordKind::Request || record.direction != Direction::ToRuntime {
            return None;
        }
        if record.prompt.is_some() {
            return Some(Self::Prompt);
        }
        // Native Pi writes also use type=steer. Only the host control envelope owns
        // a delivery attempt; a native request following that control must not add another.
        if record.upstream.is_none()
            && submitted_id(record).is_none()
            && !(body(record)["input"].is_string() || body(record)["text"].is_string())
        {
            return None;
        }
        Some(match control(record)? {
            "prompt" => Self::Prompt,
            "steer" => Self::Steer,
            "queue" => Self::Queue,
            "withdraw" => Self::Withdraw,
            _ => return None,
        })
    }
}

pub(crate) fn submitted_id(record: &Record) -> Option<&str> {
    record
        .prompt
        .as_ref()
        .map(|p| p.input_id.as_str())
        .or_else(|| body(record)["inputId"].as_str())
}

/// Adapter dispatches reuse their held UUID. Imported aggregate prompts use a separate identity.
/// A caller cannot take partial ownership by including both valid and invalid claims.
pub(crate) fn valid_carries(record: &Record, held: impl Fn(&str) -> bool) -> bool {
    InputAttemptKind::of(record) == Some(InputAttemptKind::Prompt)
        && !record.carries.is_empty()
        && record
            .carries
            .iter()
            .all(|id| (record.upstream.is_none() || submitted_id(record) != Some(id)) && held(id))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Serialize)]
pub struct InputAttempt {
    pub kind: InputAttemptKind,
    pub request: Arc<Record>,
    pub state: AttemptState,
    pub response: Option<Arc<Record>>,
    pub reason: Option<String>,
    /// The native handled receipt names this exact request. It can arrive after a retry.
    pub handled: Option<Cursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Pending,
    Accepted,
    Rejected,
    Withdrawn,
    Dropped,
    Untracked,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// Kar released a locally held input without receiving a native delivery decision.
    DeliveryUnconfirmed,
    RuntimeExited,
    RuntimeRefused,
    TurnInterrupted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Queued,
    Active,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InputTurn {
    pub phase: TurnPhase,
    pub cursor: Cursor,
}

impl ConversationInput {
    pub(super) fn claimable(&self) -> bool {
        if self.state == DeliveryState::Withdrawn {
            return self.attempts.last().is_some_and(|a| {
                a.kind == InputAttemptKind::Withdraw && a.state == AttemptState::Accepted
            });
        }
        matches!(
            self.state,
            DeliveryState::Accepted | DeliveryState::Pending | DeliveryState::Rejected
        ) && self
            .attempts
            .iter()
            .rev()
            .find(|a| a.state == AttemptState::Accepted)
            .is_some_and(|a| {
                a.kind == InputAttemptKind::Queue
                    && a.response
                        .as_ref()
                        .is_some_and(|r| body(r)["queued"] == "adapter")
            })
    }
    pub(super) fn refresh(&mut self) {
        let withdrawn = self
            .attempts
            .iter()
            .rposition(|a| {
                a.kind == InputAttemptKind::Withdraw && a.state == AttemptState::Accepted
            })
            .map_or(0, |i| i + 1);
        let dropped = self.drop.map_or(0, |(count, _)| count);
        let mut delivery = self.attempts[withdrawn.max(dropped)..]
            .iter()
            .filter(|a| a.kind != InputAttemptKind::Withdraw);
        self.reason = None;
        self.state = if delivery.clone().any(|a| a.state == AttemptState::Accepted) {
            DeliveryState::Accepted
        } else if let Some(last) = delivery.next_back() {
            match last.state {
                AttemptState::Rejected => DeliveryState::Rejected,
                _ => DeliveryState::Pending,
            }
        } else if dropped > withdrawn {
            self.reason = self.drop.map(|(_, why)| why);
            DeliveryState::Dropped
        } else if withdrawn > 0 {
            DeliveryState::Withdrawn
        } else {
            DeliveryState::Pending
        };
    }

    pub(super) fn awaits_echo(&self, witnessed: bool) -> bool {
        if self.handled.is_some()
            || matches!(
                self.state,
                DeliveryState::Dropped | DeliveryState::Withdrawn | DeliveryState::Rejected
            )
        {
            return false;
        }
        if let Some(turn) = &self.turn {
            return turn.phase == TurnPhase::Queued;
        }
        self.observations.is_empty()
            && witnessed
            && !self
                .attempts
                .iter()
                .any(|a| a.kind == InputAttemptKind::Prompt && a.state != AttemptState::Rejected)
    }
}

pub(super) fn content(record: &Record) -> (String, Vec<crate::ImageReference>) {
    if let Some(submission) = record.prompt.as_ref().and_then(|p| p.submission.as_ref()) {
        return (submission.input.clone(), submission.images.clone());
    }
    let packet = body(record);
    let text = ["input", "text", "task"]
        .iter()
        .find_map(|key| packet[key].as_str())
        .map(str::to_owned)
        .or_else(|| super::super::sent_prompt_text(packet))
        .or_else(|| packet.pointer("/params/text")?.as_str().map(str::to_owned))
        .or_else(|| super::super::input_text(&packet["content"]))
        .or_else(|| {
            record.events.iter().find_map(|e| {
                if let Event::UserMessage { text } = e {
                    Some(text.clone())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    let images = serde_json::from_value(packet["images"].clone()).unwrap_or_default();
    (text, images)
}
