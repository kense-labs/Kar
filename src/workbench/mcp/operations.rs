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
    project_wide: bool,
    targets: BTreeSet<Uuid>,
    /// Only listed native providers/models can be selected for an existing chat.
    models: BTreeMap<String, BTreeSet<String>>,
}
impl Scope {
    pub(super) fn validate(&self, app: &Workbench, p: Uuid) -> Result<()> {
        for target in &self.targets {
            app.binding(p, *target)?;
        }
        Ok(())
    }
}
pub(super) fn visible(db: &Catalog, c: &Caller, id: Uuid) -> bool {
    c.grant.as_ref().is_none_or(|g| {
        g.active()
            && Policy::current(db, g.project, g.chat) == c.policy.as_ref()
            && (g.chat == id || super::project_tools::launched_by(db, g.chat, id) || transfer::created_by(db,g.chat,id)
                || c.policy
                    .as_ref()
                    .and_then(|p| p.thread_tools.as_ref())
                    .is_some_and(|s| s.project_wide || s.targets.contains(&id)))
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
        || g.project != p
        || !visible(db, c, t)
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
    let p = project.unwrap_or(grant.project);
    let t = thread.unwrap_or(grant.chat);
    authorize(&app.0.catalog.lock().unwrap(), c, p, t, write)?;
    app.binding(p, t)?;
    Ok((p, t))
}
pub(super) fn may_run(app: &Workbench, p: Uuid, source: Uuid, target: Uuid) -> Result<()> {
    if source == target {
        return Ok(());
    }
    let from = app.binding(p, source)?;
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
    tool(
        "kar_thread_search",
        "Find matching threads within the granted project and thread scope.",
        json!({"query":s,"cursor":{"type":"integer"},"limit":{"type":"integer"}}),
        &["query"],
    );
    tool(
        "kar_thread_wait",
        "Wait for a change from the given saved chat cursor, returning its current turn and cursor.",
        json!({"after":{},"timeout_ms":{"type":"integer","maximum":30000}}),
        &[],
    );
    let write = !c.plan && p.executes() && scope.write;
    tool("kar_thread_transfer","Fork a conversation through a completed run or checkpoint, without changing files. Merge a fork's contributions into its parent on its next message.",
        json!({"action":{"enum":if write {vec!["fork","merge_back","status"]}else{vec!["status"]}},"source_point":{"type":"object"},"target_thread_id":id,"title":s,"policy":{"enum":["native","prefer_native"]},"command_id":id}),&["action"]);
    tool(
        "kar_pending_request",
        "Read outstanding user questions or answer an exact question occurrence.",
        json!({"action":{"enum":if write{vec!["list","read","respond"]}else{vec!["list","read"]}},"id":{},"cursor":{"type":"object"},"reply":{},"command_id":id}),
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
            json!({"command_id":id,"text":s,"mode":{"enum":["auto","queue","steer","restart"]},"turn_id":id,"request_id":id,"attachments":{"type":"array","items":id}}),
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
            json!({"task_id":s,"started_at":{"type":"object"},"command_id":id}),
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
    transfer_turn:Option<Uuid>,
    name: String,
    args: Value,
    input: Uuid,
    result: Option<std::result::Result<Value, String>>,
}
pub(super) async fn call(app: Workbench, c: Caller, name: &str, mut input: Value) -> Result<Value> {
    let name = name.to_owned();
    let fields = input.as_object_mut().ok_or_else(denied)?;
    let project = parse(fields.remove("project_id").unwrap_or_default())?;
    let thread = parse(fields.remove("thread_id").unwrap_or_default())?;
    let read = (name=="kar_thread_transfer" && fields.get("action").is_some_and(|s|s=="status")) || matches!(name.as_str(), "kar_thread_search" | "kar_thread_wait")
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
        if name=="kar_thread_transfer"{ return transfer::status(&app,&c,p,t); }
        return reads::call(&app, &c, p, t, &name, input).await;
    }
    let id = parse(fields.remove("command_id").unwrap_or_default())?;
    let source = c.grant.as_ref().unwrap().chat;
    if matches!(
        name.as_str(),
        "kar_thread_send" | "kar_thread_configure" | "kar_pending_request"
    ) {
        may_run(&app, p, source, t)?;
    }
    // The actor finishes a reserved native control even if its response transport goes away.
    tokio::spawn(async move {
        let gate = app.0.mcp.command_gate(id);
        let _guard = gate.lock().await;
        let prior=app.0.catalog.lock().unwrap().model_thread_commands.contains_key(&id);
        let transfer_turn=if name=="kar_thread_transfer" && !prior { Some(transfer::select(&app.0.chats.history(t)?,&input)?) }else{None};
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
                "kar_thread_send" | "kar_thread_transfer"
                    | "kar_thread_update"
                    | "kar_thread_organize"
                    | "kar_thread_configure"
            )
        {
            return Err(Error::Refused(
                "original native control has no confirmed receipt".into(),
            ));
        }
        let out = if rec.name=="kar_thread_transfer"{transfer::apply(&app,&c,id,&rec).await}else{writes::call(&app, &c, id, &rec).await};
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
