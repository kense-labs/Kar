//! Cursor ACP publishes child lifecycle on the parent and child content on a separate session.
use crate::{Event, TaskChange, TaskDetails, TaskStatus, TaskType};
use serde_json::Value;
use std::collections::HashMap;

struct Child {
    parent: String,
    path: Option<Vec<String>>,
    ended: bool,
}

#[derive(Default)]
pub(crate) struct Children {
    pub negotiated: bool,
    sessions: HashMap<String, Child>,
}

fn id(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

impl Children {
    pub fn path(&self, session: &str) -> Option<Vec<String>> {
        self.sessions.get(session)?.path.clone()
    }

    pub fn observe(&mut self, root: Option<&str>, packet: &Value, events: &mut Vec<Event>) {
        if packet["method"] != "session/update" {
            return;
        }
        let params = &packet["params"];
        let Some(parent) = id(&params["sessionId"]) else {
            return;
        };
        let Some(root) = root else {
            return;
        };
        if parent != root && !self.sessions.contains_key(parent) {
            // Isolated streams retain the original native frame but cannot attach a foreign session.
            events.clear();
            return;
        }
        if !self.negotiated {
            return;
        }
        let update = &params["update"];
        let Some(child) = id(&update["subagentSessionId"]).filter(|c| *c != parent && *c != root)
        else {
            return;
        };
        let change = match update["sessionUpdate"].as_str() {
            Some("subagent_spawned") => {
                if self.sessions.contains_key(child) {
                    return;
                }
                let call = id(&update["_meta"]["cursor"]["toolCallId"]);
                let path = call.and_then(|call| {
                    let mut path = if parent == root {
                        Vec::new()
                    } else {
                        self.path(parent)?
                    };
                    path.push(call.into());
                    Some(path)
                });
                self.sessions.insert(
                    child.into(),
                    Child {
                        parent: parent.into(),
                        path,
                        ended: false,
                    },
                );
                events.push(Event::SessionLinked {
                    parent: parent.into(),
                    child: child.into(),
                });
                TaskChange::Started {
                    task_type: TaskType::Agent,
                    details: TaskDetails {
                        native_type: id(&update["name"]).map(str::to_owned),
                        description: id(&update["task"]).map(str::to_owned),
                        tool_call_id: call.map(str::to_owned),
                        child_session_id: Some(child.into()),
                        ..Default::default()
                    },
                }
            }
            Some("subagent_state_update") => {
                let Some(bound) = self
                    .sessions
                    .get_mut(child)
                    .filter(|s| s.parent == parent && !s.ended)
                else {
                    return;
                };
                let status = match update["state"].as_str() {
                    Some("completed") => TaskStatus::Completed,
                    Some("failed") => TaskStatus::Failed,
                    Some("cancelled") => TaskStatus::Stopped,
                    Some("disconnected") => TaskStatus::Disconnected,
                    _ => return,
                };
                bound.ended = true;
                TaskChange::Ended {
                    status,
                    summary: None,
                    output: None,
                    output_file: None,
                }
            }
            _ => return,
        };
        events.push(Event::Task {
            id: child.into(),
            change,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requires_negotiation_and_a_link_to_the_current_root() {
        let mut children = Children::default();
        let mut events = Vec::new();
        let mut notice = json!({"method":"session/update","params":{"sessionId":"root","update":{"sessionUpdate":"subagent_spawned","subagentSessionId":"child","_meta":{"cursor":{"toolCallId":"call"}}}}});
        children.observe(Some("root"), &notice, &mut events);
        assert!(events.is_empty());
        children.negotiated = true;
        notice["params"]["sessionId"] = "unrelated".into();
        children.observe(Some("root"), &notice, &mut events);
        assert!(events.is_empty());
        notice["params"]["sessionId"] = "root".into();
        children.observe(Some("root"), &notice, &mut events);
        assert_eq!(children.path("child").unwrap(), ["call"]);
        assert_eq!(events.len(), 2);
        events.clear();
        notice["params"]["sessionId"] = "child".into();
        notice["params"]["update"]["subagentSessionId"] = "root".into();
        children.observe(Some("root"), &notice, &mut events);
        assert!(events.is_empty());
        notice["params"]["update"]["subagentSessionId"] = "grandchild".into();
        notice["params"]["update"]["_meta"] = Value::Null;
        children.observe(Some("root"), &notice, &mut events);
        assert!(children.path("grandchild").is_none());
        assert!(events.iter().any(|e|matches!(e,Event::SessionLinked{parent,child} if parent=="child" && child=="grandchild")));
    }
}
