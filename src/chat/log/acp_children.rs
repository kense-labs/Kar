use super::{ChatUpdate, Log};
use crate::chat::Item;
use crate::{Event, Record, Result, TaskChange, wire::opencode};
use uuid::Uuid;

impl Log {
    fn child_owner(&self, child: &str) -> Option<Uuid> {
        self.state
            .turns
            .iter()
            .rev()
            .find(|turn| turn.child_sessions.contains_key(child))
            .map(|turn| turn.id)
    }

    fn bind_child(&mut self, id: Uuid, parent: &str, child: &str, finished: bool) -> Result<()> {
        if child == parent || child == self.state.info.native_id {
            return Ok(());
        }
        let Some(turn) = self.state.find(id) else {
            return Ok(());
        };
        if parent != self.state.info.native_id && !turn.child_sessions.contains_key(parent) {
            return Ok(());
        }
        match turn.child_sessions.get(child) {
            Some(existing) if existing != parent || !finished => return Ok(()),
            Some(_)
                if !turn.items.iter().any(|item| {
                    item.source.session_id.as_deref() == Some(child)
                        && match &item.item {
                            Item::Message { complete, .. } => !complete,
                            Item::Tool { activity, .. } => !activity.finished,
                            _ => false,
                        }
                }) =>
            {
                return Ok(());
            }
            None if finished => return Ok(()),
            _ => {}
        }
        self.commit(ChatUpdate::ChildSession {
            id,
            parent: parent.into(),
            child: child.into(),
            finished,
        })
    }

    fn call_owner(&self, source: &str, call: &str) -> Option<Uuid> {
        self.state
            .turns
            .iter()
            .rev()
            .find(|t| {
                t.items.iter().any(|item| {
                    item.source.session_id.as_deref() == Some(source)
                        && matches!(&item.item,Item::Tool{id,..} if id==call)
                })
            })
            .map(|t| t.id)
    }

    fn updated_call_owner(&self, source: &str, update: &serde_json::Value) -> Option<Uuid> {
        if update["sessionUpdate"] != "tool_call_update" {
            return None;
        }
        self.call_owner(source, update["toolCallId"].as_str()?)
    }

