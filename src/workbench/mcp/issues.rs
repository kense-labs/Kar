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
    issue_writes: bool,
    issue_scope: Option<String>,
    repository: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    mode: Mode,
    issue_writes: bool,
    issue_scope: Option<String>,
}

impl Policy {
    fn get(db: &Catalog, project: Uuid, chat: Uuid) -> Option<&Self> {
        db.model_tool_permissions.get(&project)?.get(&chat)
    }
    fn writable(db: &Catalog, project: Uuid, chat: Uuid) -> Option<&Self> {
        if db.archived.contains(&chat) || db.projects.get(&project)?.metadata.deleted {
            return None;
        }
        Self::get(db, project, chat).filter(|p| p.allows())
    }
    fn allows(&self) -> bool {
        self.mode == Mode::Execute && self.issue_writes && self.repository.is_some()
    }
}

pub(super) fn authorized(app: &Workbench, grant: &Grant, plan: bool) -> Option<Policy> {
    if plan {
        return None;
    }
    Policy::writable(&app.0.catalog.lock().unwrap(), grant.project, grant.chat).cloned()
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
    let repository = if cfg.issue_writes && cfg.mode == Mode::Execute {
        Some(forge::Context::issue_repository(app.clone(), p, cfg.issue_scope.as_deref()).await?)
    } else {
        None
    };
    let policy = Policy {
        mode: cfg.mode,
        issue_writes: cfg.issue_writes,
        issue_scope: cfg.issue_scope,
        repository,
    };
    app.update(|db| {
        if policy.allows()
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
    let policy = caller.issue_policy.ok_or_else(forbidden)?;
    let grant = caller.grant.ok_or_else(forbidden)?;
    let obj = input
        .as_object_mut()
        .ok_or_else(|| crate::Error::Refused("expected tool arguments".into()))?;
    let command: Uuid = serde_json::from_value(obj.remove("command_id").unwrap_or_default())?;
    for (k, expected) in [("project_id", grant.project), ("thread_id", grant.chat)] {
        if let Some(value) = obj.remove(k)
            && serde_json::from_value::<Uuid>(value)? != expected
        {
            return Err(forbidden());
        }
    }
    let op = match name {
        "kar_create_issue" => "create_issue",
        "kar_comment_on_issue" => "comment_issue",
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
                    || Policy::writable(db, grant.project, grant.chat) != Some(&policy)
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
