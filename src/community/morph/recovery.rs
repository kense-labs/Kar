use super::projection::Projection;
use crate::chat::{ChatFrame, ChatTurn};
use crate::{Error, Event, Result};
use serde_json::Value;
use uuid::Uuid;

/// The submit response binds the local input to the task. A websocket observation
/// alone cannot establish that identity, since another client can steer the topic.
pub(crate) struct Saved {
    local: Uuid,
    task: String,
    projection: Projection,
    valid: bool,
}

pub(crate) enum Resume {
    Unconfirmed,
    Running {
        task: String,
        seq: u64,
        open_tools: Vec<String>,
    },
    Finished(ChatFrame),
}

impl Saved {
    fn matches(&self, root: &str, prompt: &str, info: &Value) -> bool {
        info["id"] == self.task
            && info["topic_id"] == root
            && info["task"].as_str() == Some(prompt.trim())
            && info["steer_target_task_id"]
                .as_str()
                .is_none_or(str::is_empty)
    }
    pub(crate) fn running(&self, root: &str, prompt: &str, info: &Value) -> bool {
        self.matches(root, prompt, info)
            && matches!(
                info["status"].as_str(),
                Some("queued" | "running" | "pending")
            )
    }
    pub(crate) async fn watch(&self, session: &crate::Session) -> Result<()> {
        session
            .rpc(
                "followTask",
                serde_json::json!({"id":self.task,"projection":self.projection}),
            )
            .await?;
        Ok(())
    }
    pub(crate) fn active_task(&self) -> Result<Resume> {
        if !self.valid {
            return Err(Error::Busy);
        }
        Ok(Resume::Running {
            task: self.task.clone(),
            seq: self.projection.cursor(),
            open_tools: self.projection.open_tools(),
        })
    }
    pub(crate) fn record(slot: &mut Option<Self>, local: Uuid, frame: &ChatFrame) {
        let Some(progress) = &frame.morph_progress else {
            return;
        };
        if progress.accepted && slot.as_ref().is_none_or(|saved| saved.local != local) {
            *slot = Some(Self {
                local,
                task: progress.task.clone(),
                projection: Projection::default(),
                valid: true,
            });
        }
        if let Some(saved) = slot
            .as_mut()
            .filter(|saved| saved.valid && saved.local == local)
        {
            saved.valid = saved.task == progress.task
                && !progress.gap
                && !frame.elided
                && saved
                    .projection
                    .replay(&frame.edits, progress.seq, progress.reasoning_bytes);
        }
    }

    pub(crate) fn task(&self, local: &ChatTurn) -> Option<&str> {
        (local.id == self.local && local.native_turn_id.as_deref() == Some(&self.task))
            .then_some(&self.task)
    }

    pub(crate) fn terminal_events(
        &self,
        root: &str,
        prompt: &str,
        info: &Value,
    ) -> Result<Option<Vec<Event>>> {
        if !self.valid || !self.matches(root, prompt, info) {
            return Ok(None);
        }
        let Some(outcome) = Projection::outcome(info) else {
            return Err(Error::Protocol(
                "expected a terminal Morph task status".into(),
            ));
        };
        if !self.projection.covers_remaining(&info["result"]["trace"]) {
            return Ok(None);
        }
        let (events, _, _) = self.projection.clone().finish(&self.task, info, outcome);
        Ok(Some(events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Outcome,
        chat::{ChatEdit, Role},
    };
    use serde_json::json;

    fn saved() -> Saved {
        let mut projection = Projection::default();
        let edits = [
            ChatEdit::Activity {
                event: Event::ToolStarted {
                    id: "task:1".into(),
                    name: "bash".into(),
                },
            },
            ChatEdit::Text {
                role: Role::Assistant,
                key: None,
                text: "开".into(),
                final_text: false,
            },
            ChatEdit::Text {
                role: Role::Reasoning,
                key: None,
                text: "旧".into(),
                final_text: false,
            },
            ChatEdit::Text {
                role: Role::Reasoning,
                key: None,
                text: "新".into(),
                final_text: false,
            },
        ];
        assert!(projection.replay(&edits, 1, Some("新".len())));
        Saved {
            local: Uuid::new_v4(),
            task: "task".into(),
            valid: true,
            projection,
        }
    }

    fn info() -> Value {
        json!({"id":"task","topic_id":"topic","task":"ask","status":"done","result":{
            "final":{"output":"开始"},"reasoning":"新理由",
            "trace":{"omitted":1,"entries":[{"seq":2,"event":{"kind":"tool_done","tool_name":"bash","status":"done","text":"ok"}}]}
        }})
    }

    #[test]
    fn extends_partial_text_reasoning_and_the_original_tool() {
        let events = saved()
            .terminal_events("topic", " ask ", &info())
            .unwrap()
            .unwrap();
        assert!(matches!(events.as_slice(), [
            Event::ToolResult {id,..}, Event::ToolEnded {id:end},
            Event::ReasoningDelta {text:reason}, Event::TextDelta{text},
            Event::TurnEnded{outcome:Outcome::Completed}
        ] if id=="task:1" && id==end && text=="始" && reason=="理由"));
    }

    #[test]
    fn checks_identity_status_and_missing_native_entries() {
        for (key, value) in [
            ("id", "another"),
            ("task", "another"),
            ("topic_id", "another"),
            ("steer_target_task_id", "another"),
        ] {
            let mut native = info();
            native[key] = value.into();
            assert!(
                saved()
                    .terminal_events("topic", "ask", &native)
                    .unwrap()
                    .is_none()
            );
        }
        let mut native = info();
        native["status"] = "running".into();
        assert!(saved().running("topic", "ask", &native));
        assert!(!saved().running("another", "ask", &native));
        let mut incomplete = saved();
        incomplete.valid = false;
        assert!(matches!(incomplete.active_task(), Err(Error::Busy)));
        native["status"] = "failed".into();
        assert!(
            incomplete
                .terminal_events("topic", "ask", &native)
                .unwrap()
                .is_none()
        );
        native["error"] = "execution failed".into();
        let events = saved()
            .terminal_events("topic", "ask", &native)
            .unwrap()
            .unwrap();
        assert!(matches!(
            events.last(),
            Some(Event::TurnEnded {
                outcome: Outcome::Failed
            })
        ));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TurnFailure { .. }))
        );
        assert!(!events.iter().any(|e| matches!(e, Event::TextDelta { .. })));
        native["status"] = "canceled".into();
        assert!(matches!(
            saved()
                .terminal_events("topic", "ask", &native)
                .unwrap()
                .unwrap()
                .last(),
            Some(Event::TurnEnded {
                outcome: Outcome::Aborted
            })
        ));
        native["result"]["trace"] = json!({"omitted":2,"entries":[{"seq":1},{"seq":4}]});
        assert!(
            saved()
                .terminal_events("topic", "ask", &native)
                .unwrap()
                .is_none()
        );
    }
}
