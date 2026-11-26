use super::{Caller, Grant, Policy, Workbench, toolkit::Tool};
use crate::{Error, Result, workbench::Project};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

mod launch;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(in crate::workbench) struct Access {
    manage: bool,
    launch: bool,
    scratch: bool,
    projects: BTreeSet<Uuid>,
    directories: Vec<PathBuf>,
}
impl Access {
    pub(super) fn validate(mut self, app: &Workbench) -> Result<Self> {
        for id in &self.projects {
            if app.project(*id)?.metadata.deleted {
                return Err(refused("project was deleted"));
            }
        }
        for path in &mut self.directories {
            *path = app.workspace_at(path)?.root;
        }
        self.directories.sort();
        self.directories.dedup();
        Ok(self)
    }
    pub(in crate::workbench) fn authorizes_launch(
        &self,
        db: &crate::workbench::Catalog,
        project: Uuid,
        chat: Uuid,
    ) -> bool {
        self.launch
            && Policy::current(db, project, chat)
                .is_some_and(|p| p.executes() && p.project_access == *self)
    }
    pub(in crate::workbench) fn check_directory(&self, path: &Path) -> Result<()> {
        self.directory(path).map(drop)
    }
    fn directory(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
            return Err(refused(
                "expected an absolute path inside a granted directory",
            ));
        }
        // Resolve the longest existing ancestor, including symlinks, before admitting a new child.
        let mut base = path;
        while !base.try_exists()? {
            base = base
                .parent()
                .ok_or_else(|| refused("missing directory ancestor"))?;
        }
        let resolved = base.canonicalize()?.join(
            path.strip_prefix(base)
                .map_err(|_| refused("invalid directory"))?,
        );
        if !self.directories.iter().any(|r| resolved.starts_with(r)) {
            return Err(refused("directory is outside this model grant"));
        }
        Ok(resolved)
    }
    fn sees(&self, grant: &Grant, p: &Project) -> bool {
        grant.project == p.id
            || self.projects.contains(&p.id)
            || self.directories.iter().any(|r| p.root.starts_with(r))
    }
}
pub(super) fn visible(caller: &Caller, p: &Project) -> bool {
    caller.grant.as_ref().is_none_or(|g| {
        g.project == p.id
            || caller
                .policy
                .as_ref()
                .is_some_and(|policy| policy.project_access.sees(g, p))
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(in crate::workbench) struct Receipt {
    chat: Uuid,
    name: String,
    digest: String,
    result: Option<Value>,
}
pub(in crate::workbench) fn pending_launch(
    db: &crate::workbench::Catalog,
    id: Uuid,
) -> Option<Uuid> {
    db.model_project_commands
        .get(&id)
        .filter(|r| r.name == "kar_thread_launch" && r.result.is_none())
        .map(|r| r.chat)
}
pub(in crate::workbench) fn launched_project(
    db: &crate::workbench::Catalog,
    caller: Uuid,
    target: Uuid,
) -> Option<Uuid> {
    let receipt = db.model_project_commands.get(&target)?;
    if receipt.chat != caller || receipt.name != "kar_thread_launch" {
        return None;
    }
    let result = receipt.result.as_ref()?;
    if result["thread_id"].as_str()?.parse::<Uuid>().ok()? != target {
        return None;
    }
    result["project_id"].as_str()?.parse().ok()
}
pub(super) fn handles(name: &str) -> bool {
    matches!(
        name,
        "kar_project_create"
            | "kar_project_update"
            | "kar_project_delete"
            | "kar_project_clone"
            | "kar_thread_launch"
    )
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    let Some(policy) = &c.policy else {
        return vec![];
    };
    if c.grant.is_none() || c.plan || !policy.executes() {
        return vec![];
    }
    let id = json!({"type":"string","format":"uuid"});
    let string = json!({"type":"string"});
    let flag = json!({"type":"boolean"});
    let mut tools = vec![];
    if policy.project_access.manage {
        let project_settings = json!({
            "type": "object",
            "properties": {
                "title": string,
                "workspaceRoot": string,
                "pinned": flag,
                "archived": flag,
                "autoPull": flag,
                "defaultRuntime": string,
                "defaultModel": string,
                "trackFolders": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"name": string, "path": string},
                        "required": ["name", "path"]
                    }
                },
                "scripts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": string,
                            "name": string,
                            "command": string,
                            "icon": string,
                            "runOnWorktreeCreate": flag
                        },
                        "required": ["id", "name", "command"]
                    }
                }
            }
        });
        let repo_lookup = json!({
            "type": "object",
            "properties": {
                "provider": string,
                "host": string,
                "owner": string,
                "name": string,
                "repository": string
            }
        });
        tools.push(Tool{name:"kar_project_create",description:"Register a folder in a granted directory or create a named Git project with a README and first commit.",properties:json!({"command_id":id,"project_id":id,"title":string,"workspace_root":string,"parent":string,"create_directory":flag,"settings":project_settings}),required:&["command_id","title"]});
        tools.push(Tool{name:"kar_project_update",description:"Update an authorized project's title, default model, tracked folders or scripts.",properties:json!({"command_id":id,"project_id":id,"settings":project_settings}),required:&["command_id","project_id","settings"]});
        tools.push(Tool{name:"kar_project_delete",description:"Remove a project registration. A nonempty project requires force. Files stay on disk.",properties:json!({"command_id":id,"project_id":id,"force":flag}),required:&["command_id","project_id"]});
        tools.push(Tool{name:"kar_project_clone",description:"Start a tracked repository clone into a granted directory and register the project.",properties:json!({"command_id":id,"project_id":id,"title":string,"directory":string,"source":string,"lookup":repo_lookup,"protocol":{"enum":["https","ssh","auto"]}}),required:&["command_id","title","directory"]});
    }
    if policy.project_access.launch {
        tools.push(Tool{name:"kar_thread_launch",description:"Launch a top-level chat in a project, scratch folder, or verified worktree. Reuse command_id after a lost reply.",properties:json!({"command_id":id,"project_id":id,"scratch":flag,"title":string,"runtime":string,"model":string,"message":string,"attachments":{"type":"array","items":id},"requested_output_schema":{},"workspace":{"enum":["root","worktree","existing_worktree"]},"directory":string,"base_ref":string,"branch":string,"start_from_origin":flag,"generate_title":flag,"defer_release":flag,"approvals":{"enum":["deny","host"]},"effort":string,"acp_mode":string}),required:&["command_id","title"]});
    }
    tools
}
fn refused(msg: &str) -> Error {
    Error::Refused(msg.into())
}
fn id(v: &Value, key: &str) -> Result<Uuid> {
    serde_json::from_value(v[key].clone()).map_err(Into::into)
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| refused(&format!("{key} is required")))
}
fn target(v: &Value, fallback: Uuid) -> Result<Uuid> {
    v.get("project_id")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map(|v| v.unwrap_or(fallback))
        .map_err(Into::into)
}
fn check(app: &Workbench, caller: &Caller, name: &str) -> Result<Policy> {
    let grant = caller
        .grant
        .as_ref()
        .filter(|g| g.active())
        .ok_or_else(|| refused("inactive model grant"))?;
    let policy =
        super::issues::authorized(app, grant).ok_or_else(|| refused("missing project access"))?;
    let access = &policy.project_access;
    if caller.plan
        || !policy.executes()
        || !(if name == "kar_thread_launch" {
            access.launch
        } else {
            access.manage
        })
    {
        return Err(refused("project action is outside this grant"));
    }
    Ok(policy)
}
pub(super) async fn call(
    app: &Workbench,
    caller: &Caller,
    name: &str,
    input: Value,
) -> Result<Value> {
    let cmd = id(&input, "command_id")?;
    let gate = app.0.mcp.command_gate(cmd);
    let _command = gate.lock().await;
    let policy = check(app, caller, name)?;
    let grant = caller.grant.as_ref().unwrap();
    let definition = tools(caller)
        .into_iter()
        .find(|t| t.name == name)
        .ok_or_else(|| refused("project tool disabled"))?;
    let obj = input
        .as_object()
        .ok_or_else(|| refused("expected project operation arguments"))?;
    if definition
        .required
        .iter()
        .any(|key| !obj.contains_key(*key))
    {
        return Err(refused("missing required project operation field"));
    }
    if obj.keys().any(|k| definition.properties.get(k).is_none()) {
        return Err(refused("unknown project operation field"));
    }
    let digest = crate::workbench::access::digest(&serde_json::to_string(&input)?);
    let slot = app.controller(cmd);
    let reservation = if name == "kar_thread_launch" {
        Some(slot.owner.lock().await)
    } else {
        None
    };
    let existing = app.update(|db| {
        if let Some(old) = db.model_project_commands.get(&cmd) {
            if old.chat != grant.chat || old.name != name || old.digest != digest {
                return Err(refused("command ID belongs to another project operation"));
            }
            return Ok(old.result.clone());
        }
        if name == "kar_thread_launch" {
            if crate::workbench::prepared_runs::reservation(db, cmd).is_some() {
                return Err(Error::Busy);
            }
            match app.0.chats.initial_info(cmd) {
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => (),
                Ok(_) => return Err(refused("launch ID belongs to an existing chat")),
                Err(e) => return Err(e),
            }
        }
        db.model_project_commands.insert(
            cmd,
            Receipt {
                chat: grant.chat,
                name: name.into(),
                digest: digest.clone(),
                result: None,
            },
        );
        Ok(None)
    })?;
    drop(reservation);
    if let Some(result) = existing {
        return Ok(result);
    }
    let p = target(
        &input,
        if matches!(name, "kar_project_create" | "kar_project_clone") {
            cmd
        } else {
            grant.project
        },
    )?;
    let access = &policy.project_access;
    let mut mutation = json!({"type":name.replace("kar_project_","project."),"commandId":cmd.to_string(),"projectId":p.to_string()});
    let result = match name {
        "kar_thread_launch" => launch::run(app, grant, access, p, cmd, &input).await?,
        "kar_project_clone" => {
            let directory = access.directory(Path::new(string(&input, "directory")?))?;
            if let Ok(existing) = app.project(p)
                && !access.sees(grant, &existing)
            {
                return Err(refused("project is outside this grant"));
            }
            let mut clone = json!({"directory":directory});
            for key in ["source", "lookup", "protocol"] {
                if let Some(value) = input.get(key) {
                    clone[key] = value.clone();
                }
            }
            if let Some(local) = clone["source"]
                .as_str()
                .filter(|s| Path::new(s).is_absolute())
            {
                access.directory(Path::new(local))?;
            }
            crate::workbench::forge::clone_jobs::model_start(
                app.clone(),
                p,
                string(&input, "title")?.into(),
                clone,
                |db, result| {
                    if !grant.active()
                        || Policy::current(db, grant.project, grant.chat) != Some(&policy)
                    {
                        return Err(refused("project grant changed during repository lookup"));
                    }
                    db.model_project_commands
                        .get_mut(&cmd)
                        .ok_or_else(|| refused("missing clone command"))?
                        .result = Some(result.clone());
                    Ok(())
                },
            )
            .await?
        }
        "kar_project_create" if input.get("workspace_root").is_none() => {
            if input.get("settings").is_some() || input.get("create_directory").is_some() {
                return Err(refused(
                    "set project options after creating the named workspace",
                ));
            }
            let parent = match input.get("parent") {
                Some(v) => access.directory(Path::new(
                    v.as_str().ok_or_else(|| refused("invalid parent"))?,
                ))?,
                None => access
                    .directories
                    .first()
                    .cloned()
                    .ok_or_else(|| refused("no project directory granted"))?,
            };
            let app = app.clone();
            let name = string(&input, "title")?.to_owned();
            tokio::task::spawn_blocking(move || {
                serde_json::to_value(app.create_named_at(&name, parent, Some(p))?)
                    .map_err(Error::from)
            })
            .await
            .map_err(|e| Error::Closed(e.to_string()))??
        }
        "kar_project_create" | "kar_project_update" | "kar_project_delete" => {
            if name != "kar_project_create" && !access.sees(grant, &app.project(p)?) {
                return Err(refused("project is outside this grant"));
            }
            if let Some(settings) = input.get("settings") {
                for (key, value) in settings
                    .as_object()
                    .ok_or_else(|| refused("invalid project settings"))?
                {
                    if matches!(key.as_str(), "type" | "commandId" | "projectId" | "force") {
                        return Err(refused("invalid project setting"));
                    }
                    if key == "workspaceRoot" {
                        access.directory(Path::new(
                            value
                                .as_str()
                                .ok_or_else(|| refused("invalid workspace root"))?,
                        ))?;
                    }
                    mutation[key] = value.clone();
                }
            }
            if name == "kar_project_create" {
                if input.get("parent").is_some() {
                    return Err(refused("choose a parent or a workspace root"));
                }
                mutation["title"] = json!(string(&input, "title")?);
                mutation["workspaceRoot"] =
                    json!(access.directory(Path::new(string(&input, "workspace_root")?))?);
                mutation["createWorkspaceRootIfMissing"] = input
                    .get("create_directory")
                    .cloned()
                    .unwrap_or(json!(false));
            } else if name == "kar_project_delete" {
                mutation["force"] = input.get("force").cloned().unwrap_or(json!(false));
            }
            serde_json::to_value(
                crate::workbench::project_lifecycle::model_mutation(
                    app.clone(),
                    mutation,
                    Some(grant.chat),
                )
                .await?,
            )?
        }
        _ => return Err(refused("unknown project action")),
    };
    app.update(|db| {
        db.model_project_commands
            .get_mut(&cmd)
            .ok_or_else(|| refused("missing project command"))?
            .result = Some(result.clone());
        Ok(())
    })?;
    Ok(result)
}
