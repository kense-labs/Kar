use super::{Direction, Event, EventCursor, ObservedEvent, Record};
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
        let message_id = event["messageId"]
            .as_str()
            .or_else(|| event["message_id"].as_str())
            .or_else(|| event["msg_id"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let phase = event["phase"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        (message_id.is_some() || phase.is_some()).then_some(Self { message_id, phase })
    }
}

fn nonempty(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

fn first_str<'a>(candidates: &[&'a Value]) -> Option<&'a str> {
    candidates.iter().find_map(|v| nonempty(v))
}

#[derive(Default)]
pub(super) struct MessageTracker {
    // Codex reports the native phase on the item lifecycle, while deltas only name the item.
    codex: HashMap<(String, Option<String>), HashMap<String, String>>,
    claude: HashMap<(String, Vec<String>), MessageBoundary>,
    // DSH streaming frames share an attemptId from `start` through `chunk`s.
    dsh: HashMap<(String, Vec<String>), HashMap<String, String>>,
    // Kimi KAP reports item metadata on `item.started`, while `item.delta` carries `item_id`.
    kimi: HashMap<(String, Vec<String>), HashMap<String, Option<String>>>,
    // Factory `droid.session_notification` events correlate deltas and `create_message` by `messageId`.
    factory: HashMap<(String, Vec<String>), HashMap<String, String>>,
    // Morph Console snapshots track rewritten message boundaries per `(session, task_id)`.
    morph: HashMap<(String, String), usize>,
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
        let session_key = session.unwrap_or_default().to_owned();
        let lane_key = (session_key.clone(), path.to_vec());
        let p = &v["params"];
        let mut context = MessageBoundary::default();

