//! Thread operations require a host-granted scope and retain the native turn or request identity.
mod reads;
mod transfer;
mod writes;
use super::*;
use crate::{Error, Result, workbench::Catalog};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use toolkit::Tool;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Scope {
    pub(super) write: bool,
    pub(super) delegate: bool,
    pub(super) remote_projects: BTreeMap<Uuid, BTreeSet<Uuid>>,
    pub(super) schedule: bool,
    project_wide: bool,
    targets: BTreeSet<Uuid>,
    /// Only listed native providers/models can be selected for an existing chat.
    pub(super) models: BTreeMap<String, BTreeSet<String>>,
}
impl Scope {
    pub(super) fn validate(&self, app: &Workbench, p: Uuid) -> Result<()> {
        if !self.remote_projects.is_empty()
            && (!self.delegate
                || self.remote_projects.len() > 32
                || self
                    .remote_projects
                    .values()
                    .any(|set| set.is_empty() || set.len() > 64))
        {
            return Err(Error::Refused(
                "remote projects must be explicitly scoped to a delegation grant".into(),
            ));
        }
        for target in &self.targets {
            app.binding(p, *target)?;
        }
        Ok(())
    }
}
fn child_project(db: &Catalog, caller: Uuid, chat: Uuid) -> Option<Uuid> {
    super::project_tools::launched_project(db, caller, chat)
        .or_else(|| transfer::fork_project(db, caller, chat))
        .or_else(|| super::orchestrator::owned(db, caller, chat))
}
pub(super) fn list_project(
    app: &Workbench,
    c: &Caller,
    asked: Option<Uuid>,
) -> Result<Option<Uuid>> {
    let Some(g) = &c.grant else { return Ok(asked) };
    let p = asked.unwrap_or(g.project);
    let db = app.0.catalog.lock().unwrap();
    if !g.active() || Policy::current(&db, g.project, g.chat) != c.policy.as_ref() {
        return Err(denied());
    }
    if p != g.project
        && !db
            .model_project_commands
            .keys()
            .chain(db.model_thread_commands.keys())
            .any(|id| child_project(&db, g.chat, *id) == Some(p))
    {
        return Err(denied());
    }
    Ok(Some(p))
}
pub(super) fn visible(db: &Catalog, c: &Caller, project: Uuid, id: Uuid) -> bool {
    c.grant.as_ref().is_none_or(|g| {
        g.active()
            && Policy::current(db, g.project, g.chat) == c.policy.as_ref()
            && (child_project(db, g.chat, id) == Some(project)
                || g.project == project
                    && (g.chat == id
                        || c.policy
                            .as_ref()
                            .and_then(|p| p.thread_tools.as_ref())
                            .is_some_and(|s| s.project_wide || s.targets.contains(&id))))
    })
}
fn denied() -> Error {
    Error::Refused("thread operation is outside this model's current grant".into())
}
fn authorize(db: &Catalog, c: &Caller, p: Uuid, t: Uuid, write: bool) -> Result<()> {
    let Some(g) = &c.grant else {
        return if write && c.plan {
            Err(denied())
        } else {
            Ok(())
        };
    };
    if !g.active()
        || !visible(db, c, p, t)
        || db.archived.contains(&g.chat)
        || Policy::current(db, g.project, g.chat) != c.policy.as_ref()
    {
        return Err(denied());
    }
    if write
        && (c.plan
            || !c
                .policy
                .as_ref()
                .is_some_and(|p| p.executes() && p.thread_tools.as_ref().is_some_and(|s| s.write)))
    {
        return Err(denied());
    }
    Ok(())
}
pub(super) fn target(
    app: &Workbench,
    c: &Caller,
    project: Option<Uuid>,
    thread: Option<Uuid>,
    write: bool,
) -> Result<(Uuid, Uuid)> {
    let Some(grant) = c.grant.as_ref() else {
        return toolkit::target(app, c, project, thread);
    };
    let t = thread.unwrap_or(grant.chat);
    let p = project
        .or_else(|| child_project(&app.0.catalog.lock().unwrap(), grant.chat, t))
        .unwrap_or(grant.project);
    authorize(&app.0.catalog.lock().unwrap(), c, p, t, write)?;
    app.binding(p, t)?;
    Ok((p, t))
}
pub(super) fn may_run(
    app: &Workbench,
    p: Uuid,
    source_project: Uuid,
    source: Uuid,
    target: Uuid,
) -> Result<()> {
    if source == target {
        return Ok(());
    }
    let from = app.binding(source_project, source)?;
    {
        let db = app.0.catalog.lock().unwrap();
        let allowed = Policy::current(&db, source_project, source);
        if let Some(target) = Policy::current(&db, p, target)
            && target != &Policy::default()
            && allowed != Some(target)
        {
            return Err(Error::Refused(
                "target thread has independent model-tool permissions".into(),
            ));
        }
    }
    let to = app.binding(p, target)?;
    let (a, b) = (&from.settings, &to.settings);
    if from.approvals == crate::ApprovalMode::Deny && to.approvals != crate::ApprovalMode::Deny
        || a.acp_mode != b.acp_mode
        || b.pi_project_trusted == Some(true) && a.pi_project_trusted != Some(true)
        || b.pi_allow_project_package_install && !a.pi_allow_project_package_install
        || !a
            .disallowed_tools
            .iter()
            .all(|v| b.disallowed_tools.contains(v))
        || !a
            .exclude_user_configuration
            .is_subset(&b.exclude_user_configuration)
        || !b
            .additional_directories
            .iter()
            .all(|v| a.additional_directories.contains(v))
    {
        return Err(Error::Refused(
            "target thread has broader execution permissions".into(),
        ));
    }
    Ok(())
}
pub(in crate::workbench) fn handles(name: &str) -> bool {
    matches!(
        name,
        "kar_thread_search"
            | "kar_thread_wait"
            | "kar_thread_transfer"
            | "kar_thread_update"
            | "kar_thread_organize"
            | "kar_thread_send"
            | "kar_thread_interrupt"
            | "kar_thread_configure"
            | "kar_pending_request"
            | "kar_workflow_stop"
    )
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    if c.grant.is_none() {
        if c.plan {
            return vec![];
        }
        let id = json!({"type":"string","format":"uuid"});
        let s = json!({"type":"string"});
        return vec![Tool {
            name: "kar_thread_send",
            description: "Send text and saved attachments through the target thread's FIFO. Steer or restart a specific active turn.",
            properties: json!({"project_id":id,"thread_id":id,"environment_id":id,"command_id":id,"text":s,"mode":{"enum":["auto","queue","steer","restart"]},"turn_id":id,"request_id":id,"attachments":{"type":"array","items":id}}),
            required: &["command_id", "text"],
        }];
    }
    let Some(p) = &c.policy else {
        return vec![];
    };
    let Some(scope) = &p.thread_tools else {
        return vec![];
    };
    let id = json!({"type":"string","format":"uuid"});
    let s = json!({"type":"string"});
    let mut out = Vec::new();
    let mut tool = |name, description, params: Value, required: &'static [&'static str]| {
        let mut fields = json!({"project_id":id,"thread_id":id});
        fields
            .as_object_mut()
            .unwrap()
            .extend(params.as_object().unwrap().clone());
        out.push(Tool {
            name,
            description,
            properties: fields,
            required,
        });
    };
    let cursor_schema = json!({
        "type": ["object", "string"],
        "properties": {
            "stream_id": id,
            "seq": {"type": "integer", "minimum": 0},
            "after_seq": {"type": "integer", "minimum": 0}
        }
    });
    let source_point_schema = json!({
        "type": "object",
        "properties": {
            "type": {"enum": ["latest_stable", "run", "checkpoint"]},
            "turn_id": id
        },
        "required": ["type"],
        "additionalProperties": false
    });
    let reply_schema = json!({
        "type": "object",
        "properties": {
            "kind": {"enum": ["answers", "decline", "cancel"]},
            "answers": {"type": "object", "additionalProperties": {"type": "array", "items": s}},
            "notes": {"type": "object", "additionalProperties": s}
        }
    });
    tool(
        "kar_thread_search",
        "Find matching threads within the granted project and thread scope.",
        json!({"query":s,"cursor":{"type":"integer"},"limit":{"type":"integer"}}),
        &["query"],
    );
    tool(
        "kar_thread_wait",
        "Wait for a change from the given saved chat cursor, returning its current turn and cursor.",
        json!({"after":cursor_schema,"timeout_ms":{"type":"integer","maximum":30000}}),
        &[],
    );
    let write = !c.plan && p.executes() && scope.write;
    tool(
        "kar_thread_transfer",
        "Fork a conversation through a completed run or checkpoint, without changing files. Merge a fork's contributions into its parent on its next message.",
        json!({"action":{"enum":if write {vec!["fork","merge_back","status"]}else{vec!["status"]}},"source_point":source_point_schema,"target_thread_id":id,"title":s,"policy":{"enum":["native","prefer_native"]},"command_id":id}),
        &["action"],
    );
    tool(
        "kar_pending_request",
        "Read outstanding user questions or answer an exact question occurrence.",
        json!({"action":{"enum":if write{vec!["list","read","respond"]}else{vec!["list","read"]}},"id":{"type":["string","integer"]},"cursor":cursor_schema,"reply":reply_schema,"command_id":id}),
        &["action"],
    );
    tool(
        "kar_thread_configure",
        "Read the current selection or switch an idle thread to a granted model and provider.",
        json!({"action":{"enum":if write{vec!["read","set"]}else{vec!["read"]}},"runtime":s,"model":s,"command_id":id}),
        &["action"],
    );
    if write {
        tool(
            "kar_thread_update",
            "Rename a thread or regenerate its title from the first user message.",
            json!({"action":{"enum":["rename","regenerate"]},"title":s,"command_id":id}),
            &["action", "command_id"],
        );
        tool(
            "kar_thread_organize",
            "Change a thread's pin, snooze, settlement, visit, archive or read state.",
            json!({"action":{"enum":["pin","unpin","snooze","unsnooze","settle","unsettle","archive","unarchive","mark_unread","visit"]},"until_ms":{"type":"integer"},"order":s,"expected_turn":id,"visited_ms":{"type":"integer"},"command_id":id}),
            &["action", "command_id"],
        );
        tool(
            "kar_thread_send",
            "Send text and saved attachments through the target thread's FIFO. Steer or restart a specific active turn.",
            json!({"environment_id":id,"command_id":id,"text":s,"mode":{"enum":["auto","queue","steer","restart"]},"turn_id":id,"request_id":id,"attachments":{"type":"array","items":id}}),
            &["command_id", "text"],
        );
        tool(
            "kar_thread_interrupt",
            "Interrupt only the named active turn.",
            json!({"turn_id":id,"request_id":id,"command_id":id}),
            &["turn_id", "command_id"],
        );
        tool(
            "kar_workflow_stop",
            "Stop the exact native Claude workflow occurrence shown in thread activity.",
            json!({"task_id":s,"started_at":cursor_schema,"command_id":id}),
            &["task_id", "started_at", "command_id"],
        );
    }
    out
}
fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    Ok(serde_json::from_value(v)?)
}
#[derive(Clone, Serialize, Deserialize)]
pub(in crate::workbench) struct Record {
    project: Uuid,
    source: Uuid,
    target: Uuid,
    #[serde(default)]
    transfer_turn: Option<Uuid>,
    name: String,
    args: Value,
    input: Uuid,
    result: Option<std::result::Result<Value, String>>,
}
pub(super) async fn call(app: Workbench, c: Caller, name: &str, mut input: Value) -> Result<Value> {
    if name == "kar_thread_send"
        && let Some(raw_env) = input
            .as_object_mut()
            .and_then(|m| m.remove("environment_id"))
        && !raw_env.is_null()
    {
        let env_id: Uuid = parse(raw_env)?;
        let local_id = super::collaboration_tools::environment::id(&app)?;
        if env_id != local_id {
            return toolkit::forward_remote_mcp(&app, &c, env_id, name, input, true).await;
        }
    }
    // External owner callers (`c.grant.is_none()`) can invoke `kar_thread_send` directly
    // (including when forwarded from a linked peer Kar environment!).
    if name == "kar_thread_send" && c.grant.is_none() {
        if c.plan {
            return Err(denied());
        }
        let fields = input.as_object_mut().ok_or_else(denied)?;
        let project: Option<Uuid> = parse(fields.remove("project_id").unwrap_or_default())?;
        let thread: Option<Uuid> = parse(fields.remove("thread_id").unwrap_or_default())?;
        let cmd: Uuid = parse(fields.remove("command_id").unwrap_or_default())?;
        let (p, t) = toolkit::target(&app, &c, project, thread)?;
        let text = fields
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(denied)?
            .to_owned();
        let attachments: Vec<Uuid> = fields
            .get("attachments")
            .cloned()
            .map(parse)
            .transpose()?
            .unwrap_or_default();
        let receipt = app.0.chats.enqueue_attached(t, cmd, text, &attachments)?;
        app.wake_queue();
        return Ok(json!({
            "project_id": p,
            "thread_id": t,
            "input_id": cmd,
            "command_id": cmd,
            "delivery": "queued",
            "receipt": receipt,
        }));
    }
    let name = name.to_owned();
    let fields = input.as_object_mut().ok_or_else(denied)?;
    let project = parse(fields.remove("project_id").unwrap_or_default())?;
    let thread = parse(fields.remove("thread_id").unwrap_or_default())?;
    let read = (name == "kar_thread_transfer"
        && fields.get("action").is_some_and(|s| s == "status"))
        || matches!(name.as_str(), "kar_thread_search" | "kar_thread_wait")
        || matches!(
            name.as_str(),
            "kar_thread_configure" | "kar_pending_request"
        ) && matches!(
            fields.get("action").and_then(Value::as_str),
            Some("list" | "read")
        );
    if c.grant.is_none()
        || c.policy
            .as_ref()
            .and_then(|p| p.thread_tools.as_ref())
            .is_none()
    {
        return Err(denied());
    }
    let (p, t) = target(&app, &c, project, thread, !read)?;
    if read {
        if name == "kar_thread_transfer" {
            return transfer::status(&app, &c, p, t);
        }
        return reads::call(&app, &c, p, t, &name, input).await;
    }
    let id = parse(fields.remove("command_id").unwrap_or_default())?;
    let source = c.grant.as_ref().unwrap().chat;
    if matches!(
        name.as_str(),
        "kar_thread_send" | "kar_thread_configure" | "kar_pending_request"
    ) {
        may_run(&app, p, c.grant.as_ref().unwrap().project, source, t)?;
    }
    // The actor finishes a reserved native control even if its response transport goes away.
    tokio::spawn(async move {
        let gate = app.0.mcp.command_gate(id);
        let _guard = gate.lock().await;
        let prior = app
            .0
            .catalog
            .lock()
            .unwrap()
            .model_thread_commands
            .contains_key(&id);
        let transfer_turn = if name == "kar_thread_transfer" && !prior {
            Some(transfer::select(&app.0.chats.history(t)?, &input)?)
        } else {
            None
        };
        let (rec, previous) = app.update(|db| {
            authorize(db, &c, p, t, true)?;
            if let Some(old) = db.model_thread_commands.get(&id) {
                if old.project != p
                    || old.source != source
                    || old.target != t
                    || old.name != name
                    || old.args != input
                {
                    return Err(Error::Refused("thread command ID already used".into()));
                }
                return Ok((old.clone(), true));
            }
            let record = Record {
                project: p,
                source,
                target: t,
                transfer_turn,
                name,
                args: input,
                input: Uuid::new_v4(),
                result: None,
            };
            db.model_thread_commands.insert(id, record.clone());
            Ok((record, false))
        })?;
        if let Some(out) = rec.result.clone() {
            return out.map_err(Error::Refused);
        }
        if previous
            && !matches!(
                rec.name.as_str(),
                "kar_thread_send"
                    | "kar_thread_transfer"
                    | "kar_thread_update"
                    | "kar_thread_organize"
                    | "kar_thread_configure"
            )
        {
            return Err(Error::Refused(
                "original native control has no confirmed receipt".into(),
            ));
        }
        let out = if rec.name == "kar_thread_transfer" {
            transfer::apply(&app, &c, id, &rec).await
        } else {
            writes::call(&app, &c, id, &rec).await
        };
        let saved = out.as_ref().map(Clone::clone).map_err(ToString::to_string);
        app.update(|db| {
            let row = db.model_thread_commands.get_mut(&id).ok_or_else(denied)?;
            if row.result.is_none() {
                row.result = Some(saved);
            }
            Ok(())
        })?;
        out
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))?
}
