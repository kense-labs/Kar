//! Associate stream-json argument fragments with their content block. Only unfinished
//! blocks are retained, separately for each native session and parent tool.
use crate::Event;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct ArgumentStreams {
    lanes: HashMap<(String, String), HashMap<u64, ArgumentBlock>>,
}
#[derive(Default)]
struct ArgumentBlock {
    id: String,
    // A suffix that could still become one of the configured secrets on the next packet.
    pending: String,
    key: Option<String>,
    escaped: bool,
    sensitive_key: bool,
    hidden: bool,
}
impl ArgumentBlock {
    fn display(&mut self, fragment: &str, secrets: &[String]) -> String {
        if self.hidden {
            return String::new();
        }
        let mut prefix = fragment;
        for (index, ch) in fragment.char_indices() {
            if let Some(key) = &mut self.key {
                // Keep only enough to recognize a credential field, including escaped JSON keys.
                if key.len() < 96 {
                    key.push(ch);
                }
                if self.escaped {
                    self.escaped = false;
                } else if ch == '\\' {
                    self.escaped = true;
                } else if ch == '"' {
                    self.sensitive_key = serde_json::from_str::<String>(key)
                        .is_ok_and(|s| crate::interaction::sensitive_field(&s));
                    self.key = None;
                }
            } else if ch == '"' {
                self.key = Some(String::from("\""));
                self.sensitive_key = false;
            } else if !ch.is_whitespace() {
                if self.sensitive_key && ch == ':' {
                    // Wait for the sanitized structured snapshot before displaying this value.
                    prefix = &fragment[..index + 1];
                    self.hidden = true;
                    break;
                }
                self.sensitive_key = false;
            }
        }
        self.pending.push_str(prefix);
        masked(&mut self.pending, secrets, self.hidden)
    }
}

