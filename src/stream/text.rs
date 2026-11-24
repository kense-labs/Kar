use super::{Direction, Event, ObservedEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBoundary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

impl MessageBoundary {
    pub(crate) fn from_recorded(event: &Value) -> Option<Self> {
        let message_id = event["messageId"].as_str().map(str::to_owned);
        let phase = event["phase"].as_str().map(str::to_owned);
        (message_id.is_some() || phase.is_some()).then_some(Self { message_id, phase })
    }
}

#[derive(Default)]
pub(super) struct MessageTracker {
    // Codex reports the native phase on the item lifecycle, while deltas only name the item.
    codex: HashMap<(String, Option<String>), HashMap<String, String>>,
    claude: HashMap<(String, Vec<String>), String>,
}
impl MessageTracker {
    pub(super) fn observe(
        &mut self,
        direction: Direction,
        session: Option<&str>,
        path: &[String],
        v: &Value,
        events: &[Event],
    ) -> BTreeMap<usize, MessageBoundary> {
        let mut out = BTreeMap::new();
        if direction != Direction::FromRuntime {
            return out;
        }
        let p = &v["params"];
        let mut context = MessageBoundary::default();
        if let (Some(method), Some(thread_id)) =
            (v["method"].as_str(), p["threadId"].as_str().or(session))
        {
            if method == "turn/completed" {
                if let Some(id) = p["turn"]["id"].as_str() {
                    self.codex.remove(&(thread_id.into(), Some(id.to_owned())));
                }
                self.codex.remove(&(thread_id.into(), None));
            } else {
                let key = (
                    thread_id.to_owned(),
                    p["turnId"].as_str().map(str::to_owned),
                );
                match method {
                    "item/started" | "item/completed" if p["item"]["type"] == "agentMessage" => {
                        if let Some(id) = p["item"]["id"].as_str() {
                            context.message_id = Some(id.into());
                            context.phase = p["item"]["phase"].as_str().map(str::to_owned);
                            if let Some(phase) = &context.phase {
                                self.codex
                                    .entry(key)
                                    .or_default()
                                    .insert(id.into(), phase.clone());
                            }
                        }
                    }
                    "item/agentMessage/delta"
                    | "item/reasoning/textDelta"
                    | "item/reasoning/summaryTextDelta" => {
                        if let Some(id) = p["itemId"].as_str() {
                            context.message_id = Some(id.into());
                            if method == "item/agentMessage/delta" {
                                context.phase = self
                                    .codex
                                    .get(&key)
                                    .and_then(|items| items.get(id))
                                    .cloned();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if v["type"] == "stream_event" {
            let e = &v["event"];
            let key = (session.unwrap_or_default().to_owned(), path.to_vec());
            match e["type"].as_str() {
                Some("message_start") => {
                    self.claude.remove(&key);
                    if let Some(id) = e["message"]["id"].as_str() {
                        self.claude.insert(key, id.to_owned());
                    }
                }
                Some("message_stop") => {
                    self.claude.remove(&key);
                }
                Some("content_block_delta") => {
                    context.message_id = self.claude.get(&key).cloned();
                }
                _ => {}
            }
        }
        if context != MessageBoundary::default() {
            for (i, event) in events.iter().enumerate() {
                if matches!(
                    event,
                    Event::TextDelta { .. } | Event::ReasoningDelta { .. } | Event::Message { .. }
                ) {
                    out.insert(i, context.clone());
                }
            }
        }
        out
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct CoalescedEvents {
    source: super::EventSubscription,
    held: Option<ObservedEvent>,
    queued: Option<ObservedEvent>,
    max_hold: Option<std::time::Duration>,
    deadline: Option<tokio::time::Instant>,
}

#[cfg(not(target_arch = "wasm32"))]
impl CoalescedEvents {
    pub(super) fn new(
        source: super::EventSubscription,
        max_hold: Option<std::time::Duration>,
    ) -> Self {
        Self {
            source,
            held: None,
            queued: None,
            max_hold,
            deadline: None,
        }
    }
    fn text(event: &mut Event) -> Option<&mut String> {
        match event {
            Event::TextDelta { text } | Event::ReasoningDelta { text } => Some(text),
            _ => None,
        }
    }
    fn same_lane(left: &ObservedEvent, right: &ObservedEvent) -> bool {
        left.session_id == right.session_id
            && left.agent_path == right.agent_path
            && left.message == right.message
            && std::mem::discriminant(&left.event) == std::mem::discriminant(&right.event)
    }
    /// A timeout, stream closure, or different message flushes the buffered text.
    /// The returned cursor always identifies the last source event it contains.
    pub async fn next(&mut self) -> Option<ObservedEvent> {
        loop {
            let next = if let Some(queued) = self.queued.take() {
                Some(queued)
            } else if let (Some(deadline), Some(_)) = (self.deadline, &self.held) {
                match tokio::time::timeout_at(deadline, self.source.next()).await {
                    Ok(event) => event,
                    Err(_) => return self.held.take(),
                }
            } else {
                self.source.next().await
            };
            let Some(mut next) = next else {
                return self.held.take();
            };
            if let Some(mut previous) = self.held.take() {
                if Self::same_lane(&previous, &next) {
                    let text = Self::text(&mut next.event).expect("held event has text");
                    let mut merged =
                        std::mem::take(Self::text(&mut previous.event).expect("held text"));
                    merged.push_str(text);
                    *text = merged;
                } else {
                    self.queued = Some(next);
                    return Some(previous);
                }
            } else if Self::text(&mut next.event).is_none() {
                return Some(next);
            }
            self.held = Some(next);
            self.deadline = self
                .max_hold
                .and_then(|d| tokio::time::Instant::now().checked_add(d));
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::stream::{Journal, RecordKind};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn merge_by_native_message_and_resume_inside_the_same_turn() {
        let journal = Journal::new(None).unwrap();
        let start = |id: &str, phase: &str| {
            journal.push(Direction::FromRuntime, RecordKind::Frame, Some("s".into()), vec![],
            json!({"method":"item/started","params":{"threadId":"s","turnId":"t","item":{"type":"agentMessage","id":id,"phase":phase}}}), vec![])
        };
        let delta = |id: &str, t: &str| {
            journal.push(Direction::FromRuntime, RecordKind::Frame, Some("s".into()), vec![],
            json!({"method":"item/agentMessage/delta","params":{"threadId":"s","turnId":"t","itemId":id}}), vec![Event::TextDelta{text:t.into()}])
        };
        start("a", "commentary");
        delta("a", "one");
        delta("a", "two");
        start("b", "final_answer");
        delta("b", "three");
        let mut stream = journal
            .events(None)
            .unwrap()
            .coalesce_text(Some(Duration::from_millis(5)));
        let a = stream.next().await.unwrap();
        assert_eq!(a.message.message_id.as_deref(), Some("a"));
        assert_eq!(a.message.phase.as_deref(), Some("commentary"));
        assert_eq!(
            a.event,
            Event::TextDelta {
                text: "onetwo".into()
            }
        );
        let mut resumed = journal.events(Some(&a.cursor)).unwrap().coalesce_text(None);
        let b = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(b.message.phase.as_deref(), Some("final_answer"));
        journal.close();
        assert_eq!(resumed.next().await.unwrap(), b);
        assert!(resumed.next().await.is_none());
    }
}
