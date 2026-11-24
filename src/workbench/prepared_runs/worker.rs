use super::*;
pub(super) fn schedule(app: &Workbench, job: &Job) {
    if matches!(job.stage, Stage::Failed | Stage::Released)
        || (job.stage == Stage::Ready && job.input.defer_release)
    {
        return;
    }
    let id = job.run;
    if !app.0.run_workers.lock().unwrap().insert(id) {
        return;
    }
    let app = app.clone();
    let (p, c) = (job.project, job.chat);
    tokio::spawn(async move {
        let saved = match drive(&app, p, c, id).await {
            Ok(()) => true,
            Err(error) => change(&app, id, |j| {
                if !matches!(j.stage, Stage::Failed | Stage::Released) {
                    j.advance(Stage::Failed, Some(error.to_string()));
                }
                Ok(())
            })
            .is_ok(),
        };
        app.0.run_workers.lock().unwrap().remove(&id);
        // An action can start a new attempt during the worker's final hand-off.
        if saved && let Ok(row) = get(&app, p, c, id) {
            schedule(&app, &row);
        }
    });
}
async fn drive(app: &Workbench, p: Uuid, c: Uuid, r: Uuid) -> Result<()> {
    loop {
        let j = get(app, p, c, r)?;
        authorize(&app.0.catalog.lock().unwrap(), &j)?;
        match j.stage {
            Stage::Worktree => {
                let host = app.clone();
                let row = j.clone();
                let cwd = tokio::task::spawn_blocking(move || checkout(&host, &row))
                    .await
                    .map_err(|e| Error::Closed(e.to_string()))??;
                let script = if matches!(j.input.workspace, Workspace::Root) {
                    None
                } else {
                    let project = app.project(p)?;
                    project
                        .metadata
                        .settings
                        .get("scripts")
                        .and_then(Value::as_array)
                        .and_then(|a| a.iter().find(|s| s["runOnWorktreeCreate"] == true))
                        .and_then(|s| {
                            Some(Script {
                                terminal: Uuid::new_v4(),
                                command: s["command"].as_str()?.into(),
                                asynchronous: s["async"].as_bool().unwrap_or(false),
                            })
                        })
                };
                change(app, r, |j| {
                    j.cwd = Some(cwd);
                    j.script = script;
                    j.advance(Stage::Setup, None);
                    Ok(())
                })?;
            }
            Stage::Setup => {
                if let Some(s) = &j.script {
                    let cwd = j.cwd.as_deref().ok_or_else(missing)?;
                    let current = app.project(p)?;
                    let allowed = current
                        .metadata
                        .settings
                        .get("scripts")
                        .and_then(Value::as_array)
                        .is_some_and(|scripts| {
                            scripts.iter().any(|v| {
                                v["runOnWorktreeCreate"] == true
                                    && v["command"].as_str() == Some(&s.command)
                            })
                        });
                    if !allowed {
                        return Err(Error::Refused(
                            "project setup changed; retry this run to use the current script"
                                .into(),
                        ));
                    }
                    app.launch_setup_terminal(p, cwd, s.terminal, &s.command)?;
                    if !s.asynchronous {
                        app.wait_setup_terminal(p, s.terminal).await?;
                    }
                }
                change(app, r, |j| {
                    j.advance(Stage::Ready, None);
                    Ok(())
                })?;
            }
            Stage::Ready if !j.input.defer_release => {
                change(app, r, |j| {
                    if j.stage == Stage::Ready {
                        j.advance(Stage::Releasing, None);
                    }
                    Ok(())
                })?;
            }
            Stage::Releasing => {
                let result = release(app, &j).await?;
                change(app, r, |j| {
                    j.result = Some(result);
                    j.advance(Stage::Released, None);
                    Ok(())
                })?;
            }
            _ => return Ok(()),
        }
    }
}
fn checkout(app: &Workbench, j: &Job) -> Result<PathBuf> {
    let p = app.project(j.project)?;
    let cwd = match &j.input.workspace {
        Workspace::Root if j.input.launch.is_some() => p.root,
        Workspace::Root => app.binding(j.project, j.chat)?.workspace,
        Workspace::Existing { directory } => {
            let path = directory.canonicalize()?;
            app.workspace_at(&path)?;
            let repo = gix::open(&p.root).map_err(|e| Error::Refused(e.to_string()))?;
            let wt = gix::open(&path).map_err(|e| Error::Refused(e.to_string()))?;
            if wt.workdir().and_then(|w| w.canonicalize().ok()).as_ref() != Some(&path)
                || repo.common_dir().canonicalize()? != wt.common_dir().canonicalize()?
            {
                return Err(denied());
            }
            app.bind_model_checkout(j.project, j.chat, &path)?;
            path
        }
        Workspace::Worktree {
            directory,
            branch,
            base,
            from_origin,
        } => {
            if let Some(caller) = &j.caller {
                caller.access.check_directory(directory)?;
            }
            task_folders::handoff_task(
                app,
                j.project,
                j.chat,
                directory.clone(),
                branch.clone(),
                base.clone(),
                *from_origin,
            )?
            .0
            .project_path
        }
    };
    Ok(cwd)
}
async fn release(app: &Workbench, j: &Job) -> Result<Value> {
    let (p, c, r) = (j.project, j.chat, j.run);
    if let Some(launch) = &j.input.launch {
        app.create_chat_as(
            p,
            c,
            &launch.runtime,
            launch.model.clone(),
            launch.approvals,
            launch.settings.clone(),
            j.caller.as_ref().map(|a| a.chat),
            Some(r),
        )
        .await?;
    }
    let info = app.binding(p, c)?;
    if j.cwd.as_ref() != Some(&info.workspace)
        && !(j.input.launch.is_some() && matches!(j.input.workspace, Workspace::Root))
    {
        return Err(Error::Refused(
            "prepared workspace changed before dispatch".into(),
        ));
    }
    if let Some(title) = &j.input.title {
        let cmd = crate::chat::stable_id([c.as_bytes().as_slice(), b"model-launch-title"]);
        thread_state::model_rename(app.clone(), p, c, cmd, title.clone()).await?;
    }
    let project_chat = ProjectChat {
        project: p,
        metadata: app.thread_metadata(c),
        info,
        archived: app.archived(c),
    };
    let mut result = json!({"chat":project_chat,"thread_id":c,"project_id":p,"run_id":r,"link":format!("kar://projects/{p}/chats/{c}")});
    if j.input.message.is_some() || !j.input.attachments.is_empty() {
        if let Some(schema) = &j.input.requested_output_schema {
            app.register_run_schema(p, c, r, schema.clone())?;
        }
        let mut files = Vec::with_capacity(j.input.attachments.len());
        for id in &j.input.attachments {
            let owner = j.caller.as_ref().map_or(c, |a| a.chat);
            let file = app.0.chats.visible_attachment(owner, *id)?;
            if owner == c {
                files.push(*id);
                continue;
            }
            let key = crate::chat::stable_id([
                c.as_bytes().as_slice(),
                id.as_bytes().as_slice(),
                b"launch-attachment",
            ]);
            let bytes = app.0.chats.verified_bytes(&file)?;
            files.push(
                app.0
                    .chats
                    .persist_attachment(c, key, &file.name, &file.mime, file.kind, &bytes)?
                    .id,
            );
        }
        // Serialise the last permission check with a revoke. The queue's own durable id/digest
        // makes the window after enqueue and before the released receipt safe to replay.
        {
            let db = app.0.catalog.lock().unwrap();
            authorize(&db, j)?;
            result["input"] = serde_json::to_value(app.0.chats.enqueue_attached(
                c,
                r,
                j.input.message.as_deref().unwrap_or_default(),
                &files,
            )?)?;
        }
        app.wake_queue();
    }
    Ok(result)
}
