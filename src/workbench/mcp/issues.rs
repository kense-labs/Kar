//! A model lease can use only the issue tracker explicitly granted by a workbench client.
use super::*;
use crate::workbench::{Catalog, forge, http::ApiError};
use axum::{extract::Path, routing::get};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Plan,
    Execute,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::workbench) struct Policy {
    mode: Mode,
    #[serde(default)]
    environment_writes: bool,
    #[serde(default)]
    pub(super) preview_tools: super::preview_tools::Resources,
    #[serde(default)]
    pub(super) project_access: super::project_tools::Access,
    #[serde(default)]
    issue_reads: bool,
    #[serde(default)]
    issue_writes: bool,
    #[serde(default)]
    workflow_reads: bool,
    #[serde(default)]
    pub(in crate::workbench) review_guides:Option<forge::guides::Settings>,
    #[serde(default)]
    pub(super) thread_tools:Option<super::operations::Scope>,
    #[serde(default)]
    pull_reads:bool,
    #[serde(default)]
    pull_writes:bool,
    #[serde(default)]
    workflow_writes: bool,
    #[serde(default)]
    worktree_directory: Option<std::path::PathBuf>,
    pub(super) issue_scope: Option<String>,
    pub(super) repository: Option<String>,
    #[serde(default)]
    pub(in crate::workbench) pull_repository:Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    mode: Mode,
    #[serde(default)]
    environment_writes: bool,
    #[serde(default)]
    pub(super) preview_tools: super::preview_tools::Resources,
    #[serde(default)]
    pub(super) project_access: super::project_tools::Access,
    #[serde(default)]
    issue_reads: bool,
    #[serde(default)]
    issue_writes: bool,
    #[serde(default)]
    workflow_reads: bool,
    #[serde(default)]
    pub(in crate::workbench) review_guides:Option<forge::guides::Settings>,
    #[serde(default)]
    pub(super) thread_tools:Option<super::operations::Scope>,
    #[serde(default)]
    pull_reads:bool,
    #[serde(default)]
    pull_writes:bool,
    #[serde(default)]
    workflow_writes: bool,
    #[serde(default)]
    worktree_directory: Option<std::path::PathBuf>,
    issue_scope: Option<String>,
}

impl Policy {
    pub(super) fn writes_environment(&self)->bool {self.executes() && self.environment_writes}
    pub(super) fn executes(&self) -> bool { self.mode == Mode::Execute }
    fn get(db: &Catalog, project: Uuid, chat: Uuid) -> Option<&Self> {
        db.model_tool_permissions.get(&project)?.get(&chat)
    }
    pub(in crate::workbench) fn current(db: &Catalog, project: Uuid, chat: Uuid) -> Option<&Self> {
        if db.archived.contains(&chat) || db.projects.get(&project)?.metadata.deleted {
            return None;
        }
        Self::get(db, project, chat)
    }
    pub(super) fn worktree_directory(&self) -> Option<&std::path::Path> {
        self.worktree_directory.as_deref().filter(|_| self.mode == Mode::Execute)
    }
    pub(super) fn reads_pulls(&self)->bool{(self.pull_reads || self.writes_pulls()) && self.pull_repository.is_some()}
    pub(in crate::workbench) fn writes_pulls(&self)->bool{self.mode==Mode::Execute && self.pull_writes && self.pull_repository.is_some()}
    pub(super) fn reads_workflows(&self) -> bool {
        self.workflow_reads || self.writes_workflows()
    }
    pub(super) fn writes_workflows(&self) -> bool {
        self.mode == Mode::Execute && self.workflow_writes
    }
    pub(super) fn reads_issues(&self) -> bool {
        (self.issue_reads || self.issue_writes) && self.repository.is_some()
    }
    pub(super) fn writes_issues(&self) -> bool {
        self.mode == Mode::Execute && self.issue_writes && self.repository.is_some()
    }
}

pub(super) fn authorized(app: &Workbench, grant: &Grant) -> Option<Policy> {
    Policy::current(&app.0.catalog.lock().unwrap(), grant.project, grant.chat).cloned()
}

async fn read(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
) -> Result<Json<Policy>, ApiError> {
    app.binding(p, c)?;
    Ok(Json(
        Policy::get(&app.0.catalog.lock().unwrap(), p, c)
            .cloned()
            .unwrap_or_default(),
    ))
}