        // 1. Codex (`item/*` & `turn/completed`)
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
                        if let Some(id) = nonempty(&p["item"]["id"]) {
                            context.message_id = Some(id.into());
                            context.phase = nonempty(&p["item"]["phase"]).map(str::to_owned);
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
                        if let Some(id) = nonempty(&p["itemId"]) {
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

        // 2. Claude (`stream_event` / `assistant`)
        if v["type"] == "stream_event" {
            let e = &v["event"];
            match e["type"].as_str() {
                Some("message_start") => {
                    self.claude.remove(&lane_key);
                    let boundary = MessageBoundary {
                        message_id: nonempty(&e["message"]["id"]).map(str::to_owned),
                        phase: nonempty(&e["message"]["phase"]).map(str::to_owned),
                    };
                    if boundary != MessageBoundary::default() {
                        self.claude.insert(lane_key.clone(), boundary);
                    }
                }
                Some("message_stop") => {
                    self.claude.remove(&lane_key);
                }
                Some("content_block_delta") => {
                    if let Some(saved) = self.claude.get(&lane_key) {
                        context = saved.clone();
                    }
                }
                _ => {}
            }
        } else if v["type"] == "assistant" && v["message"].is_object() {
            context.message_id = nonempty(&v["message"]["id"]).map(str::to_owned);
            context.phase = nonempty(&v["message"]["phase"]).map(str::to_owned);
        }

        // 3. Factory (`droid.session_notification` & unwrapped Factory stream-jsonrpc notifications)
        let factory_notice = if v["method"] == "droid.session_notification" {
            Some(&p["notification"])
        } else if matches!(
            v["type"].as_str(),
            Some(
                "assistant_text_delta"
                    | "thinking_text_delta"
                    | "create_message"
                    | "agent_turn_completed"
            )
        ) {
            Some(v)
        } else {
            None
        };
        if let Some(n) = factory_notice {
            match n["type"].as_str() {
                Some("agent_turn_completed") => {
                    self.factory.remove(&lane_key);
                }
                Some("assistant_text_delta" | "thinking_text_delta") => {
                    let id = first_str(&[&n["messageId"], &n["message_id"], &n["id"]]);
                    let phase = nonempty(&n["phase"]).map(str::to_owned);
                    if let Some(id) = id {
                        context.message_id = Some(id.to_owned());
                        if let Some(ph) = &phase {
                            self.factory
                                .entry(lane_key.clone())
                                .or_default()
                                .insert(id.to_owned(), ph.clone());
                        }
                        context.phase = phase.or_else(|| {
                            self.factory.get(&lane_key).and_then(|m| m.get(id)).cloned()
                        });
                    } else {
                        context.phase = phase;
                    }
                }
                Some("create_message") if n["message"]["role"] == "assistant" => {
                    let msg = &n["message"];
                    let id = first_str(&[
                        &msg["id"],
                        &msg["messageId"],
                        &msg["message_id"],
                        &n["messageId"],
                        &n["id"],
                    ]);
                    let phase = first_str(&[&msg["phase"], &n["phase"]]).map(str::to_owned);
                    if let Some(id) = id {
                        context.message_id = Some(id.to_owned());
                        if let Some(ph) = &phase {
                            self.factory
                                .entry(lane_key.clone())
                                .or_default()
                                .insert(id.to_owned(), ph.clone());
                        }
                        context.phase = phase.or_else(|| {
                            self.factory.get(&lane_key).and_then(|m| m.get(id)).cloned()
                        });
                    } else {
                        context.phase = phase;
                    }
                }
                _ => {}
            }
        }

        // 4. Kimi KAP (`item.started`, `item.delta`, `item.updated`, `item.ended`, `turn.ended`)
        if let Some(kind) = v["type"].as_str() {
            match kind {
                "turn.ended" => {
                    self.kimi.remove(&lane_key);
                }
                "item.started" | "item.ended" | "item.updated" | "item.delta" => {
                    let item = &v["item"];
                    let id = first_str(&[
                        &v["item_id"],
                        &item["item_id"],
                        &v["message_id"],
                        &item["id"],
                        &v["id"],
                    ]);
                    let direct_phase = first_str(&[&v["phase"], &item["phase"]]).map(str::to_owned);
                    if let Some(id) = id {
                        let stored = self
                            .kimi
                            .entry(lane_key.clone())
                            .or_default()
                            .entry(id.to_owned())
                            .or_insert_with(|| direct_phase.clone());
                        if direct_phase.is_some() {
                            *stored = direct_phase.clone();
                        }
                        context.message_id = Some(id.to_owned());
                        context.phase = direct_phase.or_else(|| stored.clone());
                    } else if direct_phase.is_some() {
                        context.phase = direct_phase;
                    }
                }
                _ => {}
            }
        }

        // 5. DSH (`assistant-stream`, `agent/assistant-stream`, `event` -> `assistant/message`, `dsh/*`)
        if context == MessageBoundary::default() {
            if v["type"] == "assistant-stream"
                || v["channel"] == "agent/assistant-stream"
                || v["type"].as_str().is_some_and(|t| t.starts_with("dsh/"))
            {
                let f = if v["frame"].is_object() {
                    &v["frame"]
                } else {
                    v
                };
                let id = first_str(&[
                    &v["itemId"],
                    &f["message_id"],
                    &f["msg_id"],
                    &f["attemptId"],
                    &f["id"],
                    &v["message_id"],
                    &v["msg_id"],
                ]);
                let direct_phase =
                    first_str(&[&f["phase"], &f["chunk"]["phase"], &v["phase"]]).map(str::to_owned);
                if let Some(attempt) = nonempty(&f["attemptId"])
                    && let Some(ph) = &direct_phase
                {
                    self.dsh
                        .entry(lane_key.clone())
                        .or_default()
                        .insert(attempt.to_owned(), ph.clone());
                }
                let saved_phase = nonempty(&f["attemptId"]).and_then(|attempt| {
                    self.dsh
                        .get(&lane_key)
                        .and_then(|m| m.get(attempt))
                        .cloned()
                });
                context.message_id = id.map(str::to_owned);
                context.phase = direct_phase.or(saved_phase);
            } else if (v["type"] == "event" && v["event"]["type"].is_string())
                || matches!(
                    v["type"].as_str(),
                    Some("assistant/message" | "assistant/attempt" | "turn/end")
                )
            {
                let e = if v["event"].is_object() {
                    &v["event"]
                } else {
                    v
                };
                match e["type"].as_str() {
                    Some("turn/end") => {
                        self.dsh.remove(&lane_key);
                    }
                    Some("assistant/message" | "assistant/attempt") => {
                        let d = &e["data"];
                        context.message_id = first_str(&[
                            &v["itemId"],
                            &d["message_id"],
                            &d["msg_id"],
                            &d["message"]["id"],
                            &d["id"],
                            &e["message_id"],
                            &e["msg_id"],
                            &e["id"],
                        ])
                        .map(str::to_owned);
                        context.phase =
                            first_str(&[&d["phase"], &d["message"]["phase"], &e["phase"]])
                                .map(str::to_owned);
                    }
                    _ => {}
                }
            }
        }

        // 6. Cybion (`kind: "response_output"` history rows and Cybion thread stream events)
        if context == MessageBoundary::default() {
            if v["kind"] == "response_output" && v["payload"].is_object() {
                let payload = &v["payload"];
                context.message_id = first_str(&[
                    &payload["message_id"],
                    &payload["id"],
                    &v["message_id"],
                    &v["id"],
                ])
                .map(str::to_owned)
                .or_else(|| v["id"].as_u64().map(|n| format!("cybion-record-{n}")));
                context.phase = first_str(&[&payload["phase"], &v["phase"]]).map(str::to_owned);
            } else if v.get("thread_id").is_some()
                || v["type"].as_str().is_some_and(|t| {
                    t.starts_with("cybion") || t.starts_with("thread/") || t.starts_with("history/")
                })
            {
                context.message_id = first_str(&[
                    &v["message_id"],
                    &v["messageId"],
                    &v["msg_id"],
                    &v["payload"]["id"],
                    &v["item_id"],
                    &v["id"],
                ])
                .map(str::to_owned);
                context.phase =
                    first_str(&[&v["phase"], &v["payload"]["phase"]]).map(str::to_owned);
            }
        }

        // 7. Morph Console snapshots & SSE events
        if context == MessageBoundary::default() {
            let is_morph_snapshot = v.get("task_id").is_some()
                || (v.get("status").is_some()
                    && (v["trace"].is_object() || v["result"]["trace"].is_object()));
            if is_morph_snapshot {
                let explicit_id = first_str(&[&v["message_id"], &v["messageId"], &v["msg_id"]]);
                let phase = nonempty(&v["phase"]).map(str::to_owned);
                if let Some(id) = explicit_id {
                    context.message_id = Some(id.to_owned());
                    context.phase = phase;
                } else if let Some(task_id) = first_str(&[&v["task_id"], &v["id"]]) {
                    let morph_key = (session_key.clone(), task_id.to_owned());
                    let has_rewrite = events.iter().any(|e| matches!(e, Event::Message { .. }))
                        && events.iter().any(|e| matches!(e, Event::TextDelta { .. }));
                    for (i, event) in events.iter().enumerate() {
                        match event {
                            Event::Message { .. } => {
                                let idx = *self.morph.entry(morph_key.clone()).or_insert(0);
                                out.insert(
                                    i,
                                    MessageBoundary {
                                        message_id: Some(format!("{task_id}:{idx}")),
                                        phase: phase.clone(),
                                    },
                                );
                                if has_rewrite {
                                    *self.morph.entry(morph_key.clone()).or_insert(0) += 1;
                                }
                            }
                            Event::TextDelta { .. } | Event::ReasoningDelta { .. } => {
                                let idx = *self.morph.entry(morph_key.clone()).or_insert(0);
                                out.insert(
                                    i,
                                    MessageBoundary {
                                        message_id: Some(format!("{task_id}:{idx}")),
                                        phase: phase.clone(),
                                    },
                                );
                            }
                            _ => {}
                        }
                    }
                    if matches!(v["status"].as_str(), Some("done" | "failed" | "canceled")) {
                        self.morph.remove(&morph_key);
                    }
                    return out;
                }
            }
        }

        // 8. Generic SSE / direct delta frames (e.g. Morph SSE, DSH/Cybion raw deltas, ScriptedRuntime)
        if context == MessageBoundary::default()
            && v.get("jsonrpc").is_none()
            && v.get("method").is_none()
            && v["type"] != "stream_event"
        {
            let explicit_msg_id = first_str(&[
                &v["message_id"],
                &v["messageId"],
                &v["msg_id"],
                &v["data"]["message_id"],
                &v["data"]["messageId"],
                &v["data"]["msg_id"],
            ])
            .or_else(|| {
                let has_stream_marker = v.get("event").is_some()
                    || v.get("delta").is_some()
                    || v.get("text").is_some()
                    || v.get("chunk").is_some();
                has_stream_marker
                    .then(|| first_str(&[&v["data"]["id"], &v["id"]]))
                    .flatten()
            });
            let phase = first_str(&[&v["phase"], &v["data"]["phase"]]);
            if explicit_msg_id.is_some() || phase.is_some() {
                context.message_id = explicit_msg_id.map(str::to_owned);
                context.phase = phase.map(str::to_owned);
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

fn delta_text_mut(event: &mut Event) -> Option<&mut String> {
    match event {
        Event::TextDelta { text } | Event::ReasoningDelta { text } => Some(text),
        _ => None,
    }
}

fn same_lane(left: &ObservedEvent, right: &ObservedEvent, merge_by_native_message: bool) -> bool {
    left.session_id == right.session_id
        && left.agent_path == right.agent_path
        && (!merge_by_native_message || left.message == right.message)
        && std::mem::discriminant(&left.event) == std::mem::discriminant(&right.event)
}

/// Pure, synchronous offline event projection and text/reasoning delta coalescing over a slice
/// of [`Record`]s.
///
/// Works without Tokio channels or timers, making it directly usable on replayed recordings,
/// imported voyages, and `wasm32` targets. When `merge_by_native_message` is `true`, consecutive
/// deltas split whenever `message_id` or `phase` changes; when `false`, consecutive deltas of the
/// same `(session_id, agent_path)` and kind merge across native message boundaries until a
/// non-delta event arrives. Each merged [`ObservedEvent`] retains the last merged event's cursor.
pub fn coalesce_events(records: &[Record], merge_by_native_message: bool) -> Vec<ObservedEvent> {
    let mut tracker = MessageTracker::default();
    let mut out = Vec::new();
    let mut held: Option<ObservedEvent> = None;

    for r in records {
        let inferred = tracker.observe(
            r.direction,
            r.session_id.as_deref(),
            &r.agent_path,
            &r.native,
            &r.events,
        );
        for (index, event) in r.events.iter().enumerate() {
            let message = r
                .message_boundaries
                .get(&index)
                .or_else(|| inferred.get(&index))
                .cloned()
                .unwrap_or_default();
            let mut next = ObservedEvent {
                cursor: EventCursor {
                    stream_id: r.stream_id,
                    seq: r.seq,
                    index,
                },
                timestamp_ms: r.timestamp_ms,
                session_id: r.session_id.clone(),
                agent_path: r.agent_path.clone(),
                span_id: r.span_id.clone(),
                elided: r.elided,
                message,
                event: event.clone(),
            };
            if delta_text_mut(&mut next.event).is_some() {
                if let Some(mut previous) = held.take() {
                    if same_lane(&previous, &next, merge_by_native_message) {
                        let suffix = delta_text_mut(&mut next.event).expect("delta text");
                        let mut merged =
                            std::mem::take(delta_text_mut(&mut previous.event).expect("held text"));
                        merged.push_str(suffix);
                        *suffix = merged;
                        held = Some(next);
                    } else {
                        out.push(previous);
                        held = Some(next);
                    }
                } else {
                    held = Some(next);
                }
            } else {
                if let Some(previous) = held.take() {
                    out.push(previous);
                }
                out.push(next);
            }
        }
    }
    if let Some(previous) = held.take() {
        out.push(previous);
    }
    out
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
                if same_lane(&previous, &next, true) {
                    let text = delta_text_mut(&mut next.event).expect("held event has text");
                    let mut merged =
                        std::mem::take(delta_text_mut(&mut previous.event).expect("held text"));
                    merged.push_str(text);
                    *text = merged;
                } else {
                    self.queued = Some(next);
                    return Some(previous);
                }
            } else if delta_text_mut(&mut next.event).is_none() {
                return Some(next);
            }
            self.held = Some(next);
            self.deadline = self
                .max_hold
                .and_then(|d| tokio::time::Instant::now().checked_add(d));
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::stream::{Journal, RecordKind};
    use serde_json::json;

    #[test]
    fn tracker_extracts_boundaries_across_all_seven_runtimes() {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("s");
        let push = |native: Value, events: Vec<Event>| {
            journal.push(
                Direction::FromRuntime,
                RecordKind::Frame,
                Some("s".into()),
                vec![],
                native,
                events,
            );
        };

        // 1. Codex
        push(
            json!({"method":"item/started","params":{"threadId":"s","turnId":"t1","item":{"type":"agentMessage","id":"codex-m1","phase":"commentary"}}}),
            vec![],
        );
        push(
            json!({"method":"item/agentMessage/delta","params":{"threadId":"s","turnId":"t1","itemId":"codex-m1"}}),
            vec![Event::TextDelta { text: "cx".into() }],
        );

        // 2. Claude
        push(
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"claude-m1","phase":"final_answer"}}}),
            vec![],
        );
        push(
            json!({"type":"stream_event","event":{"type":"content_block_delta"}}),
            vec![Event::TextDelta { text: "cl".into() }],
        );
        push(
            json!({"type":"stream_event","event":{"type":"message_stop"}}),
            vec![],
        );

        // 3. DSH
        push(
            json!({"type":"assistant-stream","itemId":"dsh/1/0/0","frame":{"type":"chunk","attemptId":"a1","phase":"commentary","chunk":{"type":"text-delta","text":"ds1"}}}),
            vec![Event::TextDelta { text: "ds1".into() }],
        );
        push(
            json!({"type":"dsh/delta","msg_id":"dsh-msg-2","phase":"final_answer"}),
            vec![Event::TextDelta { text: "ds2".into() }],
        );

        // 4. Kimi KAP
        push(
            json!({"type":"item.started","turn_id":"kt","item":{"item_id":"kimi-m1","type":"assistant_message","phase":"commentary"}}),
            vec![Event::TextDelta { text: "ki1".into() }],
        );
        push(
            json!({"type":"item.delta","turn_id":"kt","item_id":"kimi-m1","field":"text","delta":"ki2"}),
            vec![Event::TextDelta { text: "ki2".into() }],
        );

        // 5. Factory
        push(
            json!({"jsonrpc":"2.0","type":"notification","method":"droid.session_notification","params":{"notification":{"type":"assistant_text_delta","messageId":"fac-m1","phase":"final_answer","textDelta":"fa"}}}),
            vec![Event::TextDelta { text: "fa".into() }],
        );

        // 6. Morph (snapshot rewrite + SSE)
        push(
            json!({"task_id":"morph-t1","status":"running","trace":{"entries":[]},"text":"m1"}),
            vec![Event::TextDelta { text: "m1".into() }],
        );
        push(
            json!({"task_id":"morph-t1","status":"running","trace":{"entries":[]},"text":"rewritten"}),
            vec![
                Event::Message { text: "m1".into() },
                Event::TextDelta {
                    text: "rewritten".into(),
                },
            ],
        );
        push(
            json!({"event":"message_delta","message_id":"morph-sse-1","phase":"final_answer"}),
            vec![Event::TextDelta {
                text: "msse".into(),
            }],
        );

        // 7. Cybion
        push(
            json!({"id":42,"kind":"response_output","payload":{"id":"cyb-m1","type":"message","phase":"final_answer"}}),
            vec![Event::TextDelta { text: "cy1".into() }],
        );
        push(
            json!({"id":43,"kind":"response_output","payload":{"type":"message"}}),
            vec![Event::TextDelta { text: "cy2".into() }],
        );

        let records: Vec<Record> = journal
            .records()
            .into_iter()
            .map(|r| (*r).clone())
            .collect();

        let by_msg = coalesce_events(&records, true);
        let summaries: Vec<(Option<&str>, Option<&str>, &Event)> = by_msg
            .iter()
            .map(|e| {
                (
                    e.message.message_id.as_deref(),
                    e.message.phase.as_deref(),
                    &e.event,
                )
            })
            .collect();
        assert_eq!(
            summaries,
            vec![
                (
                    Some("codex-m1"),
                    Some("commentary"),
                    &Event::TextDelta { text: "cx".into() }
                ),
                (
                    Some("claude-m1"),
                    Some("final_answer"),
                    &Event::TextDelta { text: "cl".into() }
                ),
                (
                    Some("dsh/1/0/0"),
                    Some("commentary"),
                    &Event::TextDelta { text: "ds1".into() }
                ),
                (
                    Some("dsh-msg-2"),
                    Some("final_answer"),
                    &Event::TextDelta { text: "ds2".into() }
                ),
                (
                    Some("kimi-m1"),
                    Some("commentary"),
                    &Event::TextDelta {
                        text: "ki1ki2".into()
                    }
                ),
                (
                    Some("fac-m1"),
                    Some("final_answer"),
                    &Event::TextDelta { text: "fa".into() }
                ),
                (
                    Some("morph-t1:0"),
                    None,
                    &Event::TextDelta { text: "m1".into() }
                ),
                (
                    Some("morph-t1:0"),
                    None,
                    &Event::Message { text: "m1".into() }
                ),
                (
                    Some("morph-t1:1"),
                    None,
                    &Event::TextDelta {
                        text: "rewritten".into()
                    }
                ),
                (
                    Some("morph-sse-1"),
                    Some("final_answer"),
                    &Event::TextDelta {
                        text: "msse".into()
                    }
                ),
                (
                    Some("cyb-m1"),
                    Some("final_answer"),
                    &Event::TextDelta { text: "cy1".into() }
                ),
                (
                    Some("cybion-record-43"),
                    None,
                    &Event::TextDelta { text: "cy2".into() }
                ),
            ]
        );

        // When merge_by_native_message == false, consecutive TextDeltas merge across message IDs
        // while preserving Event::Message as a non-delta separator!
        let flat = coalesce_events(&records, false);
        assert_eq!(flat.len(), 3);
        assert_eq!(
            flat[0].event,
            Event::TextDelta {
                text: "cxclds1ds2ki1ki2fam1".into()
            }
        );
        assert_eq!(flat[1].event, Event::Message { text: "m1".into() });
        assert_eq!(
            flat[2].event,
            Event::TextDelta {
                text: "rewrittenmssecy1cy2".into()
            }
        );
        assert_eq!(flat[2].cursor.seq, records.last().unwrap().seq);
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
