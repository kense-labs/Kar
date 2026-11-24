use super::*;
use crate::workbench::task_folders::{self, Tracked};
use std::collections::BTreeSet;

fn bad(message: &str) -> Error {
    Error::Refused(message.into())
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty() && v.trim() == *v)
        .ok_or_else(|| bad(&format!("invalid {key}")))
}
fn boolean(v: &Value, k: &str) -> Result<Option<bool>> {
    v.get(k)
        .map(|x| x.as_bool().ok_or_else(|| bad(&format!("invalid {k}"))))
        .transpose()
}
fn allowed_keys(v: &Value, keys: &[&str]) -> Result<()> {
    for k in v.as_object().ok_or_else(|| bad("expected object"))?.keys() {
        if !keys.contains(&k.as_str()) {
            return Err(bad(&format!("unexpected project field {k}")));
        }
    }
    Ok(())
}

fn settings(input: &Value, p: &mut Project) -> Result<()> {
    for key in [
        "defaultModelSelection",
        "autoPull",
        "projectIcon",
        "faviconPath",
        "defaultThreadEnvMode",
        "scripts",
    ] {
        let Some(v) = input.get(key) else {
            continue;
        };
        match key {
            "scripts" => {
                for script in v.as_array().ok_or_else(|| bad("invalid project scripts"))? {
                    for k in ["id", "name", "command"] {
                        string(script, k)?;
                    }
                    if !matches!(
                        string(script, "icon")?,
                        "play" | "test" | "lint" | "configure" | "build" | "debug"
                    ) || boolean(script, "runOnWorktreeCreate")?.is_none()
                    {
                        return Err(bad("invalid project script"));
                    }
                    for k in ["runOnSettle", "stopOnSettle", "async", "autoOpenPreview"] {
                        boolean(script, k)?;
                    }
                    if script.get("previewUrl").is_some() {
                        string(script, "previewUrl")?;
                    }
                }
            }
            "autoPull" => {
                boolean(input, key)?;
            }
            "faviconPath" | "defaultThreadEnvMode" if !v.is_null() => {
                string(input, key)?;
            }
            "defaultModelSelection" | "projectIcon" if !(v.is_object() || v.is_null()) => {
                return Err(bad("invalid project setting"));
            }
            _ => {}
        }
        p.metadata.settings.insert(key.into(), v.clone());
    }
    if let Some(b) = boolean(input, "pinned")? {
        p.metadata.pinned = b;
    }
    if let Some(b) = boolean(input, "archived")? {
        p.metadata.archived = b;
    }
    Ok(())
}

impl Workbench {
    pub(super) fn mutate_project(&self, input: Value) -> Result<Project> {
        let op = string(&input, "type")?;
        let cmd = string(&input, "commandId")?;
        let id =
            Uuid::parse_str(string(&input, "projectId")?).map_err(|_| bad("invalid project ID"))?;
        let mut keys = vec!["type", "commandId", "projectId"];
        match op {
            "project.create" => keys.extend([
                "title",
                "workspaceRoot",
                "createWorkspaceRootIfMissing",
                "defaultModelSelection",
                "scripts",
                "trackedFolders",
            ]),
            "project.update" => keys.extend([
                "title",
                "workspaceRoot",
                "defaultModelSelection",
                "autoPull",
                "projectIcon",
                "faviconPath",
                "defaultThreadEnvMode",
                "scripts",
                "trackedFolders",
                "trackFolders",
                "untrackFolders",
                "pinned",
                "archived",
            ]),
            "project.delete" => keys.push("force"),
            _ => return Err(bad("unknown project mutation")),
        }
        allowed_keys(&input, &keys)?;
        self.update(|c| {
            if let Some((saved, result)) = c.lifecycle.receipts.get(cmd) {
                if saved != &input {
                    return Err(bad("command ID belongs to a different project mutation"));
                }
                self.allowed(&result.root)?;
                return Ok(result.clone());
            }
            let existing = c.projects.get(&id).cloned();
            let mut p = match op {
                "project.create" => {
                    if existing.is_some() || c.lifecycle.deleted_projects.contains(&id) {
                        return Err(bad("project ID already used"));
                    }
                    let name = string(&input, "title")?.to_owned();
                    let path = Path::new(string(&input, "workspaceRoot")?);
                    let root = if boolean(&input, "createWorkspaceRootIfMissing")? == Some(true) {
                        self.ensure_directory(path)?
                    } else {
                        path.canonicalize()?
                    };
                    Project {
                        id,
                        name,
                        root,
                        metadata: Default::default(),
                    }
                }
                _ => existing.clone().ok_or_else(missing)?,
            };
            self.allowed(&p.root)?;
            if op == "project.delete" {
                self.remember_chats(c, &p)?;
                let ids: Vec<_> = c
                    .lifecycle
                    .owners
                    .iter()
                    .filter(|(_, b)| b.project == id)
                    .map(|(chat, _)| *chat)
                    .collect();
                if !ids.is_empty() && boolean(&input, "force")? != Some(true) {
                    return Err(bad("project contains chats"));
                }
                let slots: Vec<_> = ids.iter().map(|i| self.controller(*i)).collect();
                let mut guards = vec![];
                for slot in &slots {
                    guards.push(slot.owner.try_lock().map_err(|_| Error::Busy)?);
                }
                // Retain the ownership tombstones: registering the same directory must never expose deleted chats.
                c.archived.extend(ids);
                c.lifecycle.deleted_projects.insert(id);
                c.projects.remove(&id);
                c.tracked_folders.remove(&id);
                if c.lifecycle.scratch == Some(id) {
                    c.lifecycle.scratch = None;
                }
                p.metadata.deleted = true;
            } else {
                if input.get("title").is_some() {
                    p.name = string(&input, "title")?.into();
                }
                if op == "project.update" && input.get("workspaceRoot").is_some() {
                    let root = Path::new(string(&input, "workspaceRoot")?).canonicalize()?;
                    if root != p.root {
                        if c.lifecycle.scratch == Some(id) {
                            return Err(bad("managed Scratch root cannot move"));
                        }
                        self.remember_chats(c, &p)?;
                        p.root = root;
                    }
                }
                self.workspace_at(&p.root)?;
                if c.projects
                    .values()
                    .any(|other| other.id != id && other.root == p.root)
                {
                    return Err(bad("folder belongs to another project"));
                }
                settings(&input, &mut p)?;
                if input.get("trackedFolders").is_some()
                    || input.get("trackFolders").is_some()
                    || input.get("untrackFolders").is_some()
                    || existing.is_some_and(|old| old.root != p.root)
                {
                    let mut folders: Vec<Tracked> = match input.get("trackedFolders") {
                        Some(v) => serde_json::from_value(v.clone())?,
                        None => c.tracked_folders.get(&id).cloned().unwrap_or_default(),
                    };
                    if let Some(raw) = input.get("untrackFolders") {
                        let paths: BTreeSet<PathBuf> = serde_json::from_value(raw.clone())?;
                        folders.retain(|f| !paths.contains(&f.path));
                    }
                    if let Some(raw) = input.get("trackFolders") {
                        let additions: Vec<Tracked> = serde_json::from_value(raw.clone())?;
                        for f in additions {
                            if !folders.iter().any(|e| e.path == f.path) {
                                folders.push(f);
                            }
                        }
                    }
                    c.tracked_folders
                        .insert(id, task_folders::normalize(self, &p, folders)?);
                }
                c.projects.insert(id, p.clone());
            }
            c.lifecycle
                .receipts
                .insert(cmd.into(), (input.clone(), p.clone()));
            Ok(p)
        })
    }
}
