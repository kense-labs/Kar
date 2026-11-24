use super::*;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    command_id: Uuid,
    threads: Vec<Thread>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Thread {
    title: Option<String>,
    prompt: Option<String>,
    runtime: Option<String>,
    model: Option<String>,
    requested_output_schema: Option<Value>,
}
pub(super) async fn create(app: &Workbench, c: &Caller, input: Value) -> Result<Value> {
    let grant = c
        .grant
        .as_ref()
        .ok_or_else(|| refused("requires working model"))?;
    let q: Batch = serde_json::from_value(input.clone())?;
    if q.threads.is_empty() || q.threads.len() > 12 {
        return Err(refused("invalid batch length"));
    }
    let cwd = app.binding(grant.project, grant.chat)?.workspace;
    let root = app.project(grant.project)?.root;
    app.update(|db| {
        if c.plan
            || !grant.active()
            || Policy::current(db, grant.project, grant.chat) != c.policy.as_ref()
            || !super::super::project_tools::tools(c)
                .iter()
                .any(|t| t.name == "kar_thread_launch")
        {
            return Err(refused("batch requires a current launch grant"));
        }
        let binding = (grant.project, grant.chat, input);
        if db
            .model_thread_batches
            .get(&q.command_id)
            .is_some_and(|r| r != &binding)
        {
            return Err(refused("batch command already used"));
        }
        db.model_thread_batches.insert(q.command_id, binding);
        Ok(())
    })?;
    let mut rows = vec![];
    for (i, r) in q.threads.into_iter().enumerate() {
        let child = crate::chat::stable_id([
            q.command_id.as_bytes().as_slice(),
            &(i as u64).to_be_bytes(),
            b"top-level chat",
        ]);
        let mut args = json!({"command_id":child,"title":r.title.unwrap_or_else(||r.prompt.as_deref().unwrap_or("New chat").chars().take(80).collect()),"project_id":grant.project});
        if root != cwd {
            args["workspace"] = "existing_worktree".into();
            args["directory"] = json!(cwd);
        }
        if let Some(v) = r.prompt {
            args["message"] = v.into();
        }
        if let Some(v) = r.runtime {
            args["runtime"] = v.into();
        }
        if let Some(v) = r.model {
            args["model"] = v.into();
        }
        if let Some(v) = r.requested_output_schema {
            args["requested_output_schema"] = v;
        }
        let res = super::super::project_tools::call(app, c, "kar_thread_launch", args).await?;
        rows.push(res);
    }
    Ok(json!({"threads":rows}))
}
