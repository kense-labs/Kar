use crate::{Event, Protocol, TaskChange, TaskDetails, TaskStatus as Status, TaskType as Type};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn string(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}
fn status(v: &Value) -> Option<Status> {
    Some(match v.as_str()? {
        "running" => Status::Running,
        "pending" | "pendingInit" => Status::Pending,
        "paused" => Status::Paused,
        "completed" => Status::Completed,
        "failed" | "errored" => Status::Failed,
        "stopped" | "killed" | "interrupted" | "shutdown" => Status::Stopped,
        _ => return None,
    })
}
fn ended(status: Status) -> TaskChange {
    TaskChange::Ended {
        status,
        summary: None,
        output: None,
        output_file: None,
    }
}
fn updated(status: Status) -> TaskChange {
    TaskChange::Updated {
        status: Some(status),
        background: None,
        description: None,
        error: None,
    }
}
fn task(id: &str, change: TaskChange) -> Event {
    Event::Task {
        id: id.into(),
        change,
    }
}
pub(super) fn reported(p: Protocol, v: &Value) -> Option<Event> {
    if p == Protocol::Acp && v["method"] == "_x.ai/session_notification" {
        let u = &v["params"]["update"];
        let id = string(&u["subagent_id"])?;
        let change = match u["sessionUpdate"].as_str()? {
            "subagent_spawned" => TaskChange::Started {
                task_type: Type::Agent,
                details: TaskDetails {
                    native_type: string(&u["subagent_type"]),
                    model: string(&u["model"]),
                    description: string(&u["description"]),
                    child_session_id: string(&u["child_session_id"]),
                    ..Default::default()
                },
            },
            "subagent_finished" => TaskChange::Ended {
                status: status(&u["status"]).filter(|s| s.is_terminal())?,
                output: string(&u["output"]),
                summary: None,
                output_file: None,
            },
            _ => return None,
        };
        return Some(task(&id, change));
    }
    if !matches!(p, Protocol::Claude | Protocol::Qoder | Protocol::Qwen) || v["type"] != "system" {
        return None;
    }
    if p == Protocol::Qwen {
        let body = &v["data"];
        let id = string(&body["task_id"])?;
        let change = match v["subtype"].as_str()? {
            "task_started" => {
                let native_type = body["subagent_type"].as_str().map(str::to_owned);
                TaskChange::Started {
                    task_type: if native_type.is_some() {
                        Type::Agent
                    } else {
                        Type::Other
                    },
                    details: TaskDetails {
                        native_type,
                        description: body["description"].as_str().map(str::to_owned),
                        tool_call_id: body["tool_use_id"].as_str().map(str::to_owned),
                        ..Default::default()
                    },
                }
            }
            "task_notification" => ended(match body["status"].as_str()? {
                "completed" => Status::Completed,
                "failed" => Status::Failed,
                "stopped" => Status::Stopped,
                _ => return None,
            }),
            _ => return None,
        };
        return Some(task(&id, change));
    }
    let body = v;
    let id = string(&body["task_id"])?;
    let change = match v["subtype"].as_str()? {
        "task_started" => {
            let native_type = string(&body["task_type"]);
            let task_type = match native_type.as_deref() {
                Some("local_agent") => Type::Agent,
                _ if p == Protocol::Qoder => Type::Other,
                Some("local_bash") => Type::Shell,
                Some("remote_agent" | "in_process_teammate") => Type::Agent,
                Some("mcp_task") => Type::Tool,
                Some("local_workflow") => Type::Workflow,
                _ => Type::Other,
            };
            TaskChange::Started {
                task_type,
                details: TaskDetails {
                    native_type,
                    description: string(&body["description"]),
                    tool_call_id: string(&body["tool_use_id"]),
                    background: body["is_backgrounded"].as_bool(),
                    ambient: body["ambient"].as_bool(),
                    ..Default::default()
                },
            }
        }
        "task_updated" if matches!(p, Protocol::Claude | Protocol::Qoder) => {
            let patch = &body["patch"];
            TaskChange::Updated {
                status: status(&patch["status"]),
                background: patch["is_backgrounded"].as_bool(),
                description: string(&patch["description"]),
                error: string(&patch["error"]),
            }
        }
        "task_progress" if matches!(p, Protocol::Claude | Protocol::Qoder) => TaskChange::Updated {
            status: None,
            background: None,
            description: Some(string(&body["description"])?),
            error: None,
        },
        "task_notification" => TaskChange::Ended {
            status: status(&body["status"]).filter(|s| s.is_terminal())?,
            summary: string(&body["summary"]),
            output: None,
            output_file: string(&body["output_file"]),
        },
        _ => return None,
    };
    Some(task(&id, change))
}

