use super::*;
use crate::chat::{Item, Role, TurnStatus};
use crate::workbench_git::git;
use serde::Serialize;
use std::io;

#[derive(Serialize)]
pub(in crate::workbench) struct Created {
    #[serde(flatten)]
    project: Project,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_error: Option<String>,
}

impl Workbench {
    // Managed workspaces must remain inside the host's authorization and outside its private store.
    fn managed_root(&self, name: &str) -> Result<PathBuf> {
        self.ensure_directory(&self.0.roots[0].path.join("Kar").join(name))
    }
    pub(in crate::workbench) fn ensure_scratch(&self) -> Result<Project> {
        self.update(|c| {
            if let Some(p) = c.lifecycle.scratch.and_then(|id| c.projects.get(&id)) {
                self.workspace_at(&p.root)?;
                return Ok(p.clone());
            }
            let root = self.managed_root("scratch")?;
            let p = c
                .projects
                .values()
                .find(|p| p.root == root)
                .cloned()
                .unwrap_or_else(|| Project {
                    id: Uuid::new_v4(),
                    root,
                    name: "No project".into(),
                    metadata: Default::default(),
                });
            c.projects.insert(p.id, p.clone());
            c.lifecycle.scratch = Some(p.id);
            Ok(p)
        })
    }
    fn claim_folder(&self, root: &Path, name: &str) -> Result<PathBuf> {
        let ws = self.workspace_at(root)?;
        let parent = ws.parent(Path::new(name), false)?;
        for i in 1..=100 {
            let name = if i == 1 {
                name.to_owned()
            } else {
                format!("{name}-{i}")
            };
            match parent.create_dir(&name) {
                Ok(()) => {
                    let cwd = root.join(name);
                    self.workspace_at(&cwd)?;
                    return Ok(cwd);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Refused(
            "all managed directory names are in use".into(),
        ))
    }
    pub(super) fn claim_scratch_chat(&self, root: &Path, id: Uuid) -> Result<PathBuf> {
        self.claim_folder(root, &format!("chat-{id}"))
    }
    pub(super) fn create_named(&self, name: &str) -> Result<Created> {
        self.create_named_at(name, self.managed_root("projects")?, None)
    }
    pub(in crate::workbench) fn create_named_at(&self, name: &str, parent: PathBuf, requested: Option<Uuid>) -> Result<Created> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 200 || name.contains(['\r', '\n', '\0']) {
            return Err(Error::Refused("invalid project name".into()));
        }
        let slug = name
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .to_lowercase();
        let mut slug = slug.chars().take(48).collect::<String>();
        if slug.is_empty() {
            slug.push_str("project");
        }
        if let Some(id) = requested {
            slug = format!("{slug}-{id}");
            if let Ok(p) = self.project(id) {
                if p.root != parent.join(&slug) || p.metadata.deleted { return Err(denied()); }
                return Ok(Created { project:p, commit_error:None });
            }
        }
        let stage = tempfile::Builder::new()
            .prefix(".kar-new-project-")
            .tempdir_in(&parent)?;
        let root = stage.path().to_path_buf();
        let ws = self.workspace_at(&root)?;
        let branch = git(&root, &["config", "--get", "init.defaultBranch"])
            .ok()
            .and_then(|v| String::from_utf8(v).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "main".into());
        git(&root, &["init", &format!("--initial-branch={branch}")])?;
        ws.publish_attachment(Path::new("README.md"),format!("<img src=\"assets/icon.svg\" width=\"64\" height=\"64\" alt=\"\">\n\n# {name}\n\nCreated in Kar.\n").as_bytes())?;
        let initial = name
            .chars()
            .find(|c| c.is_alphanumeric())
            .unwrap_or('K')
            .to_uppercase()
            .to_string();
        let icon = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64"><rect width="64" height="64" rx="12" fill="#334155"/><text x="32" y="43" text-anchor="middle" font-family="sans-serif" font-size="34" fill="white">{initial}</text></svg>"##
        );
        ws.publish_attachment(Path::new("assets/icon.svg"), icon.as_bytes())?;
        git(
            &root,
            &[
                "-c",
                "core.safecrlf=false",
                "add",
                "--force",
                "--",
                "README.md",
                "assets/icon.svg",
            ],
        )?;
        let commit_error = git(&root, &["commit", "--message", "Initial commit"])
            .err()
            .map(|e| {
                let msg = e.to_string();
                if msg.contains("identity unknown")
                    || msg.contains("tell me who you are")
                    || msg.contains("empty ident name")
                {
                    "Set Git user.name and user.email, then commit the staged files.".into()
                } else {
                    msg.lines()
                        .last()
                        .unwrap_or("Git could not create the initial commit")
                        .chars()
                        .take(512)
                        .collect()
                }
            });
        if let Some(id) = requested {
            git(&root, &["config","kar.projectRequest",&id.to_string()])?;
        }
        let project = self.publish_named(stage, &parent, &slug, name, requested)?;
        Ok(Created {
            project,
            commit_error,
        })
    }

