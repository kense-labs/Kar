//! A live Antigravity execute call can outlast the prompt that started it.
use crate::{BackgroundKind, BackgroundWork, Event, TaskChange, TaskStatus};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Commands {
    calls: HashMap<String, Call>,
    next_order: usize,
}

#[derive(Default)]
struct Call {
    execute: bool,
    scheduled: bool,
    daemon: bool,
    running: bool,
    published: bool,
    turn: Option<Uuid>,
}

impl Commands {
    pub(super) fn frame(
        &mut self,
        packet: &Value,
        turn: Option<Uuid>,
        live: bool,
        events: &mut Vec<Event>,
    ) {
        if packet["method"] != "session/update" {
            return;
        }
        let update = &packet["params"]["update"];
        if !matches!(
            update["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            return;
        }
        let Some(id) = update["toolCallId"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        if let Some(status) = match update["status"].as_str() {
            Some("completed") => Some(TaskStatus::Completed),
            Some("failed") => Some(TaskStatus::Failed),
            Some("cancelled") => Some(TaskStatus::Stopped),
            _ => None,
        } {
            if self.calls.remove(id).is_some_and(|c| c.published) {
                events.push(Event::Task {
                    id: id.into(),
                    change: TaskChange::Ended {
                        status,
                        summary: None,
                        output: None,
                        output_file: None,
                    },
                });
            }
            return;
        }
        // Historical session/load updates have no live foreground owner.
        if !live && !self.calls.contains_key(id) {
            return;
        }
        let call = self.calls.entry(id.into()).or_insert_with(|| Call {
            turn,
            ..Default::default()
        });
        if let Some(kind) = update["kind"].as_str() {
            call.execute = kind == "execute";
        }
        if let Some(title) = update["title"].as_str() {
            call.scheduled = matches!(title, "Run schedule" | "Running schedule");
        }
        if let Some(daemon) = update
            .pointer("/rawInput/IsDaemon")
            .and_then(Value::as_bool)
        {
            call.daemon = daemon;
        }
        if let Some(status) = update["status"].as_str() {
            call.running = status == "in_progress";
        }
        let held = call.running && (call.scheduled || call.execute && !call.daemon);
        if held == call.published {
            return;
        }
        call.published = held;
        events.push(super::mark(
            id,
            BackgroundWork {
                kind: BackgroundKind::Command,
                pending: held,
                awaited: held,
                order: self.next_order,
                turn_id: call.turn,
                description: None,
                tool_use_id: Some(id.into()),
                command: None,
                parent_task_id: None,
            },
        ));
        self.next_order = self.next_order.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn held_command_excludes_daemons_and_historical_replay_and_ends_after_prompt() {
        let mut commands = Commands::default();
        let turn = Uuid::new_v4();
        let frame = |id: &str, kind: &str, status: &str, daemon: bool| {
            json!({
                "method":"session/update","params":{"sessionId":"native","update":{
                    "sessionUpdate":"tool_call_update","toolCallId":id,"status":status,"kind":kind,"rawInput":{"IsDaemon":daemon}
                }}
            })
        };
        let mut events = vec![];
        commands.frame(
            &frame("replayed", "execute", "in_progress", false),
            None,
            false,
            &mut events,
        );
        commands.frame(
            &frame("server", "execute", "in_progress", true),
            Some(turn),
            true,
            &mut events,
        );
        assert!(events.is_empty());
        commands.frame(
            &frame("build", "execute", "in_progress", false),
            Some(turn),
            true,
            &mut events,
        );
        assert!(
            matches!(&events[..],[Event::Task {id,change:TaskChange::Background{work}}] if id=="build" && work.awaited && work.turn_id==Some(turn))
        );
        events.clear();
        commands.frame(
            &frame("build", "execute", "completed", false),
            None,
            false,
            &mut events,
        );
        assert!(
            matches!(&events[..],[Event::Task{id, change: TaskChange::Ended{status:TaskStatus::Completed,..}}] if id=="build")
        );
    }
}