impl ArgumentStreams {
    // Rewrite the recorded fragment too, so raw subscriptions and durable projections agree.
    pub(crate) fn observe(&mut self, v: &mut Value, secrets: &[String]) -> Option<Event> {
        if !matches!(
            v["type"].as_str(),
            Some("stream_event" | "result" | "assistant")
        ) {
            return None;
        }
        let lane = (
            v["session_id"].as_str().unwrap_or_default().into(),
            v["parent_tool_use_id"].as_str().unwrap_or_default().into(),
        );
        let event = &v["event"];
        if v["type"] == "result"
            || matches!(
                event["type"].as_str(),
                Some("message_start" | "message_stop")
            )
        {
            self.lanes.remove(&lane);
            return None;
        }
        if v["type"] == "assistant" {
            if let Some(blocks) = self.lanes.get_mut(&lane) {
                for tool in v["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "tool_use" && b.get("input").is_some())
                {
                    if let Some(id) = tool["id"].as_str() {
                        blocks.retain(|_, b| b.id != id);
                    }
                }
            }
            return None;
        }
        let index = event["index"].as_u64();
        match event["type"].as_str()? {
            "content_block_start" => {
                let index = index?;
                let block = &event["content_block"];
                if block["type"] == "tool_use"
                    && block["name"].is_string()
                    && let Some(id) = block["id"].as_str().filter(|id| !id.is_empty())
                {
                    let blocks = self.lanes.entry(lane).or_default();
                    if blocks.get(&index).is_none_or(|b| b.id != id) {
                        blocks.insert(
                            index,
                            ArgumentBlock {
                                id: id.into(),
                                ..Default::default()
                            },
                        );
                    }
                } else if let Some(blocks) = self.lanes.get_mut(&lane) {
                    blocks.remove(&index);
                }
            }
            "content_block_delta" if event["delta"]["type"] == "input_json_delta" => {
                let fragment = event["delta"]["partial_json"].as_str()?;
                let block = index.and_then(|i| self.lanes.get_mut(&lane)?.get_mut(&i));
                let Some(block) = block else {
                    v["event"]["delta"]["partial_json"] = Value::Null;
                    return None;
                };
                let delta = block.display(fragment, secrets);
                v["event"]["delta"]["partial_json"] = Value::String(delta.clone());
                if !delta.is_empty() {
                    return Some(Event::ToolInputDelta {
                        id: block.id.clone(),
                        delta,
                    });
                }
            }
            "content_block_stop" => {
                let mut block = self.lanes.get_mut(&lane)?.remove(&index?)?;
                let delta = masked(&mut block.pending, secrets, true);
                if !delta.is_empty() {
                    return Some(Event::ToolInputDelta {
                        id: block.id,
                        delta,
                    });
                }
            }
            _ => {}
        }
        None
    }
}

// Compare decoded JSON characters so a Unicode escape or a split escape cannot expose
// a configured value. Offset corrections keep all unaffected display bytes verbatim.
struct Decoded<'a> {
    text: std::borrow::Cow<'a, str>,
    offsets: Vec<(usize, usize)>,
}
impl<'a> Decoded<'a> {
    fn new(raw: &'a str, end: bool) -> Self {
        let Some(mut pos) = raw.find('\\') else {
            return Self {
                text: raw.into(),
                offsets: vec![],
            };
        };
        let mut text = raw[..pos].to_owned();
        let mut offsets = Vec::new();
        while pos < raw.len() {
            let rest = &raw[pos..];
            if !rest.starts_with('\\') {
                let len = rest.find('\\').unwrap_or(rest.len());
                text.push_str(&rest[..len]);
                pos += len;
                continue;
            }
            let mut len = if rest.starts_with("\\u") { 6 } else { 2 };
            if len == 6
                && rest
                    .get(2..6)
                    .and_then(|v| u16::from_str_radix(v, 16).ok())
                    .is_some_and(|unit| (0xd800..=0xdbff).contains(&unit))
            {
                len = 12;
            }
            if rest.len() < len && !end {
                break;
            }
            if let Some(encoded) = rest.get(..len)
                && let Ok(value) = serde_json::from_str::<String>(&format!("\"{encoded}\""))
            {
                text.push_str(&value);
                pos += len;
                offsets.push((text.len(), pos - text.len()));
            } else {
                text.push('\\');
                pos += 1;
            }
        }
        Self {
            text: text.into(),
            offsets,
        }
    }
    fn raw_index(&self, i: usize) -> usize {
        let n = self.offsets.partition_point(|(at, _)| *at <= i);
        i + n.checked_sub(1).map_or(0, |i| self.offsets[i].1)
    }
}
fn masked(pending: &mut String, secrets: &[String], end: bool) -> String {
    let raw = std::mem::take(pending);
    if secrets.is_empty() || raw.is_empty() {
        return raw;
    }
    let decoded = Decoded::new(&raw, end);
    let mut result = String::new();
    let mut i = 0;
    let mut copy = 0;
    while i < decoded.text.len() {
        let rest = &decoded.text[i..];
        if !end
            && secrets
                .iter()
                .any(|s| s.len() > rest.len() && s.starts_with(rest))
        {
            break;
        }
        if let Some(secret) = secrets
            .iter()
            .filter(|s| !s.is_empty() && rest.starts_with(s.as_str()))
            .max_by_key(|s| s.len())
        {
            result.push_str(&raw[copy..decoded.raw_index(i)]);
            result.push_str("[redacted]");
            i += secret.len();
            copy = decoded.raw_index(i);
        } else {
            i += rest.chars().next().unwrap().len_utf8();
        }
    }
    let boundary = decoded.raw_index(i);
    result.push_str(&raw[copy..boundary]);
    pending.push_str(&raw[boundary..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_secrets_across_every_split_and_preserves_other_encoded_bytes() {
        let secrets = ["shortkey".to_owned(), "shortkey🔐tail".to_owned()];
        let encoded = r#"{"note":"line\nshort\u006bey\ud83d\udd10tail 🦀"}"#;
        let expected = r#"{"note":"line\n[redacted] 🦀"}"#;
        for split in encoded.char_indices().map(|(i, _)| i) {
            let mut block = ArgumentBlock::default();
            let mut text = block.display(&encoded[..split], &secrets);
            text.push_str(&block.display(&encoded[split..], &secrets));
            text.push_str(&masked(&mut block.pending, &secrets, true));
            assert_eq!(text, expected, "split {split}");
        }
        let mut block = ArgumentBlock::default();
        let chunks = [
            r#"{"visible":"x","pass"#,
            r#"\u0077ord":"private","more":"later"}"#,
        ];
        let safe: String = chunks.iter().map(|s| block.display(s, &secrets)).collect();
        assert_eq!(safe, r#"{"visible":"x","pass\u0077ord":"#);
    }
    #[test]
    fn parallel_blocks_and_parent_tools_have_separate_argument_buffers() {
        let mut streams = ArgumentStreams::default();
        let mut send = |parent: Option<&str>, event: Value| {
            let mut v = json!({"session_id":"s","type":"stream_event","parent_tool_use_id":parent,"event":event});
            streams.observe(&mut v, &[])
        };
        let start = |index, id| json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":"Edit"}});
        let delta = |index| json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":"part"}});
        send(None, start(1, "one"));
        send(None, start(2, "two"));
        send(Some("parent"), start(1, "child"));
        for (parent, index, expected) in [
            (None, 2, "two"),
            (Some("parent"), 1, "child"),
            (None, 1, "one"),
        ] {
            assert!(
                matches!(send(parent,delta(index)),Some(Event::ToolInputDelta{id,delta}) if id == expected && delta == "part")
            );
        }
        send(None, json!({"type":"content_block_stop","index":1}));
        assert!(send(None, delta(1)).is_none());
        send(None, json!({"type":"message_start"}));
        assert!(send(None, delta(2)).is_none());
        assert!(send(Some("parent"), delta(1)).is_some());
    }
}
