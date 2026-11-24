//! Grants bind local orchestration to the calling Kar chat. The catalog owns task
//! identity; the chat queue owns input delivery, including recovery from a lost reply.
mod schedules;
mod tasks;
mod threads;
use super::*;
use crate::{
    Error, Result,
    workbench::{Catalog, workflows},
};
use serde::{Deserialize, Serialize};
use toolkit::Tool;

fn refused(msg: &str) -> Error {
    Error::Refused(msg.to_owned())
}
fn scope(policy: &Policy) -> Option<&operations::Scope> {
    policy.thread_tools.as_ref()
}
fn authorize(db: &Catalog, c: &Caller, schedules: bool, write: bool) -> Result<()> {
    let g = c
        .grant
        .as_ref()
        .ok_or_else(|| refused("requires a working model"))?;
    let p = Policy::current(db, g.project, g.chat)
        .ok_or_else(|| refused("model tool grant is unavailable"))?;
    if !g.active()
        || Some(p) != c.policy.as_ref()
        || !scope(p).is_some_and(|s| if schedules { s.schedule } else { s.delegate })
        || write && (c.plan || !p.executes())
    {
        return Err(refused("orchestration grant does not allow this action"));
    }
    Ok(())
}
pub(super) fn handles(name: &str) -> bool {
    matches!(
        name,
        "kar_delegate_task"
            | "kar_task_status"
            | "kar_task_cancel"
            | "kar_create_threads"
            | "kar_schedule_task"
            | "kar_list_scheduled_tasks"
            | "kar_delete_scheduled_task"
            | "kar_run_scheduled_task_now"
    )
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    let Some(p) = &c.policy else { return vec![] };
    let scope = scope(p);
    let write = p.executes() && !c.plan;
    let id = json!({"type":"string","format":"uuid"});
    let string = json!({"type":"string"});
    let flag = json!({"type":"boolean"});
    let mut tools = vec![];
    let mut add = |name, description, properties, required: &'static [&'static str]| {
        tools.push(Tool {
            name,
            description,
            properties,
            required,
        })
    };
    if write
        && super::project_tools::tools(c)
            .iter()
            .any(|t| t.name == "kar_thread_launch")
    {
        add(
            "kar_create_threads",
            "Create top-level chats in the current checkout. Each may start with a prompt.",
            json!({"command_id":id,"threads":{"type":"array","minItems":1,"maxItems":12,"items":{"type":"object","properties":{"title":string,"prompt":string,"runtime":string,"model":string,"requested_output_schema":{"type":"object"}},"additionalProperties":false}}}),
            &["command_id", "threads"],
        );
    }
    if scope.is_some_and(|s| s.delegate) {
        add(
            "kar_task_status",
            "Read a delegated task, including child work and structured results.",
            json!({"task_id":id}),
            &["task_id"],
        );
        if write {
            let mut task = json!({"task":string,"title":string,"runtime":string,"model":string,"isolation":{"enum":["shared","worktree"]},"branch":string,"requested_output_schema":{"type":"object"}});
            if !scope.unwrap().remote_projects.is_empty(){
                task["remote"]=json!({"type":"object","properties":{"environment_id":id,"project_id":id},"required":["environment_id","project_id"],"additionalProperties":false});
            }
            let mut one = task.clone();
            one["command_id"] = id.clone();
            add(
                "kar_delegate_task",
                "Start a child with an explicit task and no parent conversation history. The result wakes this chat.",
                one,
                &["task", "command_id"],
            );
            add(
                "kar_task_cancel",
                "Cancel unfinished work in this delegated chat and its descendants.",
                json!({"task_id":id}),
                &["task_id"],
            );
        }
    }
    if scope.is_some_and(|s| s.schedule) {
        add(
            "kar_list_scheduled_tasks",
            "Read scheduled work owned by this chat, with the next scheduled time.",
            json!({}),
            &[],
        );
        if write {
            add(
                "kar_schedule_task",
                "Create or edit a durable agent schedule. Optional webhook signing secrets use a private reference.",
                json!({"command_id":id,"scheduled_task_id":id,"prompt":string,"title":string,"schedule":{"type":"object"},"runtime":string,"model":string,"enabled":flag,"fresh_session":flag,"bind_to_current_thread":flag,"webhook_secret_ref":id}),
                &["command_id"],
            );
            add(
                "kar_delete_scheduled_task",
                "Remove a task's schedule and stop its current run.",
                json!({"command_id":id,"scheduled_task_id":id}),
                &["command_id", "scheduled_task_id"],
            );
            add(
                "kar_run_scheduled_task_now",
                "Trigger one run without altering the next regular firing time.",
                json!({"command_id":id,"scheduled_task_id":id}),
                &["command_id", "scheduled_task_id"],
            );
        }
    }
    tools
}
pub(super) async fn call(app: Workbench, c: Caller, name: &str, req: Value) -> Result<Value> {
    if name == "kar_create_threads" {
        return threads::create(&app, &c, req).await;
    }
    let schedule = name.contains("scheduled_task") || name == "kar_schedule_task";
    let write = !matches!(name, "kar_task_status" | "kar_list_scheduled_tasks");
    authorize(&app.0.catalog.lock().unwrap(), &c, schedule, write)?;
    if schedule {
        schedules::call(&app, &c, name, req).await
    } else {
        tasks::call(&app, &c, name, req).await
    }
}
pub(super) use tasks::owned;
pub(in crate::workbench) use tasks::milestone_parent;
pub(in crate::workbench) use tasks::peer_routes;
pub(in crate::workbench) use tasks::{Task, recover};
pub(in crate::workbench) fn scheduled_execution(
    app: &Workbench,
    p: Uuid,
    c: Uuid,
    info: &crate::chat::ChatInfo,
    rt: &str,
    model: Option<&str>,
) -> bool {
    let db = app.0.catalog.lock().unwrap();
    Policy::current(&db, p, c)
        .filter(|p| p.executes())
        .and_then(scope)
        .is_some_and(|s| s.schedule && child_model(info, s, Some(rt), model).is_ok())
}
fn child_model(
    info: &crate::chat::ChatInfo,
    scope: &operations::Scope,
    rt: Option<&str>,
    model: Option<&str>,
) -> Result<(String, Option<String>)> {
    let rt = rt.unwrap_or(&info.runtime);
    let model = model
        .map(str::to_owned)
        .or_else(|| (rt == info.runtime).then(|| info.model.clone()).flatten());
    if (rt != info.runtime || model != info.model)
        && !scope
            .models
            .get(rt)
            .is_some_and(|ms| model.as_ref().is_some_and(|m| ms.contains(m)))
    {
        return Err(refused("child model is outside the granted selection"));
    }
    Ok((rt.into(), model))
}
