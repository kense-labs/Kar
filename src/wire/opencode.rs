//! ACP child carriers used by OpenCode v2. Preserve the provider's original packet in records.
use crate::{Event, Protocol, TaskChange, TaskDetails, TaskStatus, TaskType};
use serde_json::{Value, json};
use std::collections::HashSet;

const EXTENSION: &str = "opencode/session/child_update";
pub(crate) const CAPABILITY: &str = "opencode/child-session-updates";

fn id(v: &Value) -> Option<&str> {
    v.as_str().filter(|v| !v.is_empty())
}

pub(crate) struct Notice<'a> {
    pub parent: &'a str,
    pub child: &'a str,
    pub source: &'a str,
    pub extension: bool,
}
pub(crate) fn notice<'a>(packet: &'a Value, root: Option<&str>) -> Option<Notice<'a>> {
    let args = &packet["params"];
    let (parent, child, source, carrier, extension) = match packet["method"].as_str()? {
        EXTENSION => {
            let parent = id(&args["parentSessionId"])?;
            let child = id(&args["childSessionId"])?;
            let source = match args["type"].as_str()? {
                "update" if args["update"].is_object() => child,
                "status" => parent,
                _ => return None,
            };
            (parent, child, source, id(&args["rootSessionId"])?, true)
        }
        "session/update" => {
            let update = &args["update"];
            let meta = update
                .pointer("/_meta/opencode~1child-session")
                .or_else(|| args.pointer("/_meta/opencode~1child-session"))?;
            let child = id(&meta["id"])?;
            (
                id(&meta["parentID"])?,
                child,
                child,
                id(&args["sessionId"])?,
                false,
            )
        }
        _ => return None,
    };
    (parent != child && child != carrier && root.is_none_or(|r| r == carrier)).then_some(Notice {
        parent,
        child,
        source,
        extension,
    })
}

pub(crate) fn spawned(update: &Value) -> Option<&str> {
    if !matches!(
        update["sessionUpdate"].as_str(),
        Some("tool_call" | "tool_call_update")
    ) {
        return None;
    }
    id(update.pointer("/rawOutput/metadata/sessionID")?)
}

#[derive(Default)]
pub(crate) struct Children {
    pub extended: bool,
    claims: HashSet<(String, String)>,
    spawns: HashSet<(String, String)>,
    linked: HashSet<(String, String)>,
}
pub(crate) const STEER: &str = "_session/steering";
impl Children {
    fn link(
        &mut self,
        parent: &str,
        child: &str,
        claim: bool,
        direct: bool,
        events: &mut Vec<Event>,
    ) {
        if parent == child {
            return;
        }
        let edge = (parent.to_owned(), child.to_owned());
        if claim {
            self.claims.insert(edge.clone());
        } else {
            self.spawns.insert(edge.clone());
        }
        if (direct || self.claims.contains(&edge) && self.spawns.contains(&edge))
            && self.linked.insert(edge)
        {
            events.push(Event::SessionLinked {
                parent: parent.into(),
                child: child.into(),
            });
        }
    }
    pub(crate) fn project(
        &mut self,
        root: Option<&str>,
        packet: &Value,
        compactions: &mut super::compaction::Compactions,
    ) -> Option<Vec<Event>> {
        let method = packet["method"].as_str()?;
        if !matches!(method, EXTENSION | "session/update") {
            return None;
        }
        let args = &packet["params"];
        let update = &args["update"];
        let mut events = vec![];
        let source = if let Some(n) = notice(packet, root) {
            // Opt-in moves child updates onto the extension carrier. A standard copy may arrive first.
            if self.extended != n.extension {
                return Some(events);
            }
            if n.extension && args["type"] == "status" {
                let change = match args["status"].as_str() {
                    Some("created") => Some(TaskChange::Started {
                        task_type: TaskType::Agent,
                        details: TaskDetails {
                            background: Some(true),
                            child_session_id: Some(n.child.into()),
                            description: id(&args["title"]).map(str::to_owned),
                            ..Default::default()
                        },
                    }),
                    Some("running") => Some(TaskChange::Updated {
                        status: Some(TaskStatus::Running),
                        description: None,
                        background: None,
                        error: None,
                    }),
                    Some(s @ ("completed" | "failed" | "interrupted")) => Some(TaskChange::Ended {
                        status: match s {
                            "completed" => TaskStatus::Completed,
                            "failed" => TaskStatus::Failed,
                            _ => TaskStatus::Stopped,
                        },
                        summary: None,
                        output: None,
                        output_file: None,
                    }),
                    _ => None,
                };
                if let Some(change) = change {
                    events.push(Event::Task {
                        id: n.child.into(),
                        change,
                    });
                }
            } else {
                let normalized = json!({"method":"session/update","params":{"sessionId":n.child,"update":update}});
                events = super::project(Protocol::Acp, &normalized, false);
                events.extend(compactions.observe(Protocol::Acp, &normalized));
            }
            self.link(n.parent, n.child, true, !n.extension, &mut events);
            n.source
        } else {
            if method == EXTENSION
                || args.pointer("/_meta/opencode~1child-session").is_some()
                || update.pointer("/_meta/opencode~1child-session").is_some()
            {
                return Some(events);
            }
            let carrier = id(&args["sessionId"])?;
            if root.is_some_and(|r| r != carrier) {
                return Some(events);
            }
            events = super::project(Protocol::Acp, packet, false);
            events.extend(compactions.observe(Protocol::Acp, packet));
            carrier
        };
        if let Some(child) = spawned(update).filter(|id| Some(*id) != root) {
            self.link(source, child, false, false, &mut events);
        }
        Some(events)
    }
}
