//! Claude's live roster is process-owned. Wait marks change the Working view, never the native result.
mod antigravity;

use super::{Actor, Direction, Event, Protocol, RecordKind, Value};
use crate::{
    BackgroundKind, BackgroundWaitReceipt, BackgroundWaitRequest, BackgroundWork, Error,
    NativeTask, Result, TaskChange,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use uuid::Uuid;

pub(super) const WAIT: &str = "kar/background/wait";

#[derive(Default)]
pub(super) struct ClaudeBackground {
    antigravity: Option<antigravity::Commands>,
    tools: VecDeque<Launch>,
    seen: VecDeque<Uuid>,
}
struct Launch {
    id: String,
    monitor: bool,
    command: Option<String>,
    turn: Option<Uuid>,
}

impl ClaudeBackground {
    pub(super) fn for_runtime(protocol: Protocol, id: &str) -> Self {
        Self { antigravity: (protocol == Protocol::Acp && id == "antigravity").then(Default::default), ..Default::default() }
    }
    fn tools(&mut self, events: &[Event], turn: Option<Uuid>, secrets: &[String]) -> bool {
        let mut changed = false;
        for event in events {
            match event {
                Event::ToolStarted { id, name } if matches!(name.as_str(), "Monitor" | "Bash") => {
                    if !self.tools.iter().any(|t| t.id == *id) {
                        changed = true;
                        self.tools.push_back(Launch {
                            id: id.clone(),
                            monitor: name == "Monitor",
                            command: None,
                            turn,
                        });
                        if self.tools.len() > 256 {
                            self.tools.pop_front();
                        }
                    }
                }
                Event::ToolInput { id, input } => {
                    if let Some(tool) = self.tools.iter_mut().find(|t| t.id == *id)
                        && let Some(command) = input["command"].as_str()
                    {
                        let mut text = command.to_owned();
                        super::redact_text(&mut text, secrets);
                        if tool.command.as_ref() != Some(&text) {
                            changed = true;
                            tool.command = Some(text);
                        }
                    }
                }
                _ => {}
            }
        }
        changed
    }
    fn work(
        &self,
        data: &Value,
        prior: Option<&BackgroundWork>,
        launch_id: Option<&str>,
        order: usize,
        turn: Option<Uuid>,
        secrets: &[String],
    ) -> BackgroundWork {
        let tool_id = data["tool_use_id"]
            .as_str()
            .or_else(|| prior?.tool_use_id.as_deref())
            .or(launch_id);
        let tool = tool_id.and_then(|id| self.tools.iter().find(|t| t.id == id));
        let monitor = tool.is_some_and(|t| t.monitor)
            || prior.is_some_and(|p| p.kind == BackgroundKind::Monitor);
        let kind = if monitor {
            BackgroundKind::Monitor
        } else {
            BackgroundKind::Command
        };
        let mut description = data["description"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| prior?.description.clone());
        if let Some(text) = &mut description {
            super::redact_text(text, secrets);
        }
        BackgroundWork {
            kind,
            pending: true,
            order,
            awaited: prior.is_some_and(|p| p.awaited)
                || monitor && prior.is_none_or(|p| p.kind != BackgroundKind::Monitor),
            turn_id: prior
                .and_then(|p| p.turn_id)
                .or_else(|| tool?.turn)
                .or(turn.filter(|_| prior.is_none())),
            tool_use_id: tool_id.map(str::to_owned),
            command: tool
                .and_then(|t| t.command.clone())
                .or_else(|| prior?.command.clone()),
            parent_task_id: data["parent_task_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| prior?.parent_task_id.clone()),
            description,
        }
    }
}

fn mark(id: &str, work: BackgroundWork) -> Event {
    Event::Task {
        id: id.into(),
        change: TaskChange::Background { work },
    }
}
impl Actor {
    pub(super) fn background_frame(&mut self, v: &Value, events: &mut Vec<Event>) {
        if let Some(commands) = &mut self.background.antigravity {
            commands.frame(v, self.turn.as_ref().and_then(|t| t.chat_input), self.turn.is_some(), events);
            return;
        }
        if self.protocol != Protocol::Claude {
            return;
        }
        if v["isReplay"] == true || v["is_replay"] == true {
            if v["type"] == "system"
                && matches!(
                    v["subtype"].as_str(),
                    Some("background_tasks_changed" | "task_started" | "task_notification")
                )
            {
                events.retain(|e| !matches!(e, Event::Task { .. }));
            }
            return;
        }
        let turn = events
            .iter()
            .find_map(|e| {
                if let Event::NotificationActivity { id } = e {
                    Some(*id)
                } else {
                    None
                }
            })
            .or_else(|| self.turn.as_ref().and_then(|t| t.chat_input));
        let launch_changed = self.background.tools(events, turn, &self.secrets);
        let kind = v["subtype"].as_str();
        let roster_frame = v["type"] == "system"
            && matches!(
                kind,
                Some("background_tasks_changed" | "task_started" | "task_notification")
            );
        if !launch_changed && !roster_frame {
            return;
        }
        if roster_frame && let Some(id) = v["uuid"].as_str().and_then(|s| s.parse().ok()) {
            if self.background.seen.contains(&id) {
                events.retain(|e| !matches!(e, Event::Task { .. }));
                return;
            }
            self.background.seen.push_back(id);
            if self.background.seen.len() > 512 {
                self.background.seen.pop_front();
            }
        }
        let snapshot = self.journal.snapshot();
        let running: BTreeMap<_, _> = snapshot
            .tasks
            .iter()
            .filter(|r| {
                r.pending_background()
                    && r.source.path.is_empty()
                    && r.source.session_id.as_deref() == Some(snapshot.id.as_str())
            })
            .map(|r| (r.id.as_str(), r.background_work.as_ref().unwrap()))
            .collect();
        if !roster_frame {
            for (id, prior) in running {
                let work = self.background.work(
                    &Value::Null,
                    Some(prior),
                    None,
                    prior.order,
                    None,
                    &self.secrets,
                );
                if work != *prior {
                    events.push(mark(id, work));
                }
            }
            return;
        }
        if kind == Some("task_notification") {
            let Some(id) = v["task_id"].as_str() else {
                return;
            };
            if let Some(prior) = running.get(id) {
                if v["tool_use_id"]
                    .as_str()
                    .zip(prior.tool_use_id.as_deref())
                    .is_some_and(|(a, b)| a != b)
                {
                    events.retain(|e| !matches!(e, Event::Task { id:key, .. } if key==id));
                } else if !events.iter().any(
                    |e| matches!(e,Event::Task{id:key,change:TaskChange::Ended{..}} if key==id),
                ) {
                    let mut removed = (**prior).clone();
                    removed.pending = false;
                    events.push(mark(id, removed));
                }
            }
            return;
        }
        if kind == Some("task_started") {
            if v["task_type"] != "local_bash" || v["is_backgrounded"] == false {
                return;
            }
            if let Some(id) = v["task_id"].as_str().filter(|s| !s.is_empty()) {
                let prior = running.get(id).copied();
                let order = prior
                    .map(|p| p.order)
                    .unwrap_or_else(|| running.values().map(|w| w.order + 1).max().unwrap_or(0));
                let work = self
                    .background
                    .work(v, prior, None, order, turn, &self.secrets);
                if prior.is_some() {
                    // A listing may precede the detailed start, reusing its original cursor.
                    events.retain(|e|!matches!(e,Event::Task{id:key,change:TaskChange::Started{..}} if key==id));
                }
                if prior != Some(&work) {
                    events.push(mark(id, work))
                }
            }
            return;
        }
        let Some(entries) = v["tasks"].as_array() else {
            return;
        };
        let mut listed = HashSet::new();
        for data in entries {
            let Some(id) = data["task_id"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            if data["task_type"] != "local_bash" || !listed.insert(id) {
                continue;
            }
            let prior = running.get(id).copied();
            let launch_id = snapshot
                .tasks
                .iter()
                .rev()
                .find(|row| {
                    row.id == id
                        && row.source.path.is_empty()
                        && row.source.session_id.as_deref() == Some(snapshot.id.as_str())
                        && !row.status.is_terminal()
                })
                .and_then(|row| row.details.tool_call_id.as_deref());
            let work = self.background.work(
                data,
                prior,
                launch_id,
                listed.len() - 1,
                turn,
                &self.secrets,
            );
            if prior != Some(&work) {
                events.push(mark(id, work))
            }
        }
        for (id, prior) in running {
            if !listed.contains(id) {
                let mut ended = prior.clone();
                ended.pending = false;
                events.push(mark(id, ended));
            }
        }
    }
    pub(super) fn background_wait(
        &self,
        req: BackgroundWaitRequest,
    ) -> Result<BackgroundWaitReceipt> {
        let held_commands = self.background.antigravity.is_some();
        if self.protocol != Protocol::Claude && !held_commands {
            return Err(Error::Unsupported("background waits on this runtime".into()));
        }
        let snapshot = self.journal.snapshot();
        let mut done = HashSet::new();
        let mut matched = vec![];
        let mut unmatched = vec![];
        let mut changes = vec![];
        let candidates: HashMap<&str, &NativeTask> = snapshot
            .tasks
            .iter()
            .filter(|r| {
                r.pending_background()
                    && r.started_at
                        .as_ref()
                        .is_some_and(|at| at.stream_id == self.journal.id())
                    && r.source.path.is_empty()
                    && r.source.session_id.as_deref() == Some(snapshot.id.as_str())
            })
            .map(|r| (r.id.as_str(), r))
            .collect();
        for id in &req.task_ids {
            // The Antigravity adapter holds the turn itself. Its surfaced tool may have no shell ID.
            let selected = if held_commands {
                candidates.values().find(|row| req.started_at.get(id).is_none_or(|at| row.started_at.as_ref() == Some(at)))
            } else { candidates.get(id.as_str()) }.filter(|row| {
                req.started_at
                    .get(id)
                    .is_none_or(|at| row.started_at.as_ref() == Some(at))
            });
            let Some(row) = selected else {
                unmatched.push(id.clone());
                continue;
            };
            if !done.insert(id) {
                continue;
            }
            matched.push(id.clone());
            let mut work = row.background_work.clone().unwrap();
            if !held_commands && work.awaited != req.waiting {
                work.awaited = req.waiting;
                changes.push(mark(id, work));
            }
        }
        if req.waiting && matched.is_empty() {
            return Err(Error::Refused(BackgroundWaitRequest::UNMATCHED.into()));
        }
        if !changes.is_empty() {
            self.journal.push(
                Direction::Host,
                RecordKind::Frame,
                None,
                vec![],
                serde_json::json!({"type":"kar/background/wait"}),
                changes,
            );
        }
        Ok(BackgroundWaitReceipt { matched, unmatched })
    }
}
