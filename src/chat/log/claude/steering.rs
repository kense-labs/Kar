//! Match persisted Claude delivery envelopes to the host's individual submissions.
use crate::{
    SteerStatus,
    chat::{ChatTurn, Item, Role},
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub(super) struct Submissions {
    entries: HashMap<Uuid, (String, SteerStatus)>,
    claimed: HashSet<Uuid>,
}

pub(in crate::chat::log) struct Batch {
    pub inputs: Vec<Uuid>,
    pub at: u64,
}

impl Submissions {
    pub(super) fn new(origin: &ChatTurn, turns: &[ChatTurn], root: &str) -> Self {
        let mut entries = HashMap::new();
        let mut ambiguous = HashSet::new();
        for t in turns
            .iter()
            .filter(|t| t.id == origin.id || t.steered_from == Some(origin.id))
        {
            for item in &t.items {
                if !item.source.path.is_empty() || item.source.session_id.as_deref() != Some(root) {
                    continue;
                }
                let Item::Message {
                    role: Role::User,
                    provider_id: Some(id),
                    text,
                    ..
                } = &item.item
                else {
                    continue;
                };
                let Ok(id) = Uuid::parse_str(id) else {
                    continue;
                };
                let Some(status) = origin.steered_inputs.get(&id) else {
                    continue;
                };
                if entries.insert(id, (text.clone(), *status)).is_some() {
                    ambiguous.insert(id);
                }
            }
        }
        Self {
            entries,
            claimed: ambiguous,
        }
    }

    pub(super) fn in_turn(&mut self, data: &Value) -> Option<(Uuid, String)> {
        if data["commandMode"] != "prompt" {
            return None;
        }
        let id = uuid(&data["source_uuid"])?;
        let (text, status) = self.entries.get(&id)?;
        if !matches!(
            status,
            SteerStatus::Accepted
                | SteerStatus::Queued
                | SteerStatus::Submitted
                | SteerStatus::Unconfirmed
        ) || visible_text(&data["prompt"])? != *text
            || !self.claimed.insert(id)
        {
            return None;
        }
        Some((id, text.clone()))
    }

    pub(super) fn next_turn(&mut self, id: Uuid, row: &Value) -> Option<Batch> {
        if row["isMeta"] == true
            || row
                .get("turnAttributionKey")
                .is_some_and(|v| uuid(v) != Some(id))
        {
            return None;
        }
        let mut ids = Vec::new();
        if let Some(merged) = row.get("mergedSendUuids") {
            for entry in merged.as_array()? {
                ids.push(uuid(entry)?);
            }
        }
        ids.push(id);
        let mut texts = Vec::with_capacity(ids.len());
        let mut seen = HashSet::new();
        for id in &ids {
            let (text, status) = self.entries.get(id)?;
            if !matches!(
                status,
                SteerStatus::FollowUp
                    | SteerStatus::Queued
                    | SteerStatus::Submitted
                    | SteerStatus::Unconfirmed
            ) || self.claimed.contains(id)
                || !seen.insert(*id)
            {
                return None;
            }
            texts.push(text.as_str());
        }
        if visible_text(&row["message"]["content"])? != texts.join("\n") {
            return None;
        }
        let at = row["timestamp"]
            .as_str()?
            .parse::<jiff::Timestamp>()
            .ok()?
            .as_millisecond()
            .try_into()
            .ok()?;
        self.claimed.extend(ids.iter().copied());
        Some(Batch { inputs: ids, at })
    }
}

pub(super) fn uuid(value: &Value) -> Option<Uuid> {
    Uuid::parse_str(value.as_str()?).ok()
}

fn visible_text(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_owned());
    }
    let mut text = String::new();
    for part in value.as_array()? {
        match part["type"].as_str()? {
            "text" => text.push_str(part["text"].as_str()?),
            "image" => {}
            _ => return None,
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merged_input_requires_every_uuid_in_order_and_matching_text() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let row = json!({"timestamp":"2026-11-23T00:00:00Z","turnAttributionKey":b,
            "mergedSendUuids":[a],"message":{"content":[{"type":"image","source":{"data":"image-payload"}},
                {"type":"text","text":"first\n"},{"type":"text","text":"second"}]}});
        let mut submissions = Submissions {
            entries: HashMap::from([
                (a, ("first".into(), SteerStatus::Unconfirmed)),
                (b, ("second".into(), SteerStatus::Queued)),
            ]),
            claimed: HashSet::new(),
        };
        for (key, bad) in [
            ("mergedSendUuids", json!([Uuid::new_v4()])),
            ("mergedSendUuids", json!([a, a])),
            ("turnAttributionKey", json!(a)),
            ("mergedSendUuids", json!([])),
            ("isMeta", json!(true)),
            ("message", json!({"content":"second\nfirst"})),
            ("timestamp", json!(null)),
        ] {
            let mut changed = row.clone();
            changed[key] = bad;
            assert!(submissions.next_turn(b, &changed).is_none(), "{key}");
        }
        assert_eq!(submissions.next_turn(b, &row).unwrap().inputs, [a, b]);
        assert!(submissions.next_turn(b, &row).is_none());
    }

    #[test]
    fn queued_command_requires_the_registered_input_and_unique_delivery() {
        let id = Uuid::new_v4();
        let mut submissions = Submissions {
            entries: HashMap::from([(id, ("same".into(), SteerStatus::Submitted))]),
            claimed: HashSet::new(),
        };
        let data = json!({"source_uuid":id,"commandMode":"prompt","prompt":[{"type":"image"},{"type":"text","text":"same"}]});
        for (key, value) in [
            ("source_uuid", json!(Uuid::new_v4())),
            ("prompt", json!("changed")),
            ("commandMode", json!("shell")),
        ] {
            let mut changed = data.clone();
            changed[key] = value;
            assert!(submissions.in_turn(&changed).is_none());
        }
        assert_eq!(submissions.in_turn(&data), Some((id, String::from("same"))));
        assert!(submissions.in_turn(&data).is_none());
        submissions.claimed.clear();
        submissions.entries.get_mut(&id).unwrap().1 = SteerStatus::FollowUp;
        assert!(submissions.in_turn(&data).is_none());
    }
}
