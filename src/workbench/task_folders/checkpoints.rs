use super::{Workbench, folders};
use crate::{
    Result,
    chat::{ChatInfo, checkpoint::RepositoryScope},
};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

pub(in crate::workbench) fn repositories(
    app: &Workbench,
    chat: &ChatInfo,
) -> Result<Vec<RepositoryScope>> {
    let mut scopes = Vec::new();
    // A same-directory conversation fork retains its parent's managed task folder.
    let mut owner = chat.clone();
    let mut seen_chats = BTreeSet::new();
    let task = loop {
        if !seen_chats.insert(owner.id) {
            return Err(crate::Error::Protocol("cyclic chat lineage".into()));
        }
        let task_id = super::super::mcp::worktree::task_for(app, &owner).unwrap_or(owner.id);
        if let Some(row) = app.0.task_folders.read(task_id)?
            && app
                .0
                .task_folders
                .workspace(row.task.project_id, task_id)?
                .as_ref()
                == Some(&chat.workspace)
        {
            break Some(row.task);
        }
        let Some(parent) = owner.forked_from.as_ref() else {
            break None;
        };
        let prior = match app.0.chats.ancestry_info(parent.chat) {
            Ok(prior) => prior,
            Err(crate::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => break None,
            Err(e) => return Err(e),
        };
        if prior.workspace != chat.workspace {
            break None;
        }
        owner = prior;
    };
    if let Some(task) = task {
        scopes.extend(
            task.repositories
                .into_iter()
                .filter(|r| r.ready)
                .map(|r| RepositoryScope {
                    name: r.name,
                    repository_path: r.repository_path,
                    worktree_path: r.worktree_path,
                }),
        );
    } else if let Some(project) = {
        let catalog = app.0.catalog.lock().unwrap();
        let mut project = None;
        for p in catalog.projects.values() {
            if app.owns_chat(&catalog, p, chat)? {
                project = Some(super::super::Project {
                    root: chat.workspace.clone(),
                    ..p.clone()
                });
                break;
            }
        }
        project
    } {
        scopes.extend(
            folders::list(app, &project)?
                .into_iter()
                .filter(|f| f.state == "git")
                .map(|f| RepositoryScope {
                    name: f.name,
                    repository_path: f.path.clone(),
                    worktree_path: f.path,
                }),
        );
    }
    for dir in app.chat_directories(&chat.workspace, &chat.settings)? {
        if dir.join(".git").exists() && !scopes.iter().any(|s| s.worktree_path == dir) {
            scopes.push(RepositoryScope {
                name: dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                repository_path: dir.clone(),
                worktree_path: dir,
            });
        }
    }
    // The superproject records a gitlink, while the user can edit an initialized submodule's files.
    // Capture that checkout separately and retain its logical path across linked worktrees.
    let mut seen = BTreeSet::new();
    let mut roots: Vec<_> = scopes
        .iter()
        .map(|s| (s.repository_path.clone(), s.worktree_path.clone()))
        .collect();
    roots.push((chat.workspace.clone(), chat.workspace.clone()));
    while let Some((logical, work)) = roots.pop() {
        if !seen.insert(work.clone()) || !work.join(".git").exists() {
            continue;
        }
        app.allowed(&work)?;
        for path in submodule_paths(&work)? {
            let checkout = work.join(&path);
            if !checkout.join(".git").exists() || checkout.canonicalize()? != checkout {
                continue;
            }
            app.allowed(&checkout)?;
            let repository = logical.join(&path);
            roots.push((repository.clone(), checkout.clone()));
            if !scopes.iter().any(|s| s.worktree_path == checkout) {
                scopes.push(RepositoryScope {
                    name: path.to_string_lossy().into_owned(),
                    repository_path: repository,
                    worktree_path: checkout,
                });
            }
        }
    }
    scopes.retain(|s| !chat.workspace.starts_with(&s.worktree_path));
    if let Some(base) = chat
        .forked_from
        .as_ref()
        .and_then(|f| f.worktree_base.as_ref())
    {
        for scope in &mut scopes {
            if let Some(saved) = base
                .repositories
                .iter()
                .find(|r| r.scope.worktree_path == scope.worktree_path)
            {
                scope.repository_path = saved.scope.repository_path.clone();
            }
        }
    }
    scopes.sort_by(|a, b| a.repository_path.cmp(&b.repository_path));
    Ok(scopes)
}

fn submodule_paths(work: &Path) -> Result<Vec<PathBuf>> {
    let err = |e: String| crate::Error::Refused(format!("cannot inspect tracked submodules: {e}"));
    let repo = gix::open(work).map_err(|e| err(e.to_string()))?;
    let index = repo.index_or_empty().map_err(|e| err(e.to_string()))?;
    Ok(index
        .entries()
        .iter()
        .filter(|e| e.mode == gix::index::entry::Mode::COMMIT)
        .map(|e| gix::path::from_bstr(e.path(&index)).into_owned())
        .filter(|p| {
            p.components()
                .all(|c| matches!(c, Component::Normal(n) if !n.eq_ignore_ascii_case(".git")))
        })
        .collect())
}
