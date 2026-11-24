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
    v2: bool,
    pub extended: bool,
    claims: HashSet<(String, String)>,
    spawns: HashSet<(String, String)>,
    linked: HashSet<(String, String)>,
}
pub(crate) const STEER: &str = "_session/steering";
impl Children {
    pub fn new(v2: bool) -> Self {
        Self {
            v2,
            ..Self::default()
        }
    }
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
        received_ms: u64,
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
        if self.v2
            && let Some(event) = retry(update, received_ms)
        {
            events.push(event);
        }
        if let Some(child) = spawned(update).filter(|id| Some(*id) != root) {
            self.link(source, child, false, false, &mut events);
        }
        Some(events)
    }
}

// The ACP prompt response can carry the last retry again. Only the scheduled update is an event.
fn retry(update: &Value, received_ms: u64) -> Option<Event> {
    if update["sessionUpdate"] != "session_info_update" {
        return None;
    }
    let retry = update.pointer("/_meta/opencode~1retry")?;
    let attempt = retry["attempt"]
        .as_f64()
        .filter(|n| n.fract() == 0.0 && (1.0..=9_007_199_254_740_991.0).contains(n))?
        as u64;
    let delay_ms = retry["nextRetryAt"]
        .as_str()
        .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map(|at| {
            u64::try_from(at.as_millisecond())
                .unwrap_or(0)
                .saturating_sub(received_ms)
        });
    let reason = retry
        .pointer("/error/type")
        .and_then(Value::as_str)
        .filter(|s| {
            s.split('.').all(|part| {
                let mut chars = part.bytes();
                chars.next().is_some_and(|c| c.is_ascii_lowercase())
                    && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            })
        })
        .map(str::to_owned);
    Some(Event::Retry {
        attempt: Some(attempt),
        max_attempts: None,
        delay_ms,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_retry_uses_the_original_record_clock_and_native_category() {
        let at = 1_795_082_202_838;
        let retry = json!({"attempt": 2, "nextRetryAt": "2026-11-19T09:56:45.119Z", "error":{"type":"provider.internal","message":"private details"}});
        let project = |v2, root, attempt: Value, now| {
            Children::new(v2).project(Some(root), &json!({"method":"session/update","params":{
                "sessionId":"root","update":{"sessionUpdate":"session_info_update","_meta":{"opencode/retry":attempt}}
            }}), now, &mut Default::default()).unwrap()
        };
        let projected = project(true, "root", retry.clone(), at);
        assert_eq!(
            serde_json::to_value(&projected).unwrap(),
            json!([{"kind":"retry","attempt":2,"delay_ms":2281,"reason":"provider.internal"}])
        );
        assert!(matches!(
            project(true, "root", retry.clone(), at + 3000).as_slice(),
            [Event::Retry {
                delay_ms: Some(0),
                ..
            }]
        ));
        assert!(project(false, "root", retry.clone(), at).is_empty());
        assert!(project(true, "other", retry, at).is_empty());
        assert!(project(true, "root", Value::Null, at).is_empty());
        assert!(project(true, "root", json!({"attempt":0}), at).is_empty());
        assert!(project(true, "root", json!({"attempt":9007199254740992u64}), at).is_empty());
        let partial = project(
            true,
            "root",
            json!({"attempt":2,"nextRetryAt":"invalid","error":{"type":"provider: private details"}}),
            at,
        );
        assert_eq!(
            serde_json::to_value(partial).unwrap(),
            json!([{"kind":"retry","attempt":2}])
        );
        assert_eq!(
            serde_json::from_value::<Event>(json!({"kind":"retry","reason":"existing"})).unwrap(),
            Event::Retry {
                attempt: None,
                max_attempts: None,
                delay_ms: None,
                reason: Some("existing".into())
            }
        );
    }
}
