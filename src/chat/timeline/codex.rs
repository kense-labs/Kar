//! Index identities once per native thread snapshot, including ambiguous and steered UUIDs.
use super::{ChatTurn, Frame};
use crate::{Direction, Record};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

pub(in crate::chat) struct History<'a> {
    root: &'a str,
    record: &'a Record,
    turns: HashMap<&'a str, Option<&'a Value>>,
    openers: HashMap<Uuid, Option<&'a str>>,
}
impl<'a> History<'a> {
    pub(in crate::chat) fn new(root: &'a str, record: &'a Record) -> Option<Self> {
        let thread = record.native.pointer("/result/thread")?;
        if record.direction != Direction::FromRuntime
            || !record.agent_path.is_empty()
            || record.session_id.as_deref() != Some(root)
            || thread["id"].as_str() != Some(root)
        {
            return None;
        }
        let mut index = Self {
            root,
            record,
            turns: HashMap::new(),
            openers: HashMap::new(),
        };
        for turn in thread["turns"].as_array()? {
            let id = turn["id"].as_str().filter(|s| !s.is_empty());
            if let Some(id) = id {
                index
                    .turns
                    .entry(id)
                    .and_modify(|v| *v = None)
                    .or_insert(Some(turn));
            }
            if record.elided {
                continue;
            }
            for (n, item) in turn["items"].as_array().into_iter().flatten().enumerate() {
                if item["type"] == "userMessage"
                    && let Some(input) = item["clientId"].as_str().and_then(|s| s.parse().ok())
                {
                    index
                        .openers
                        .entry(input)
                        .and_modify(|v| *v = None)
                        .or_insert(if n == 0 { id } else { None });
                }
            }
        }
        Some(index)
    }

    pub(in crate::chat) fn frame(&self, local: &ChatTurn) -> Option<Frame> {
        let opener = self
            .openers
            .get(&local.id)
            .copied()
            .flatten()
            .filter(|_| !local.native_continuation);
        let id = local.native_turn_id.as_deref().or(opener)?;
        let turn = *self.turns.get(id)?;
        Frame::codex_turn(local, self.root, self.record, id, turn?, opener == Some(id))
    }
}
