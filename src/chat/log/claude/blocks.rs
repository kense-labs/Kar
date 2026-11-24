//! Read only visible SDK blocks on the parent chain descending from the host input.
use crate::{
    Protocol,
    chat::{ChatEdit, ChatTurn, Item, Role},
    observe::Tool,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub(super) struct Transcript {
    pub(super) input: Uuid,
    leaf: Option<Uuid>,
    past_input: bool,
    rows: HashSet<Uuid>,
    text_keys: HashSet<String>,
    local_keys: HashSet<String>,
    tools: HashMap<String, (Uuid, usize)>,
    edits: Vec<ChatEdit>,
}

impl Transcript {
    pub(super) fn new(turn: &ChatTurn, root: &str) -> Self {
        let local_keys = turn
            .items
            .iter()
            .filter(|i| i.source.path.is_empty() && i.source.session_id.as_deref() == Some(root))
            .filter_map(|i| match &i.item {
                Item::Message { provider_id, .. } => provider_id.clone(),
                _ => None,
            })
            .collect();
        Self {
            input: turn.id,
            leaf: None,
            past_input: false,
            rows: HashSet::new(),
            text_keys: HashSet::new(),
            local_keys,
            tools: HashMap::new(),
            edits: vec![],
        }
    }

    pub(super) fn start(&mut self) {
        self.leaf = Some(self.input);
        self.rows.insert(self.input);
    }

    pub(super) fn started(&self) -> bool {
        self.leaf.is_some()
    }

    pub(super) fn push(&mut self, id: Uuid, row: &Value) -> Option<()> {
        let parent = Uuid::parse_str(row["parentUuid"].as_str()?).ok()?;
        if self.leaf != Some(parent) || !self.rows.insert(id) {
            return None;
        }
        self.leaf = Some(id);
        let kind = row["type"].as_str()?;
        if self.past_input {
            return Some(());
        }
        if kind == "system" {
            self.past_input = true;
            return Some(());
        }
        if kind == "attachment" {
            // A notification or queued input may start another native model loop.
            self.past_input = matches!(
                row.pointer("/attachment/type")?.as_str()?,
                "task_notification" | "queued_command"
            );
            return Some(());
        }
        if kind == "user" && row["promptSource"] == "sdk" && row["message"]["role"] == "user" {
            self.past_input = true;
            return Some(());
        }
        if row["isMeta"] == true
            || row["isApiErrorMessage"] == true
            || row["message"]["role"] != kind
        {
            return None;
        }
        let blocks = row["message"]["content"].as_array()?;
        for (i, block) in blocks.iter().enumerate() {
            match (kind, block["type"].as_str()?) {
                ("assistant", "text" | "thinking") => {
                    let (role, text) = if block["type"] == "thinking" {
                        (Role::Reasoning, block["thinking"].as_str()?)
                    } else {
                        (Role::Assistant, block["text"].as_str()?)
                    };
                    let message = row["message"]["id"].as_str()?.trim();
                    if message.is_empty() || row["message"]["model"] == "<synthetic>" {
                        return None;
                    }
                    // The wire stream uses the model id and block index. An SDK
                    // envelope without its stream uses the packet UUID instead.
                    let packet_key = format!("{id}#{i}");
                    let stream_key = match row.get("apiBlockIndex") {
                        Some(index) => {
                            format!("{message}#{}", index.as_u64()?.checked_add(i as u64)?)
                        }
                        None => packet_key.clone(),
                    };
                    if !self.text_keys.insert(stream_key.clone()) {
                        return None;
                    }

                    let key = if self.local_keys.contains(&packet_key) {
                        packet_key
                    } else {
                        stream_key
                    };
                    self.edits.push(ChatEdit::Text {
                        role,
                        text: text.into(),
                        key: Some(key),
                        final_text: true,
                    });
                }
                ("assistant", "tool_use") => {
                    let call = block["id"].as_str().filter(|id| !id.is_empty())?;
                    block["name"].as_str().filter(|name| !name.is_empty())?;
                    block["input"].as_object()?;
                    if self
                        .tools
                        .insert(call.into(), (id, self.edits.len()))
                        .is_some()
                    {
                        return None;
                    }
                    self.edits.push(ChatEdit::ToolSnapshot {
                        id: call.into(),
                        activity: project_tool(kind, block, call),
                    });
                }
                ("user", "tool_result") => {
                    let call = block["tool_use_id"].as_str()?;
                    let &(owner, index) = self.tools.get(call)?;
                    if row.get("sourceToolAssistantUUID").is_some_and(|v| {
                        v.as_str().and_then(|v| Uuid::parse_str(v).ok()) != Some(owner)
                    }) {
                        return None;
                    }
                    let ChatEdit::ToolSnapshot { activity, .. } = &mut self.edits[index] else {
                        return None;
                    };
                    if activity.result.is_some() {
                        return None;
                    }
                    activity.result = Some(project_tool(kind, block, call).result?);
                    activity.finished = true;
                }
                ("assistant", "redacted_thinking") => {}
                _ => return None,
            }
        }
        Some(())
    }

    pub(super) fn finish(self) -> Vec<ChatEdit> {
        self.edits
    }
}

fn project_tool(kind: &str, block: &Value, call: &str) -> Tool {
    let packet = json!({"type":kind,"message":{"role":kind,"content":[block]}});
    let mut tool = Tool::default();
    for event in crate::wire::project(Protocol::Claude, &packet, false) {
        if Tool::id(&event) == Some(call) {
            tool.apply(&event, false);
        }
    }
    tool
}
