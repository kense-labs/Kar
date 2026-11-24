use super::{Actor, Event, Outcome, Protocol, RecordKind, Result, Value, json};
use uuid::Uuid;

pub(super) fn private_suggestion(packet: &Value) -> bool {
    if let Some(id) = packet["id"].as_str() {
        return id.starts_with(crate::prompt_suggestions::PREFIX)
            && (packet["method"] == "thread/sample"
                || packet.get("method").is_none()
                    && (packet.get("result").is_some() || packet.get("error").is_some()));
    }
    let id = match packet["type"].as_str() {
        Some("control_request") => packet["request_id"].as_str(),
        Some("control_response") => packet["response"]["request_id"].as_str(),
        _ => None,
    };
    id.is_some_and(|id| id.starts_with(crate::prompt_suggestions::PREFIX))
}

impl Actor {
    pub(super) fn suggestion_frame(&mut self, packet: &Value) -> bool {
        if let Some(native) = &mut self.suggestions
            && private_suggestion(packet)
        {
            if self.protocol == Protocol::Codex {
                native.result(packet, &self.secrets);
            }
            return true;
        }
        if self.protocol != Protocol::Claude || packet["type"] != "prompt_suggestion" {
            return false;
        }
        let root = self.session();
        let Some(native) = &mut self.suggestions else {
            return false;
        };
        if root.as_deref().is_some_and(|id| packet["session_id"] == id)
            && packet["parent_tool_use_id"].is_null()
        {
            let idle = self.turn.is_none()
                && !self.claude_notifications.active()
                && self.claude_followup.is_none()
                && self.manual_compaction.is_none();
            native.claude_message(packet, idle, &self.secrets);
        }
        true
    }

    pub(super) fn next_prompt_sample(
        &mut self,
        packet: &Value,
        active: Option<Uuid>,
        events: &[Event],
        outcome: Option<&Outcome>,
    ) -> Option<Value> {
        let root = self.session();
        let suggestions = self.suggestions.as_mut()?;
        if events.iter().any(|e| {
            matches!(
                e,
                Event::NotificationActivity { .. } | Event::SteeringTurnStarted { .. }
            )
        }) || active.is_none() && events.contains(&Event::TurnStarted)
        {
            suggestions.start(None);
            return None;
        }
        let turn = active?;
        if outcome != Some(&Outcome::Completed)
            || self.manual_compaction.is_some()
            || !suggestions.complete(turn)
        {
            return None;
        }
        if self.protocol == Protocol::Codex
            && packet["method"] == "turn/completed"
            && let Some(root) = root
            && packet["params"]["threadId"] == root
        {
            let native = packet["params"]["turn"]["id"].as_str()?;
            return Some(suggestions.request(&root, native));
        }
        None
    }

    pub(super) async fn configure_prompt_suggestions(&mut self) -> Result<()> {
        if self.protocol != Protocol::Claude {
            return Ok(());
        }
        let Some((initialize, enabled)) =
            self.suggestions.as_mut().and_then(|s| s.claude_setting())
        else {
            return Ok(());
        };
        if initialize {
            // Claude accepts the opt-in on initial and subsequent SDK initialize requests.
            self.suggestion_control(json!({"subtype":"initialize","promptSuggestions":true}))
                .await?;
        }
        self.suggestion_control(
            json!({"subtype":"set_prompt_suggestions_paused","paused":!enabled}),
        )
        .await
    }

    async fn suggestion_control(&mut self, request: Value) -> Result<()> {
        let id = format!(
            "{}{}",
            crate::prompt_suggestions::PREFIX,
            uuid::Uuid::new_v4()
        );
        self.write_packet(
            json!({"type":"control_request","request_id":id,"request":request}),
            RecordKind::Request,
            Some(vec![]),
        )
        .await
    }
}
