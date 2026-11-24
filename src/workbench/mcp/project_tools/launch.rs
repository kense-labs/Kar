use super::*;
use crate::workbench::{task_folders, thread_state};

pub(super) async fn run(
    app: &Workbench,
    grant: &Grant,
    access: &Access,
    project: Uuid,
    chat: Uuid,
    input: &Value,
) -> Result<Value> {
    let title = string(input, "title")?.trim().to_owned();
    if title.chars().count() > 200 || title.contains(['\n', '\r', '\0']) {
        return Err(refused("invalid chat title"));
    }
    if chat == grant.chat {
        return Err(refused("launch ID belongs to the calling chat"));
    }
    let scratch = input
        .get("scratch")
        .map(|v| v.as_bool().ok_or_else(|| refused("invalid scratch flag")))
        .transpose()?
        .unwrap_or(false);
    let project = if scratch {
        if !access.scratch || input.get("project_id").is_some() || input.get("workspace").is_some()
        {
            return Err(refused("scratch launch needs its own grant and workspace"));
        }
        app.ensure_scratch()?.id
    } else {
        let p = app.project(project)?;
        if !access.sees(grant, &p) || p.metadata.deleted {
            return Err(refused("launch project is outside the grant"));
        }
        project
    };
    let parent = app.binding(grant.project, grant.chat)?;
    let p = app.project(project)?;
    let runtime = input
        .get("runtime")
        .map(|_| string(input, "runtime"))
        .transpose()?
        .unwrap_or(&parent.runtime)
        .to_owned();
    let model = input
        .get("model")
        .map(|_| string(input, "model").map(str::to_owned))
        .transpose()?
        .or_else(|| {
            (runtime == parent.runtime)
                .then(|| parent.model.clone())
                .flatten()
        });
    let attachments: Vec<Uuid> = input
        .get("attachments")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let attachments = attachments
        .into_iter()
        .map(|id| app.0.chats.visible_attachment(grant.chat, id))
        .collect::<Result<Vec<_>>>()?;
    let message = input
        .get("message")
        .map(|v| v.as_str().ok_or_else(|| refused("invalid launch message")))
        .transpose()?;
    let strategy = input
        .get("workspace")
        .map(|_| string(input, "workspace"))
        .transpose()?
        .unwrap_or("root");
    match strategy {
        "root" => {
            if ["base_ref", "branch", "directory", "start_from_origin"]
                .iter()
                .any(|k| input.get(k).is_some())
            {
                return Err(refused("worktree options require a worktree launch"));
            }
        }
        "existing_worktree" => {
            if ["base_ref", "branch", "start_from_origin"]
                .iter()
                .any(|k| input.get(k).is_some())
            {
                return Err(refused(
                    "an existing checkout does not take a base or a new branch",
                ));
            }
            let path = Path::new(string(input, "directory")?).canonicalize()?;
            app.workspace_at(&path)?;
            let repo = gix::open(&p.root).map_err(|e| refused(&e.to_string()))?;
            let work = gix::open(&path).map_err(|e| refused(&e.to_string()))?;
            if work.workdir().and_then(|d| d.canonicalize().ok()).as_ref() != Some(&path)
                || repo.common_dir().canonicalize()? != work.common_dir().canonicalize()?
            {
                return Err(refused("existing checkout does not belong to the project"));
            }
            app.bind_model_checkout(project, chat, &path)?;
        }
        "worktree" => {
            let dest = access.directory(Path::new(string(input, "directory")?))?;
            let branch = string(input, "branch")?.to_owned();
            let base = input
                .get("base_ref")
                .map(|_| string(input, "base_ref"))
                .transpose()?
                .unwrap_or("HEAD")
                .to_owned();
            let origin = input
                .get("start_from_origin")
                .map(|v| {
                    v.as_bool()
                        .ok_or_else(|| refused("invalid start_from_origin"))
                })
                .transpose()?
                .unwrap_or(
                    p.metadata
                        .settings
                        .get("newWorktreesStartFromOrigin")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                );
            let host = app.clone();
            tokio::task::spawn_blocking(move || {
                task_folders::handoff_task(&host, project, chat, dest, branch, base, origin)
                    .map(|_| ())
            })
            .await
            .map_err(|e| Error::Closed(e.to_string()))??;
        }
        _ => return Err(refused("unsupported launch workspace")),
    }
    if !grant.active() {
        return Err(refused("calling grant expired"));
    }
    let settings = crate::SessionSettings {
        effort: input
            .get("effort")
            .map(|_| string(input, "effort").map(str::to_owned))
            .transpose()?
            .or_else(|| {
                (runtime == parent.runtime)
                    .then(|| parent.settings.effort.clone())
                    .flatten()
            }),
        acp_mode: input
            .get("acp_mode")
            .map(|_| string(input, "acp_mode").map(str::to_owned))
            .transpose()?,
        ..Default::default()
    };
    let child = app
        .create_chat_with_settings(project, chat, &runtime, model, parent.approvals, settings)
        .await?;
    let title_id = crate::chat::stable_id([chat.as_bytes().as_slice(), b"model-launch-title"]);
    thread_state::model_rename(app.clone(), project, chat, title_id, title).await?;
    let mut result = json!({"chat":child,"project_id":project,"thread_id":chat,"link":format!("kar://projects/{project}/chats/{chat}")});
    if message.is_some() || !attachments.is_empty() {
        let input_id =
            crate::chat::stable_id([chat.as_bytes().as_slice(), b"model-launch-message"]);
        let copied = attachments
            .iter()
            .map(|a| {
                let id = crate::chat::stable_id([
                    chat.as_bytes().as_slice(),
                    a.id.as_bytes().as_slice(),
                    b"launch-attachment",
                ]);
                let bytes = app.0.chats.verified_bytes(a)?;
                app.0
                    .chats
                    .persist_attachment(chat, id, &a.name, &a.mime, a.kind, &bytes)
                    .map(|saved| saved.id)
            })
            .collect::<Result<Vec<_>>>()?;
        result["input"] = serde_json::to_value(app.0.chats.enqueue_attached(
            chat,
            input_id,
            message.unwrap_or_default(),
            &copied,
        )?)?;
        app.wake_queue();
    }
    Ok(result)
}
