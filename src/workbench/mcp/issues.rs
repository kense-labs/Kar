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
    issue_reads: bool,
    #[serde(default)]
    issue_writes: bool,
    #[serde(default)]
    workflow_reads: bool,
    #[serde(default)]
    workflow_writes: bool,
    issue_scope: Option<String>,
    repository: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    mode: Mode,
    #[serde(default)]
    issue_reads: bool,
    #[serde(default)]
    issue_writes: bool,
    #[serde(default)]
    workflow_reads: bool,
    #[serde(default)]
    workflow_writes: bool,
    issue_scope: Option<String>,
}

impl Policy {
    fn get(db: &Catalog, project: Uuid, chat: Uuid) -> Option<&Self> {
        db.model_tool_permissions.get(&project)?.get(&chat)
    }
    pub(super) fn current(db: &Catalog, project: Uuid, chat: Uuid) -> Option<&Self> {
        if db.archived.contains(&chat) || db.projects.get(&project)?.metadata.deleted {
            return None;
        }
        Self::get(db, project, chat)
    }
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
    Json(cfg): Json<Configure>,
) -> Result<Json<Policy>, forge::ForgeError> {
    app.binding(p, c)?;
    let repository = if cfg.issue_reads || cfg.issue_writes && cfg.mode == Mode::Execute {
        Some(forge::Context::issue_repository(app.clone(), p, cfg.issue_scope.as_deref()).await?)
    } else {
        None
    };
    let policy = Policy {
        mode: cfg.mode,
        issue_reads: cfg.issue_reads,
        issue_writes: cfg.issue_writes,
        workflow_reads: cfg.workflow_reads,
        workflow_writes: cfg.workflow_writes,
        issue_scope: cfg.issue_scope,
        repository,
    };
    app.update(|db| {
        if (policy.reads_issues() || policy.reads_workflows())
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