    // Cursor rebuilds tool call IDs as replay-<turn>-<step> during session/load.
    // Use the original child session binding to keep durable message and task owners stable.
    pub(super) fn cursor_source_path(&self, record: &Record) -> Option<Vec<String>> {
        let source = record.session_id.as_deref()?;
        if source == self.state.info.native_id {
            return None;
        }
        let task =
            self.state.tasks.iter().rev().find(|t| {
                t.id == source && t.details.child_session_id.as_deref() == Some(source)
            })?;
        let call = task.details.tool_call_id.as_ref()?;
        if task.source.path.is_empty()
            && task.source.session_id.as_deref() != Some(&self.state.info.native_id)
        {
            return None;
        }
        if record.agent_path.len() == task.source.path.len() + 1
            && record.agent_path.starts_with(&task.source.path)
            && record.agent_path.last() == Some(call)
        {
            return None;
        }
        let mut path = task.source.path.clone();
        path.push(call.clone());
        Some(path)
    }
    pub(super) fn reconcile_cursor_child(&mut self, record: &Record) -> Result<()> {
        let Some(parent) = record.session_id.as_deref() else {
            return Ok(());
        };
        let Some((child, change)) = record.events.iter().find_map(|e| match e {
            Event::Task { id, change } => Some((id.as_str(), change)),
            _ => None,
        }) else {
            return Ok(());
        };
        // TasksChanged precedes ChildSession in a live log. The saved task receipt
        // still identifies its original tool if the controller died between writes.
        let saved_call = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == child && task.source.session_id.as_deref() == Some(parent))
            .and_then(|task| task.details.tool_call_id.as_deref());
        let owner = self.child_owner(child).or_else(|| {
            let reported_call = match change {
                TaskChange::Started { details, .. } => details.tool_call_id.as_deref(),
                _ => None,
            };
            saved_call
                .or(reported_call)
                .and_then(|call| self.call_owner(parent, call))
        });
        let Some(owner) = owner else { return Ok(()) };
        self.bind_child(owner, parent, child, false)?;
        if self
            .state
            .find(owner)
            .and_then(|t| t.child_sessions.get(child))
            .map(String::as_str)
            != Some(parent)
        {
            return Ok(());
        }
        let previous = self
            .state
            .tasks
            .iter()
            .find(|task| {
                task.id == child
                    && task.source.session_id.as_deref() == Some(parent)
                    && task.source.path == record.agent_path
            })
            .map(|t| (t.status, t.ended_on_exit));
        match change {
            TaskChange::Started { .. } if previous.is_some() => return Ok(()),
            TaskChange::Ended { status, .. } => {
                self.bind_child(owner, parent, child, true)?;
                if previous.is_some_and(|(prior, on_exit)| {
                    !on_exit
                        && (prior == *status
                            || *status == crate::TaskStatus::Disconnected && prior.is_terminal())
                }) {
                    return Ok(());
                }
            }
            _ => {}
        }
        let tasks = crate::tasks::changes(&self.state.tasks, record, false);
        if !tasks.is_empty() {
            self.commit(ChatUpdate::TasksChanged { tasks })?;
        }
        Ok(())
    }

    pub(super) fn record_cursor_child(&mut self, record: &Record) -> Result<bool> {
        if record.native["method"] != "session/update" {
            return Ok(false);
        }
        let root = self.state.info.native_id.clone();
        let Some(source) = record.session_id.as_deref() else {
            return Ok(false);
        };
        let owner = if source == root {
            self.state.active().filter(|_| self.dispatched)
        } else {
            self.child_owner(source)
        };
        for event in &record.events {
            if let Event::Task { id: child, change } = event {
                match change {
                    TaskChange::Started { details, .. } => {
                        // A background call can announce its child after another root turn has begun.
                        let turn = details
                            .tool_call_id
                            .as_deref()
                            .and_then(|call| self.call_owner(source, call))
                            .or(owner);
                        if let Some(id) = turn {
                            self.bind_child(id, source, child, false)?;
                        }
                    }
                    TaskChange::Ended { .. } => {
                        if let Some(id) = self.child_owner(child) {
                            self.bind_child(id, source, child, true)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        let target = if source == root {
            match self.updated_call_owner(source, &record.native["params"]["update"]) {
                Some(id) if Some(id) != owner => Some(id),
                _ => return Ok(false),
            }
        } else {
            owner
        };
        if let Some(id) = target
            && let Some(frame) = self.ids.frame(self.state.info.protocol, &root, record)
        {
            self.commit(ChatUpdate::Frame { id, frame })?;
        }
        Ok(true)
    }

    // On 2.3+ the native parent's prompt may have ended before any of the child's work arrives.
    // Keep the durable binding until a spawning/follow-up tool explicitly selects it again.
    pub(super) fn record_opencode(&mut self, record: &Record, historical: bool) -> Result<bool> {
        let root = self.state.info.native_id.clone();
        let Some(source) = record.session_id.as_deref() else {
            return Ok(false);
        };
        let packet = &record.native;
        let is_extension = packet["method"] == "opencode/session/child_update";
        if !is_extension && packet["method"] != "session/update" {
            return Ok(false);
        }
        if historical && !self.dispatched || record.events.is_empty() {
            return Ok(true);
        }
        let active = self.state.active().filter(|_| self.dispatched);
        let update = &packet["params"]["update"];
        let owner = |source: &str, this: &Self| {
            if source == root {
                active
            } else {
                this.child_owner(source)
            }
        };
        if let Some(n) = opencode::notice(packet, Some(&root)) {
            if self.child_owner(n.child).is_none()
                && let Some(id) = owner(n.parent, self)
            {
                self.bind_child(id, n.parent, n.child, false)?;
            }
            if record
                .events
                .iter()
                .any(|e| matches!(e, Event::Task {id,change:TaskChange::Ended{..}} if id==n.child))
                && let Some(id) = self.child_owner(n.child)
            {
                self.bind_child(id, n.parent, n.child, true)?;
            }
        }
        let call_id = update["toolCallId"].as_str();
        let target = self
            .updated_call_owner(source, update)
            .or_else(|| owner(source, self));
        if let Some(id) = target {
            if let Some(child) = opencode::spawned(update) {
                self.bind_child(id, source, child, false)?;
            } else if matches!(
                update["sessionUpdate"].as_str(),
                Some("tool_call" | "tool_call_update")
            ) && matches!(update["status"].as_str(), Some("in_progress") | None)
                && let Some(child) = update
                    .pointer("/rawInput/sessionID")
                    .and_then(|v| v.as_str())
                && let Some(old) = self.child_owner(child).and_then(|id| self.state.find(id))
                && old
                    .child_sessions
                    .get(child)
                    .is_some_and(|parent| parent == source)
            {
                // A follow-up can emit child content before the subagent tool returns its result.
                let subagent = update["name"] == "subagent" || update["title"] == "subagent"
                    || self.state.find(id).is_some_and(|turn| turn.items.iter().any(|item|
                        item.source.session_id.as_deref() == Some(source)
                        && matches!(&item.item, Item::Tool{id, activity} if Some(id.as_str()) == call_id && activity.name.as_deref() == Some("subagent"))
                    ));
                if subagent {
                    self.bind_child(id, source, child, false)?;
                }
            }
        }
        if source == root && (target == active || target.is_none()) {
            return Ok(false);
        }
        if let Some(id) = target
            && let Some(frame) = self.ids.frame(self.state.info.protocol, &root, record)
        {
            self.commit(ChatUpdate::Frame { id, frame })?;
        }
        Ok(true)
    }
}
