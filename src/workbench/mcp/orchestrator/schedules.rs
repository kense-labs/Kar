use super::*;
pub(super) async fn call(app: &Workbench, c: &Caller, name: &str, mut req: Value) -> Result<Value> {
    let g = c.grant.as_ref().unwrap();
    if name == "kar_list_scheduled_tasks" {
        if req.as_object().is_none_or(|m| !m.is_empty()) {
            return Err(refused("list takes no arguments"));
        }
        return app.model_schedules(g.project, g.chat);
    }
    let fields = req
        .as_object_mut()
        .ok_or_else(|| refused("expected schedule arguments"))?;
    let id: Uuid = serde_json::from_value(
        fields
            .remove("command_id")
            .ok_or_else(|| refused("missing command ID"))?,
    )?;
    let gate = app.0.mcp.command_gate(id);
    let _guard = gate.lock().await;
    let defaults = if name == "kar_schedule_task" {
        let a: workflows::model_schedules::Save = serde_json::from_value(req.clone())?;
        let info = app.binding(g.project, g.chat)?;
        let scope = scope(c.policy.as_ref().unwrap()).unwrap();
        let (rt, model) = child_model(&info, scope, a.runtime.as_deref(), a.model.as_deref())?;
        Some((
            workflows::model_schedules::Binding {
                owner: g.chat,
                workspace: info.workspace,
                settings: info.settings,
                approvals: info.approvals,
                bind: false,
                fresh: true,
            },
            rt,
            model,
        ))
    } else {
        if fields.len() != 1 || !fields.contains_key("scheduled_task_id") {
            return Err(refused("expected scheduled_task_id"));
        }
        None
    };
    app.model_schedule_command(g.project, g.chat, id, name, req, defaults, || {
        authorize(&app.0.catalog.lock().unwrap(), c, true, true)
    })
}
