//! Qwen can reuse one block index and give each final snapshot its own message
//! ID. Only a stop immediately followed by its snapshot establishes identity.
use crate::Event;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
pub(crate) struct QwenContent {
    lanes: HashMap<(String, String), Message>,
}
#[derive(Default)]
struct Message {
    id: Option<String>,
    blocks: BTreeMap<u64, Block>,
    stopped: Option<u64>,
}
struct Block {
    thinking: bool,
    text: String,
    finalized: bool,
}

impl QwenContent {
    pub(crate) fn observe(&mut self, frame: &Value, events: &mut Vec<Event>) {
        let kind = frame["type"].as_str();
        if !matches!(kind, Some("stream_event" | "assistant" | "result")) {
            return;
        }
        let lane = (
            frame["session_id"].as_str().unwrap_or_default().to_owned(),
            frame["parent_tool_use_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
        let event = &frame["event"];
        if kind == Some("result") || event["type"] == "message_stop" {
            self.lanes.remove(&lane);
        } else if kind == Some("assistant") {
            Message::finish(self.lanes.get_mut(&lane), &frame["message"], events);
        } else if event["type"] == "message_start" {
            self.lanes.insert(
                lane,
                Message {
                    id: event["message"]["id"].as_str().map(str::to_owned),
                    ..Default::default()
                },
            );
        } else {
            self.lanes.entry(lane).or_default().stream(event, events);
        }
    }
}
impl Message {
    fn stream(&mut self, event: &Value, events: &mut Vec<Event>) {
        self.stopped = None;
        let Some(index) = event["index"].as_u64() else {
            return;
        };
        let started = event["type"] == "content_block_start";
        let body = match event["type"].as_str() {
            Some("content_block_stop") => {
                self.stopped = Some(index);
                return;
            }
            Some("content_block_start") => &event["content_block"],
            Some("content_block_delta") => &event["delta"],
            _ => return,
        };
        let thinking = match body["type"].as_str() {
            Some("text" | "text_delta") => false,
            Some("thinking" | "thinking_delta") => true,
            _ => {
                if started {
                    self.blocks.remove(&index);
                }
                return;
            }
        };
        let text = body[if thinking { "thinking" } else { "text" }]
            .as_str()
            .unwrap_or_default();
        let new = || Block {
            thinking,
            text: String::new(),
            finalized: false,
        };
        let block = self.blocks.entry(index).or_insert_with(new);
        if started || block.thinking != thinking || block.finalized {
            *block = new();
        }
        block.text.push_str(text);
        if !text.is_empty() {
            events.push(if thinking {
                Event::ReasoningDelta { text: text.into() }
            } else {
                Event::TextDelta { text: text.into() }
            });
        }
    }
    fn finish(mut partial: Option<&mut Self>, message: &Value, events: &mut Vec<Event>) {
        let Some(content) = message["content"].as_array() else {
            return;
        };
        let stopped = partial.as_mut().and_then(|p| p.stopped.take());
        for block in content {
            let thinking = block["type"] == "thinking";
            if !thinking && block["type"] != "text" {
                continue;
            }
            let Some(full) = block[if thinking { "thinking" } else { "text" }].as_str() else {
                continue;
            };
            let prior = partial.as_mut().and_then(|p| {
                let adjacent = stopped.filter(|index| {
                    content.len() == 1
                        && p.blocks
                            .get(index)
                            .is_some_and(|b| b.thinking == thinking && !b.finalized)
                });
                let index = adjacent.or_else(|| {
                    if p.id.as_deref()? != message["id"].as_str()? {
                        return None;
                    }
                    p.blocks.iter().find_map(|(index, b)| {
                        (b.thinking == thinking && !b.finalized).then_some(*index)
                    })
                })?;
                let b = p.blocks.get_mut(&index)?;
                b.finalized = true;
                Some((b.text.as_str(), adjacent.is_some()))
            });
            if thinking {
                let suffix = match prior {
                    Some((prefix, true)) if !prefix.is_empty() => "",
                    Some((prefix, _)) => full.strip_prefix(prefix).unwrap_or(full),
                    None => full,
                };
                if !suffix.is_empty() {
                    events.push(Event::ReasoningDelta {
                        text: suffix.into(),
                    });
                }
            } else {
                let text = match prior {
                    Some((prefix, true)) if !prefix.is_empty() => prefix,
                    _ => full,
                };
                events.push(Event::Message { text: text.into() });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn qwen_snapshot_identity_comes_from_block_boundaries_even_when_text_repeats() {
        let mut state = QwenContent::default();
        let mut feed = |event: Value| {
            let mut events = Vec::new();
            state.observe(&event, &mut events);
            events
        };
        let stream = |event| json!({"type":"stream_event","event":event});
        let snapshot = |id: &str, kind: &str, text: &str| json!({"type":"assistant","message":{"id":id,"content":[{"type":kind, (kind):text}]}});
        let reason = |text: &str| Event::ReasoningDelta { text: text.into() };
        let message = |text: &str| Event::Message { text: text.into() };
        for _ in 0..2 {
            feed(stream(
                json!({"type":"message_start","message":{"id":"stream"}}),
            ));
            assert_eq!(
                feed(stream(
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"reason"}})
                )),
                [reason("reason")]
            );
            feed(stream(json!({"type":"content_block_stop","index":0})));
            let mut child = snapshot("same", "thinking", "child thought");
            child["parent_tool_use_id"] = "child-tool".into();
            assert_eq!(feed(child), [reason("child thought")]);
            assert!(feed(snapshot("snapshot", "thinking", "different")).is_empty());
            assert_eq!(
                feed(snapshot("following", "thinking", "reason")),
                [reason("reason")]
            );
            for (i, chunk) in ["echo", "echo"].iter().enumerate() {
                assert_eq!(
                    feed(stream(if i == 0 {
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":chunk}})
                    } else {
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":chunk}})
                    })),
                    [Event::TextDelta {
                        text: (*chunk).into()
                    }]
                );
            }
            feed(stream(json!({"type":"content_block_stop","index":0})));
            assert_eq!(
                feed(snapshot("snapshot2", "text", "different")),
                [message("echoecho")]
            );
            feed(stream(json!({"type":"message_stop"})));
            assert_eq!(
                feed(snapshot("following", "text", "echoecho")),
                [message("echoecho")]
            );
            feed(json!({"type":"result","is_error":false}));
        }
    }

    #[test]
    fn qwen_final_only_and_partial_reasoning_keep_the_native_suffix() {
        let mut state = QwenContent::default();
        let mut events = Vec::new();
        for frame in [
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"s"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"first"}}}),
            json!({"type":"assistant","message":{"id":"s","content":[{"type":"thinking","thinking":"first second"}]}}),
            json!({"type":"assistant","message":{"id":"final","content":[{"type":"thinking","thinking":"third"}]}}),
        ] {
            state.observe(&frame, &mut events);
        }
        assert_eq!(
            events,
            ["first", " second", "third"].map(|s| Event::ReasoningDelta { text: s.into() })
        );
    }
}
