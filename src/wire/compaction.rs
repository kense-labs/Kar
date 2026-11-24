use crate::{Event, Protocol};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct Compactions {
    // Codex can report the same boundary with both an item and a legacy notification.
    codex: HashMap<String, CodexItem>,
    claude: HashSet<(String, String)>,
}
struct CodexItem {
    turn: String,
    id: String,
    ended: bool,
}
impl Compactions {
    pub(crate) fn observe(&mut self, protocol: Protocol, v: &Value) -> Option<Event> {
        let state = match protocol {
            Protocol::Codex => self.codex(v)?,
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen => self.stream_json(protocol, v)?,
            Protocol::Pi => match v["type"].as_str()? {
                "compaction_start" => "started",
                "compaction_end" if v["aborted"] == true => "aborted",
                "compaction_end" if v["errorMessage"].is_string() => "failed",
                "compaction_end" => "completed",
                _ => return None,
            },
            Protocol::Acp if v["method"] == "session/update" => {
                let marker = v.pointer("/params/update/_meta/opencode~1compaction")?;
                match marker["status"].as_str()? {
                    "started" => "started",
                    "completed" => "completed",
                    "failed"
                        if matches!(
                            marker["error"]["type"].as_str(),
                            Some("aborted" | "compaction.interrupted")
                        ) =>
                    {
                        "aborted"
                    }
                    "failed" => "failed",
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(Event::Compaction {
            state: state.into(),
        })
    }
    fn codex(&mut self, v: &Value) -> Option<&'static str> {
        let p = &v["params"];
        let thread = p["threadId"].as_str()?;
        let turn = p["turnId"].as_str()?;
        let method = v["method"].as_str()?;
        if method == "thread/compacted" {
            let item = self.codex.get_mut(thread)?;
            if item.ended || item.turn != turn {
                return None;
            }
            item.ended = true;
            return Some("completed");
        }
        if p["item"]["type"] != "contextCompaction" {
            return None;
        }
        let ended = match method {
            "item/started" => false,
            "item/completed" => true,
            _ => return None,
        };
        let id = p["item"]["id"].as_str()?;
        if let Some(item) = self.codex.get(thread)
            && item.turn == turn
            && item.id == id
            && (item.ended || !ended)
        {
            return None;
        }
        self.codex.insert(
            thread.into(),
            CodexItem {
                turn: turn.into(),
                id: id.into(),
                ended,
            },
        );
        Some(if ended { "completed" } else { "started" })
    }
    fn stream_json(&mut self, protocol: Protocol, v: &Value) -> Option<&'static str> {
        let kind = v["type"].as_str()?;
        if !matches!(kind, "system" | "result") {
            return None;
        }
        let key = (
            v["session_id"].as_str().unwrap_or_default().into(),
            v["parent_tool_use_id"].as_str().unwrap_or_default().into(),
        );
        if kind == "result" {
            self.claude.remove(&key);
            return None;
        }
        match v["subtype"].as_str()? {
            "compact_boundary" => {
                self.claude.remove(&key);
                Some("completed")
            }
            "status" if protocol == Protocol::Claude && v["compact_result"] == "failed" => {
                self.claude.remove(&key);
                Some("failed")
            }
            "status"
                if matches!(protocol, Protocol::Claude | Protocol::Qoder)
                    && v["status"] == "compacting" =>
            {
                self.claude.insert(key).then_some("started")
            }
            "status" if v.get("status").is_some_and(Value::is_null) => {
                self.claude.remove(&key);
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stream_json_repeated_status_and_child_boundaries_are_independent() {
        let mut c = Compactions::default();
        let mut tick =
            json!({"type":"system","subtype":"status","status":"compacting","session_id":"root"});
        let observe = |c: &mut Compactions, v: &Value| match c.observe(Protocol::Claude, v) {
            Some(Event::Compaction { state }) => Some(state),
            _ => None,
        };
        assert_eq!(observe(&mut c, &tick).as_deref(), Some("started"));
        assert!(observe(&mut c, &tick).is_none());
        tick["parent_tool_use_id"] = "child".into();
        assert_eq!(observe(&mut c, &tick).as_deref(), Some("started"));
        let failed = json!({"type":"system","subtype":"status","session_id":"root","parent_tool_use_id":"child","compact_result":"failed"});
        assert_eq!(observe(&mut c, &failed).as_deref(), Some("failed"));
        tick["parent_tool_use_id"] = Value::Null;
        assert!(observe(&mut c, &tick).is_none());
        let end = json!({"type":"system","session_id":"root","subtype":"compact_boundary","compact_metadata":{"trigger":"auto"}});
        assert_eq!(observe(&mut c, &end).as_deref(), Some("completed"));
        assert_eq!(observe(&mut c, &tick).as_deref(), Some("started"));
    }

    #[test]
    fn native_terminal_flags_and_metadata_survive_projection() {
        use crate::{Direction, Record, RecordKind};
        let mut c = Compactions::default();
        for (protocol, native, state, trigger, reason) in [
            (
                Protocol::Pi,
                json!({"type":"compaction_end","reason":"manual","aborted":true,"errorMessage":"cancelled"}),
                "aborted",
                "manual",
                "cancelled",
            ),
            (
                Protocol::Pi,
                json!({"type":"compaction_end","reason":"overflow","errorMessage":"model unavailable"}),
                "failed",
                "overflow",
                "model unavailable",
            ),
            (
                Protocol::Acp,
                json!({"method":"session/update","params":{"update":{"_meta":{"opencode/compaction":{"messageId":"compact","status":"failed","reason":"manual","error":{"type":"compaction.interrupted","message":"stopped"}}}}}}),
                "aborted",
                "manual",
                "stopped",
            ),
        ] {
            let record = Record {
                prompt: None,
                response_to: None,
                opening: None,
                events: c.observe(protocol, &native).into_iter().collect(),
                native,
                seq: 4,
                timestamp_ms: 17,
                stream_id: uuid::Uuid::nil(),
                direction: Direction::FromRuntime,
                kind: RecordKind::Frame,
                session_id: Some("root".into()),
                agent_path: vec![],
                span_id: None,
                elided: false,
                upstream: None,
            };
            let report = record.compactions().next().unwrap();
            assert_eq!(report.state, state);
            assert_eq!(report.trigger.as_deref(), Some(trigger));
            assert_eq!(report.reason.as_deref(), Some(reason));
            assert_eq!((report.cursor.seq, report.cursor.index), (4, 0));
            if protocol == Protocol::Acp {
                assert_eq!(report.compaction_id.as_deref(), Some("compact"));
            }
        }
    }
}
