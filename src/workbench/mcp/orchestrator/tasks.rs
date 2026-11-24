use super::*;
use crate::chat::{InputStatus, Item, Role, TurnStatus};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Spec {
    task: String,
    title: Option<String>,
    runtime: Option<String>,
    model: Option<String>,
    isolation: Option<String>,
    branch: Option<String>,
    requested_output_schema: Option<Value>,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Admitted,
    Opening,
    Running,
    WaitingForChildren,
    ResultAvailable,
    Cancelling,
    Cancelled,
    Failed,
    Unconfirmed,
}
#[derive(Clone, Serialize, Deserialize)]
pub(in crate::workbench) struct Task {
    id: Uuid,
    project: Uuid,
    parent: Uuid,
    spec: Spec,
    runtime: String,
    model: Option<String>,
    workspace: PathBuf,
    settings: crate::SessionSettings,
    approvals: crate::ApprovalMode,
    directory: Option<PathBuf>,
    phase: Phase,
    summary: Option<String>,
    output: Option<Value>,
    delivered: bool,
}
impl Task {
    fn input(&self) -> Uuid {
        crate::chat::stable_id([self.id.as_bytes().as_slice(), b"delegated input"])
    }
    fn wake(&self) -> Uuid {
        crate::chat::stable_id([self.id.as_bytes().as_slice(), b"delegated result"])
    }
    fn terminal(&self) -> bool {
        matches!(
            self.phase,
            Phase::Cancelled | Phase::Failed | Phase::ResultAvailable | Phase::Unconfirmed
        )
    }
    fn view(&self) -> Value {
        json!({"task_id":self.id,"thread_id":self.id,"parent_thread_id":self.parent,"project_id":self.project,"input_id":self.input(),"work_state":self.phase,"summary":self.summary,"structured_output":self.output,"delivered":self.delivered})
    }
}
pub(in crate::workbench::mcp) fn owned(db: &Catalog, parent: Uuid, child: Uuid) -> Option<Uuid> {
    db.model_tasks
        .get(&child)
        .filter(|t| t.parent == parent && t.phase != Phase::Admitted && t.phase != Phase::Opening)
        .map(|t| t.project)
}
fn exists(app: &Workbench, id: Uuid) -> Result<bool> {
    match app.0.chats.initial_info(id) {
        Ok(_) => Ok(true),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn get(app: &Workbench, id: Uuid) -> Result<Task> {
    app.0
        .catalog
        .lock()
        .unwrap()
        .model_tasks
        .get(&id)
        .cloned()
        .ok_or_else(|| refused("unknown task"))
}
fn change(app: &Workbench, id: Uuid, f: impl FnOnce(&mut Task)) -> Result<Task> {
    app.update(|db| {
        let t = db
            .model_tasks
            .get_mut(&id)
            .ok_or_else(|| refused("unknown task"))?;
        let cancelling = t.phase == Phase::Cancelling;
        f(t);
        if cancelling && t.phase != Phase::Cancelled {
            t.phase = Phase::Cancelling;
        }
        Ok(t.clone())
    })
}
fn still_granted(app: &Workbench, t: &Task) -> Result<()> {
    let db = app.0.catalog.lock().unwrap();
    let policy = Policy::current(&db, t.project, t.parent)
        .filter(|p| p.executes())
        .ok_or_else(|| refused("delegation grant was revoked"))?;
    if !scope(policy).is_some_and(|s| s.delegate)
        || t.directory
            .as_deref()
            .is_some_and(|d| policy.worktree_directory() != Some(d))
    {
        return Err(refused("delegation grant was revoked"));
    }
    Ok(())
}
pub(in crate::workbench) fn recover(app: &Workbench) {
    let ids: Vec<_> = app
        .0
        .catalog
        .lock()
        .unwrap()
        .model_tasks
        .iter()
        .filter(|(_, r)| !r.terminal() || (r.phase != Phase::Cancelled && !r.delivered))
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        launch(app.clone(), id);
    }
}
fn launch(app: Workbench, id: Uuid) {
    tokio::spawn(async move {
        let gate = app.0.mcp.command_gate(id);
        let Ok(_guard) = gate.try_lock() else { return };
        let Ok(t) = get(&app, id) else { return };
        if let Err(e) = progress(&app, t).await
            && !matches!(e, Error::Busy)
        {
            let _ = change(&app, id, |r| {
                if !r.terminal() && r.phase != Phase::Cancelling {
                    r.phase = Phase::Failed;
                    r.summary = Some(e.to_string());
                }
            });
        }
    });
}
async fn progress(app: &Workbench, t: Task) -> Result<()> {
    if t.phase == Phase::Cancelling {
        return cancel_one(app, &t).await;
    }
    if t.terminal() {
        if !t.delivered && t.phase != Phase::Cancelled {
            let msg = format!(
                "Delegated task {} finished ({})\n{}",
                t.id,
                serde_json::to_string(&t.phase)?,
                t.summary.as_deref().unwrap_or("")
            );
            if !app.archived(t.parent) && still_granted(app, &t).is_ok() {
                app.0.chats.enqueue(t.parent, t.wake(), msg)?;
                app.wake_queue();
            }
            change(app, t.id, |r| r.delivered = true)?;
        }
        return Ok(());
    }
    if matches!(t.phase, Phase::Opening | Phase::Admitted) {
        still_granted(app, &t)?;
        let live = app.binding(t.project, t.parent)?;
        let scope = Policy::current(&app.0.catalog.lock().unwrap(), t.project, t.parent)
            .and_then(|p| p.thread_tools.clone())
            .ok_or_else(|| refused("missing delegation permission"))?;
        let (runtime, model) = child_model(
            &live,
            &scope,
            t.spec.runtime.as_deref(),
            t.spec.model.as_deref(),
        )?;
        if runtime != t.runtime
            || model != t.model
            || live.approvals != t.approvals
            || live.settings != t.settings
            || live.workspace != t.workspace
        {
            return Err(refused(
                "parent chat execution settings changed before child launch",
            ));
        }
        if t.phase == Phase::Opening && !exists(app, t.id)? {
            change(app, t.id, |r| {
                r.phase = Phase::Unconfirmed;
                r.summary = Some(
                    "host stopped during native chat creation; no second child was launched".into(),
                );
            })?;
            return Ok(());
        }
        let cwd = if let Some(dir) = &t.directory {
            let branch = t
                .spec
                .branch
                .clone()
                .unwrap_or_else(|| format!("kar/task-{}", t.id));
            crate::workbench::task_folders::handoff_task(
                app,
                t.project,
                t.id,
                dir.join(t.id.to_string()),
                branch,
                "HEAD".into(),
                false,
            )?
            .0
            .project_path
        } else {
            t.workspace.clone()
        };
        app.workspace_at(&cwd)?;
        app.bind_model_checkout(t.project, t.id, &cwd)?;
        change(app, t.id, |r| r.phase = Phase::Opening)?;
        app.create_chat_with_settings(
            t.project,
            t.id,
            &t.runtime,
            t.model.clone(),
            t.approvals,
            t.settings.clone(),
        )
        .await?;
        let title = t.spec.title.clone().unwrap_or_else(|| {
            t.spec
                .task
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(80)
                .collect()
        });
        crate::workbench::thread_state::model_rename(app.clone(), t.project, t.id, t.id, title)
            .await?;
        if let Some(schema) = &t.spec.requested_output_schema {
            app.register_run_schema(t.project, t.id, t.input(), schema.clone())?;
        }
        still_granted(app, &t)?;
        app.0.chats.enqueue(t.id, t.input(), t.spec.task.clone())?;
        change(app, t.id, |r| r.phase = Phase::Running)?;
        app.wake_queue();
        return Ok(());
    }
    let h = app.0.chats.history(t.id)?;
    let Some(turn) = h.turns.iter().find(|r| r.id == t.input()) else {
        if let Some(entry) = app
            .0
            .chats
            .queued_inputs(t.id)?
            .iter()
            .find(|q| q.id == t.input())
            && matches!(
                entry.status,
                InputStatus::Withdrawn
                    | InputStatus::Cancelled
                    | InputStatus::Refused
                    | InputStatus::TooLarge
            )
        {
            change(app, t.id, |r| {
                r.phase = Phase::Failed;
                r.summary = Some(format!("child input ended with {:?}", entry.status))
            })?;
        }
        return Ok(());
    };
    if turn.status == TurnStatus::Running {
        return Ok(());
    }
    let children: Vec<_> = app
        .0
        .catalog
        .lock()
        .unwrap()
        .model_tasks
        .values()
        .filter(|r| r.parent == t.id)
        .cloned()
        .collect();
    let inbox = if children.is_empty() {
        vec![]
    } else {
        app.0.chats.queued_inputs(t.id)?
    };
    let waiting = children.iter().any(|r| {
        !r.terminal()
            || (!r.delivered && r.phase != Phase::Cancelled)
            || inbox.iter().any(|q| {
                q.id == r.wake()
                    && matches!(
                        q.status,
                        InputStatus::Pending | InputStatus::Sending | InputStatus::Uncertain
                    )
            })
            || h.turns
                .iter()
                .any(|t| t.id == r.wake() && t.status == TurnStatus::Running)
    });
    if waiting {
        if t.phase != Phase::WaitingForChildren {
            change(app, t.id, |r| r.phase = Phase::WaitingForChildren)?;
        }
        return Ok(());
    }
    let turn = h
        .turns
        .iter()
        .rev()
        .find(|t| children.iter().any(|r| r.wake() == t.id))
        .unwrap_or(turn);
    let summary = turn
        .items
        .iter()
        .rev()
        .filter(|r| r.source.path.is_empty())
        .find_map(|r| match &r.item {
            Item::Message {
                role: Role::Assistant,
                text,
                ..
            } => Some(text.chars().take(16000).collect()),
            _ => None,
        });
    change(app, t.id, |r| {
        r.phase = match turn.status {
            TurnStatus::Completed => Phase::ResultAvailable,
            TurnStatus::Interrupted => Phase::Unconfirmed,
            TurnStatus::Aborted => Phase::Cancelled,
            _ => Phase::Failed,
        };
        r.summary = summary;
        r.output = turn.structured_output.clone();
    })?;
    app.wake_queue();
    Ok(())
}
async fn cancel_one(app: &Workbench, t: &Task) -> Result<()> {
    if exists(app, t.id)? {
        for q in app
            .0
            .chats
            .queued_inputs(t.id)?
            .into_iter()
            .filter(|r| r.status == InputStatus::Pending)
        {
            app.0.chats.withdraw(t.id, q.id)?;
        }
        if let Some(active) = app
            .0
            .chats
            .history(t.id)?
            .turns
            .last()
            .filter(|r| r.status == TurnStatus::Running)
        {
            let session = app.active_control(t.project, t.id)?.ok_or(Error::Busy)?;
            session.abort_turn(active.id, None).await?;
            return Ok(());
        }
        app.set_archived(t.project, t.id, true).await?;
    }
    change(app, t.id, |r| {
        r.phase = Phase::Cancelled;
        r.delivered = true;
    })?;
    Ok(())
}
pub(super) async fn call(app: &Workbench, c: &Caller, name: &str, mut req: Value) -> Result<Value> {
    let grant = c.grant.as_ref().unwrap();
    if matches!(name, "kar_task_status" | "kar_task_cancel") {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            task_id: Uuid,
        }
        let q: Input = serde_json::from_value(req)?;
        let gate = app.0.mcp.command_gate(q.task_id);
        let _guard = gate.lock().await;
        let prior = get(app, q.task_id)?;
        if prior.parent != grant.chat || prior.project != grant.project {
            return Err(refused("task was not delegated by this chat"));
        }
        if name == "kar_task_status"
            && matches!(prior.phase, Phase::Running | Phase::WaitingForChildren)
        {
            progress(app, prior).await?;
        }
        let t =
            app.update(|db| {
                authorize(db, c, false, name == "kar_task_cancel")?;
                let t = db
                    .model_tasks
                    .get_mut(&q.task_id)
                    .filter(|t| t.parent == grant.chat && t.project == grant.project)
                    .ok_or_else(|| refused("task was not delegated by this chat"))?;
                if t.terminal() {
                    t.delivered = true;
                } else if name == "kar_task_cancel" {
                    t.phase = Phase::Cancelling;
                }
                let t = t.clone();
                if t.phase == Phase::Cancelling {
                    let mut parents = vec![t.id];
                    while let Some(p) = parents.pop() {
                        for r in db.model_tasks.values_mut().filter(|r| {
                            r.parent == p && !r.terminal() && r.phase != Phase::Cancelling
                        }) {
                            r.phase = Phase::Cancelling;
                            parents.push(r.id);
                        }
                    }
                }
                Ok(t)
            })?;
        app.wake_queue();
        return Ok(t.view());
    }
    let map = req
        .as_object_mut()
        .ok_or_else(|| refused("expected task arguments"))?;
    let cmd: Uuid = serde_json::from_value(
        map.remove("command_id")
            .ok_or_else(|| refused("missing command ID"))?,
    )?;
    let spec: Spec = serde_json::from_value(req)?;
    let p = grant.project;
    let parent = app.binding(p, grant.chat)?;
    let policy = c.policy.as_ref().unwrap();
    let scope = scope(policy).unwrap();
    {
        if spec.task.trim().is_empty()
            || spec.task.len() > 65536
            || spec.title.as_ref().is_some_and(|s| {
                s.trim().is_empty() || s.chars().count() > 200 || s.contains(['\n', '\r', '\0'])
            })
        {
            return Err(refused("invalid child prompt or title"));
        }
        let (runtime, model) = child_model(
            &parent,
            scope,
            spec.runtime.as_deref(),
            spec.model.as_deref(),
        )?;
        let directory = match spec.isolation.as_deref().unwrap_or("shared") {
            "shared" if spec.branch.is_none() => None,
            "worktree" => Some(
                policy
                    .worktree_directory()
                    .ok_or_else(|| refused("no worktree directory granted"))?
                    .to_owned(),
            ),
            _ => return Err(refused("invalid isolation or branch")),
        };
        let id = cmd;
        let task = Task {
            id,
            project: p,
            parent: grant.chat,
            spec,
            runtime,
            model,
            workspace: parent.workspace.clone(),
            settings: parent.settings.clone(),
            approvals: parent.approvals,
            directory,
            phase: Phase::Admitted,
            summary: None,
            output: None,
            delivered: false,
        };
        let saved = app.update(|db| {
            authorize(db, c, false, true)?;
            if let Some(old) = db.model_tasks.get(&id) {
                if old.parent != task.parent || old.project != task.project || old.spec != task.spec
                {
                    return Err(refused("delegate command already used"));
                }
                return Ok(old.clone());
            }
            if id == grant.chat || exists(app, id)? {
                return Err(refused("child ID already in use"));
            }
            db.model_tasks.insert(id, task.clone());
            Ok(task)
        })?;
        app.wake_queue();
        Ok(saved.view())
    }
}
