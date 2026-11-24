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
    if let Some(task) = app.0.task_folders.read(chat.id)?.map(|r| r.task)
        && app
            .0
            .task_folders
            .workspace(task.project_id, chat.id)?
            .as_ref()
            == Some(&chat.workspace)
    {
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
    } else if let Some(project) = app
        .projects()
        .into_iter()
        .find(|p| p.root == chat.workspace)
    {
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
    scopes.retain(|s| s.worktree_path != chat.workspace);
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