#[derive(Default)]
pub(crate) struct CodexTasks {
    parents: HashMap<String, (String, Option<String>)>,
    turns: HashMap<String, String>,
}
impl CodexTasks {
    pub(crate) fn turn(&self, thread: &str) -> Option<&str> {
        self.turns.get(thread).map(String::as_str)
    }
    pub(crate) fn active_child(&self, root: &str, child: &str) -> crate::Result<&str> {
        let mut id = child;
        let mut seen = HashSet::new();
        while id != root && seen.insert(id) {
            let Some((parent, _)) = self.parents.get(id) else {
                break;
            };
            if parent == root {
                return self.turns.get(child).map(String::as_str).ok_or_else(|| {
                    crate::Error::Refused("no active turn observed for this child".into())
                });
            }
            id = parent;
        }
        Err(crate::Error::Unsupported(
            "task is not an observed descendant of the session".into(),
        ))
    }
    pub(crate) fn observe(&mut self, root: Option<&str>, v: &Value, events: &mut Vec<Event>) {
        let p = &v["params"];
        if v["method"] == "thread/started" {
            let thread = &p["thread"];
            if let Some(id) = thread["id"].as_str().filter(|id| !id.is_empty())
                && root.is_some_and(|root| root != id)
            {
                events.extend(super::settings(
                    thread["model"].as_str(),
                    thread["reasoningEffort"].as_str(),
                    None,
                ));
            }
            return;
        }
        let Some(reporter) = p["threadId"].as_str() else {
            return;
        };
        let method = v["method"].as_str().unwrap_or("");
        if let Some(turn) = p["turn"]["id"].as_str() {
            match method {
                "turn/started" => {
                    self.turns.insert(reporter.into(), turn.into());
                }
                "turn/completed" if self.turns.get(reporter).is_some_and(|id| id == turn) => {
                    self.turns.remove(reporter);
                }
                _ => {}
            }
        }
        if !matches!(method, "item/started" | "item/completed") {
            return;
        }
        let item = &p["item"];
        if item["type"] == "subAgentActivity" {
            let Some(child) = item["agentThreadId"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != reporter && Some(*s) != root)
            else {
                return;
            };
            let path = string(&item["agentPath"]);
            let own = self
                .parents
                .get(child)
                .is_some_and(|(parent, _)| parent == reporter);
            if item["kind"] == "started" {
                if self.parents.contains_key(child) && !own {
                    return;
                }
                if !own {
                    self.parents
                        .insert(child.into(), (reporter.into(), path.clone()));
                    events.push(task(
                        child,
                        TaskChange::Started {
                            task_type: Type::Agent,
                            details: TaskDetails {
                                native_type: Some("subAgent".into()),
                                model: string(&item["model"]),
                                description: path,
                                tool_call_id: string(&item["id"]),
                                child_session_id: Some(child.into()),
                                background: Some(true),
                                ..Default::default()
                            },
                        },
                    ));
                }
            } else {
                let reporter_path = if Some(reporter) == root {
                    Some("/root")
                } else {
                    self.parents
                        .get(reporter)
                        .and_then(|(_, path)| path.as_deref())
                };
                let path_matches = path
                    .as_deref()
                    .and_then(|p| p.rsplit_once('/'))
                    .zip(reporter_path)
                    .is_some_and(|((parent, _), reporter)| parent == reporter);
                if !own && (self.parents.contains_key(child) || !path_matches) {
                    return;
                }
                let change = match item["kind"].as_str() {
                    Some("completed") => ended(Status::Completed),
                    Some("interrupted") => ended(Status::Stopped),
                    Some("interacted") => updated(Status::Running),
                    _ => return,
                };
                events.push(task(child, change));
            }
        } else if matches!(
            item["type"].as_str(),
            Some("collabAgentToolCall" | "collabToolCall")
        ) && item["senderThreadId"] == reporter
        {
            for child in item["receiverThreadIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|c| !c.is_empty() && *c != reporter && Some(*c) != root)
            {
                if item["tool"] == "spawnAgent" && !self.parents.contains_key(child) {
                    self.parents.insert(child.into(), (reporter.into(), None));
                    events.push(task(
                        child,
                        TaskChange::Started {
                            task_type: Type::Agent,
                            details: TaskDetails {
                                native_type: Some("subAgent".into()),
                                child_session_id: Some(child.into()),
                                tool_call_id: string(&item["id"]),
                                description: string(&item["prompt"]),
                                background: Some(true),
                                ..Default::default()
                            },
                        },
                    ));
                }
                if self
                    .parents
                    .get(child)
                    .is_some_and(|(owner, _)| owner == reporter)
                    && let Some(s) = status(&item["agentsStates"][child]["status"])
                {
                    events.push(task(
                        child,
                        if s.is_terminal() {
                            ended(s)
                        } else {
                            updated(s)
                        },
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn qwen_task_notifications_preserve_only_documented_task_transitions() {
        let mut frame = json!({"type":"system","subtype":"task_notification","data":{"task_id":"child","status":"completed","summary":"unrelated"}});
        for (native, expected) in [
            ("completed", Status::Completed),
            ("stopped", Status::Stopped),
            ("failed", Status::Failed),
        ] {
            frame["data"]["status"] = native.into();
            assert_eq!(
                reported(Protocol::Qwen, &frame),
                Some(task("child", ended(expected)))
            );
        }
        for native in ["running", "killed", "errored", "shutdown"] {
            frame["data"]["status"] = native.into();
            assert!(reported(Protocol::Qwen, &frame).is_none());
        }
    }
}