    fn publish_named(
        &self,
        stage: tempfile::TempDir,
        parent: &Path,
        slug: &str,
        title: &str,
        requested: Option<Uuid>,
    ) -> Result<Project> {
        let workspace = self.workspace_at(parent)?;
        let dir = workspace.parent(Path::new("child"), false)?;
        let src = stage.path().file_name().ok_or_else(denied)?.to_owned();
        self.update(|catalog| {
            if let Some(id) = requested {
                if catalog.lifecycle.is_deleted(id) { return Err(denied()); }
                if let Some(p) = catalog.projects.get(&id) {
                    if p.root != parent.join(slug) || p.metadata.deleted { return Err(denied()); }
                    return Ok(p.clone());
                }
            }
            for i in 1..=if requested.is_some() {1} else {100} {
                let dest = if i == 1 {
                    slug.to_owned()
                } else {
                    format!("{slug}-{i}")
                };
                let root = parent.join(&dest);
                if let Some(project) = catalog.projects.values().find(|p| p.root == root) {
                    if requested == Some(project.id) && !project.metadata.deleted { return Ok(project.clone()); }
                    continue;
                }
                // Publish the fully prepared repo without overwriting even an empty existing folder.
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                let publish = rustix::fs::renameat_with(
                    &dir,
                    &src,
                    &dir,
                    dest.as_str(),
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(io::Error::from);
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                let publish = match dir.symlink_metadata(&dest) {
                    Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => dir.rename(&src, &dir, &dest),
                    Err(e) => Err(e),
                };
                match publish {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        let ours = requested.is_some_and(|id| root.canonicalize().ok().as_ref() == Some(&root)
                            && root.join(".git").is_dir() && git(&root,&["config","--local","--get","kar.projectRequest"]).ok()
                                .is_some_and(|s| String::from_utf8_lossy(&s).trim() == id.to_string()));
                        if !ours { continue; }
                    },
                    Err(e) => return Err(e.into()),
                }
                let project = Project {
                    id: requested.unwrap_or_else(Uuid::new_v4),
                    name: title.into(),
                    root,
                    metadata: Default::default(),
                };
                catalog.projects.insert(project.id, project.clone());
                return Ok(project);
            }
            Err(Error::Refused(
                "all managed project folder names are in use".into(),
            ))
        })

    }

    pub(super) fn promote_scratch_chat(&self, chat: Uuid) -> Result<Project> {
        self.update(|c| {
            let scratch = c
                .lifecycle
                .scratch
                .and_then(|s| c.projects.get(&s))
                .cloned()
                .ok_or_else(missing)?;
            let info = self.0.chats.saved_info(chat)?;
            let folder = &info.workspace;
            self.workspace_at(folder)?;
            if folder.parent() != Some(scratch.root.as_path()) {
                return Err(Error::Refused(
                    "only a Scratch chat with its own folder can become a project".into(),
                ));
            }
            self.remember_chats(c, &scratch)?;
            let existing = c.projects.values().find(|p| p.root == *folder).cloned();
            let owner = c.lifecycle.owners.get(&chat).ok_or_else(missing)?;
            if owner.project != scratch.id
                && existing.as_ref().is_none_or(|p| p.id != owner.project)
            {
                return Err(missing());
            }
            let chats: Vec<_> = c
                .lifecycle
                .owners
                .iter()
                .filter(|(_, b)| b.project == scratch.id && b.cwd == *folder)
                .map(|(id, _)| *id)
                .collect();
            let slots = chats
                .iter()
                .map(|id| self.controller(*id))
                .collect::<Vec<_>>();
            let mut guards = vec![];
            for (id, slot) in chats.iter().zip(&slots) {
                guards.push(slot.owner.try_lock().map_err(|_| Error::Busy)?);
                let h = match self.0.chats.history(*id) {
                    Ok(h) => h,
                    // A caller may have reserved a directory before native startup failed. Carry this
                    // unpublished ID to the new owner along with its folder. A staged fork must finish first.
                    Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                        if self.0.chats.fork_claimed(*id)? {
                            return Err(Error::Busy);
                        }
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                if h.turns.iter().any(|t| t.status == TurnStatus::Running)
                    || !h.pending_requests.is_empty()
                    || h.pending_rollback.is_some()
                    || h.pending_fork.is_some()
                    || self.0.chats.queued_inputs(*id)?.iter().any(|q| q.outstanding())
                {
                    return Err(Error::Busy);
                }
            }
            let p = match existing {
                Some(p) => p,
                None => {
                    let h = self.0.chats.history(chat)?;
                    let name = h
                        .turns
                        .iter()
                        .flat_map(|t| &t.items)
                        .find_map(|i| match &i.item {
                            Item::Message {
                                role: Role::User,
                                text,
                                ..
                            } if !text.trim().is_empty() => Some(
                                text.trim()
                                    .lines()
                                    .next()
                                    .unwrap_or_default()
                                    .chars()
                                    .take(128)
                                    .collect(),
                            ),
                            _ => None,
                        })
                        .unwrap_or_else(|| {
                            folder
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        });
                    Project {
                        id: Uuid::new_v4(),
                        name,
                        root: folder.clone(),
                        metadata: Default::default(),
                    }
                }
            };
            for id in chats {
                c.lifecycle.owners.get_mut(&id).expect("owned chat").project = p.id;
            }
            c.projects.insert(p.id, p.clone());
            Ok(p)
        })
    }
}