async fn configure(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    device: Option<axum::Extension<crate::workbench::access::DeviceLease>>,
    Json(cfg): Json<Configure>,
) -> Result<Json<Policy>, forge::ForgeError> {
    app.binding(p, c)?;
    if cfg.environment_writes && device.is_some(){return Err(forge::ForgeError::bad("only the owner can grant model environment writes"));}
    let repository = if cfg.issue_reads || cfg.issue_writes && cfg.mode == Mode::Execute {
        Some(forge::Context::issue_repository(app.clone(), p, cfg.issue_scope.as_deref()).await?)
    } else {
        None
    };
    let worktree_directory = if let Some(path) = cfg.worktree_directory {
        let dir=app.workspace_at(&path)?;
        Some(dir.root)
    } else {None};
    let pull_repository=if cfg.pull_reads || cfg.pull_writes && cfg.mode==Mode::Execute {Some(forge::model::pull_repository(app.clone(),p).await?)}else{None};
    if let Some(s)=&cfg.thread_tools {s.validate(&app,p)?;}
    let policy = Policy {
        review_guides:cfg.review_guides,
        thread_tools:cfg.thread_tools,
        pull_repository,pull_reads:cfg.pull_reads,pull_writes:cfg.pull_writes,
        worktree_directory,
        mode: cfg.mode,
        environment_writes: cfg.environment_writes,
        preview_tools: cfg.preview_tools,
        project_access: cfg.project_access.validate(&app)?,
        issue_reads: cfg.issue_reads,
        issue_writes: cfg.issue_writes,
        workflow_reads: cfg.workflow_reads,
        workflow_writes: cfg.workflow_writes,
        issue_scope: cfg.issue_scope,
        repository,
    };
    app.update(|db| {
        if (policy.thread_tools.is_some() || policy.reads_pulls() || policy.reads_issues() || policy.reads_workflows() || policy.worktree_directory().is_some())
            && (db.archived.contains(&c)
                || db
                    .projects
                    .get(&p)
                    .is_none_or(|project| project.metadata.deleted))
        {
            return Err(crate::Error::Refused("chat or project is inactive".into()));
        }
        db.model_tool_permissions
            .entry(p)
            .or_default()
            .insert(c, policy.clone());
        Ok(())
    })?;
    if let Some(lease) = app.controller(c).model_tools.lock().unwrap().as_ref() {
        lease.tools_changed();
    }
    Ok(Json(policy))
}

pub(in crate::workbench) fn permission_routes() -> Router<Workbench> {
    Router::new().route(
        "/projects/{project}/chats/{chat}/model-tools/permissions",
        get(read).put(configure),
    )
}

pub(super) async fn call(
    app: Workbench,
    caller: Caller,
    name: &str,
    mut input: Value,
) -> crate::Result<Value> {
    let forbidden = || {
        crate::Error::Refused(
            "model issue writes require an authorized execution session for this tracker".into(),
        )
    };
    let policy = caller.policy.ok_or_else(forbidden)?;
    let grant = caller.grant.ok_or_else(forbidden)?;
    let obj = input
        .as_object_mut()
        .ok_or_else(|| crate::Error::Refused("expected tool arguments".into()))?;

    for (k, expected) in [("project_id", grant.project), ("thread_id", grant.chat)] {
        if let Some(value) = obj.remove(k)
            && serde_json::from_value::<Uuid>(value)? != expected
        {
            return Err(forbidden());
        }
    }
    let write = matches!(
        name,
        "kar_create_issue" | "kar_comment_on_issue" | "kar_set_issue_state"
    );
    if write && (caller.plan || !policy.writes_issues()) || !write && !policy.reads_issues() {
        return Err(forbidden());
    }
    let scope = policy.issue_scope.clone();
    let authorize = |db: &Catalog, repo: &str| {
        if !grant.active()
            || Policy::current(db, grant.project, grant.chat) != Some(&policy)
            || policy.repository.as_deref() != Some(repo)
        {
            return Err(forbidden());
        }
        Ok(())
    };
    if !write {
        return forge::model_issue_read(
            &app,
            grant.project,
            scope.as_deref(),
            name,
            input,
            !caller.plan && policy.writes_issues(),
            authorize,
        )
        .await
        .map_err(|e| crate::Error::Refused(format!("{}: {}", e.kind, e.detail)));
    }
    let command: Uuid = serde_json::from_value(obj.remove("command_id").unwrap_or_default())?;
    let op = match name {
        "kar_create_issue" => "create_issue",
        "kar_comment_on_issue" => "comment_issue",
        "kar_set_issue_state"
            if obj
                .get("number")
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0)
                && matches!(
                    obj.get("state").and_then(Value::as_str),
                    Some("open" | "closed")
                ) =>
        {
            "set_issue_state"
        }
        _ => return Err(forbidden()),
    };
    if obj.insert("op".to_owned(), Value::from(op)).is_some() {
        return Err(crate::Error::Refused("unexpected operation".into()));
    }
    let operation = serde_json::from_value(input)?;
    // Retain a claimed remote write even when the model's HTTP response is interrupted.
    tokio::spawn(async move {
        forge::model_issue_write(
            app,
            grant.project,
            grant.chat,
            command,
            policy.issue_scope.clone(),
            operation,
            move |db, repo| {
                if !grant.active()
                    || Policy::current(db, grant.project, grant.chat) != Some(&policy)
                    || policy.repository.as_deref() != Some(repo)
                {
                    return Err(forbidden());
                }
                Ok(())
            },
        )
        .await
        .map_err(|e| crate::Error::Refused(format!("{}: {}", e.kind, e.detail)))
    })
    .await
    .map_err(|e| crate::Error::Closed(e.to_string()))?
}
