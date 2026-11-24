//! Read only visible SDK blocks on the parent chain descending from the host input.
use super::{
    History,
    steering::{Batch, Submissions, uuid},
};
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
    prompt: Option<Uuid>,
    submissions: Submissions,
    pending: Vec<(Uuid, String)>,
    group: Option<Batch>,
    completed: History,
    group_keys: HashMap<Uuid, HashSet<String>>,

    past_input: bool,
    rows: HashSet<Uuid>,
    text_keys: HashSet<String>,
    local_keys: HashSet<String>,
    tools: HashMap<String, (Uuid, usize)>,
    edits: Vec<ChatEdit>,
}

impl Transcript {
    fn keys(turn: &ChatTurn, root: &str) -> HashSet<String> {
        turn.items
            .iter()
            .filter(|i| i.source.path.is_empty() && i.source.session_id.as_deref() == Some(root))
            .filter_map(|i| match &i.item {
                Item::Message { provider_id, .. } => provider_id.clone(),
                _ => None,
            })
            .collect()
    }

    pub(super) fn for_family(turn: &ChatTurn, turns: &[ChatTurn], root: &str) -> Self {
        Self {
            input: turn.id,
            prompt: None,
            group: None,
            pending: vec![],
            submissions: Submissions::new(turn, turns, root),
            completed: History {
                origin: Some(turn.id),
                ..History::default()
            },
            group_keys: turns
                .iter()
                .filter(|t| t.steered_from == Some(turn.id))
                .map(|t| (t.id, Self::keys(t, root)))
                .collect(),
            leaf: None,
            past_input: false,
            rows: HashSet::new(),
            text_keys: HashSet::new(),
            local_keys: Self::keys(turn, root),
            tools: HashMap::new(),
            edits: vec![],
        }
    }

    pub(super) fn start(&mut self, row: &Value) {
        self.prompt = uuid(&row["promptId"]);
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
            match row.pointer("/attachment/type")?.as_str()? {
                "task_notification" => self.past_input = true,
                "queued_command" => match self.submissions.in_turn(&row["attachment"]) {
                    Some(input) if self.group.is_none() => self.pending.push(input),
                    _ => self.past_input = true,
                },
                _ => {}
            }
            return Some(());
        }
        if kind == "user" && row["promptSource"] == "sdk" && row["message"]["role"] == "user" {
            let Some(batch) = self.submissions.next_turn(id, row) else {
                self.past_input = true;
                return Some(());
            };
            self.finish_segment();
            self.local_keys = self.group_keys.remove(&batch.inputs[0]).unwrap_or_default();
            self.text_keys.clear();
            self.tools.clear();
            self.pending.clear();
            self.prompt = uuid(&row["promptId"]);
            self.group = Some(batch);
            return Some(());
        }
        // A Claude image-paste companion has no host input or model output.
        if kind == "user"
            && row["turnCompanion"] == true
            && row["isMeta"] == true
            && self.prompt.is_some()
            && uuid(&row["promptId"]) == self.prompt
        {
            return Some(());
        }
        if row["isMeta"] == true
            || row["isApiErrorMessage"] == true
            || row["message"]["role"] != kind
        {
            return None;
        }
        if kind == "assistant" {
            row["message"]["id"]
                .as_str()
                .filter(|id| !id.trim().is_empty())?;
            row["message"]["model"]
                .as_str()
                .filter(|model| !model.is_empty() && *model != "<synthetic>")?;
        }
        let blocks = row["message"]["content"].as_array()?;
        let before = self.edits.len();
        for (i, block) in blocks.iter().enumerate() {
            match (kind, block["type"].as_str()?) {
                ("assistant", "text" | "thinking") => {
                    let (role, text) = if block["type"] == "thinking" {
                        (Role::Reasoning, block["thinking"].as_str()?)
                    } else {
                        (Role::Assistant, block["text"].as_str()?)
                    };
                    let message = row["message"]["id"].as_str()?.trim();
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
        // A queued attachment alone does not prove a model step consumed it.
        if kind == "assistant" && self.edits.len() > before {
            self.edits.extend(
                self.pending
                    .drain(..)
                    .map(|(id, text)| ChatEdit::SteeringInput {
                        attachments: Vec::new(),
                        id,
                        text,
                        status: crate::SteerStatus::Accepted,
                        request_id: None,
                    }),
            );
        }
        Some(())
    }

    fn finish_segment(&mut self) {
        let edits = std::mem::take(&mut self.edits);
        match self.group.take() {
            Some(batch) if !edits.is_empty() => self.completed.groups.push((batch, edits)),
            None => self.completed.edits = edits,
            _ => {}
        }
    }

    pub(super) fn finish(mut self) -> History {
        self.finish_segment();
        self.completed
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
