//! Keep each repository's trees in the chat object database, including work outside its cwd.
use super::{ChatInfo, ChatStore, CheckpointPoint, err};
use crate::Result;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryScope {
    pub name: String,
    pub repository_path: PathBuf,
    pub worktree_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryCheckpoint {
    #[serde(flatten)]
    pub scope: RepositoryScope,
    pub before_tree: Option<String>,
    pub after_tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) type Lookup = Arc<dyn Fn(&ChatInfo) -> Result<Vec<RepositoryScope>> + Send + Sync>;

impl ChatStore {
    pub(crate) fn with_repository_scopes(mut self, lookup: Lookup) -> Self {
        self.repository_scopes = Some(lookup);
        self
    }

    pub(in crate::chat) fn scope_baselines(
        &self,
        info: &ChatInfo,
    ) -> Result<Vec<RepositoryCheckpoint>> {
        self.resumed_scopes(info, &[])
    }

    pub(in crate::chat) fn resumed_scopes(
        &self,
        info: &ChatInfo,
        previous: &[RepositoryCheckpoint],
    ) -> Result<Vec<RepositoryCheckpoint>> {
        let scopes = if let Some(lookup) = &self.repository_scopes {
            lookup(info)?
        } else {
            let mut scopes = Vec::new();
            let additional = info.settings.directories(&info.workspace)?;
            for saved in previous {
                let mut scope = saved.scope.clone();
                // Inherited turns retain the parent's journal paths. Use the destination
                // established by the fork before resolving any paths against the filesystem.
                if let Some(mapped) = info
                    .forked_from
                    .as_ref()
                    .and_then(|f| f.worktree_base.as_ref())
                    .and_then(|b| {
                        b.repositories
                            .iter()
                            .find(|r| r.scope.repository_path == scope.repository_path)
                    })
                {
                    scope = mapped.scope.clone();
                }
                self.check_cli_scope(info, &additional, &scope.worktree_path)?;
                scopes.push(scope);
            }
            for dir in &additional {
                if !info.workspace.starts_with(dir)
                    && dir.join(".git").exists()
                    && !scopes.iter().any(|s| s.worktree_path == *dir)
                {
                    self.check_cli_scope(info, &additional, dir)?;
                    scopes.push(RepositoryScope {
                        name: dir
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        repository_path: dir.clone(),
                        worktree_path: dir.clone(),
                    });
                }
            }
            scopes
        };
        Ok(scopes
            .into_iter()
            .map(|scope| RepositoryCheckpoint {
                scope,
                before_tree: None,
                after_tree: None,
                error: None,
            })
            .collect())
    }

    pub(super) fn check_cli_scope(
        &self,
        info: &ChatInfo,
        additional: &[PathBuf],
        path: &std::path::Path,
    ) -> Result<()> {
        if info.workspace.starts_with(path)
            || path.starts_with(&self.dir)
            || !std::iter::once(&info.workspace)
                .chain(additional.iter())
                .any(|root| path.starts_with(root))
        {
            return Err(err(format!(
                "tracked checkout is outside the chat directories: {}",
                path.display()
            )));
        }
        super::checked_root(path)?;
        if !path.join(".git").exists() {
            return Err(err(format!(
                "tracked checkout is unavailable: {}",
                path.display()
            )));
        }
        Ok(())
    }

    pub(super) fn capture_scopes(
        &self,
        repo: &gix::Repository,
        chat: &ChatInfo,
        point: CheckpointPoint,
        scopes: &mut [RepositoryCheckpoint],
    ) {
        for scope in scopes {
            if !point.before && scope.before_tree.is_none() {
                continue;
            }
            let result = (|| -> Result<String> {
                let work = &scope.scope.worktree_path;
                super::checked_root(work)?;
                if !work.join(".git").exists()
                    || work == &chat.workspace
                    || work.starts_with(&self.dir)
                {
                    return Err(err("tracked checkout is unavailable"));
                }
                let (_, tree) = self.write_workspace_tree(repo, work)?;
                let key = gix::objs::compute_hash(
                    repo.object_hash(),
                    gix::objs::Kind::Blob,
                    &gix::path::into_bstr(&scope.scope.repository_path),
                )
                .map_err(err)?;
                let side = if point.before { "before" } else { "after" };
                repo.reference(
                    format!(
                        "refs/kar/chats/{}/repositories/{}/{key}/{side}",
                        chat.id, point.turn
                    ),
                    tree,
                    gix::refs::transaction::PreviousValue::Any,
                    "Kar tracked repository checkpoint",
                )
                .map_err(err)?;
                Ok(tree.to_string())
            })();
            match result {
                Ok(tree) if point.before => scope.before_tree = Some(tree),
                Ok(tree) => scope.after_tree = Some(tree),
                Err(error) => scope.error = Some(error.to_string()),
            }
        }
    }
}

impl super::Capture {
    pub(in crate::chat) fn check_selected_repositories(
        &self,
        expected: &[RepositoryCheckpoint],
    ) -> Result<()> {
        for row in expected {
            let tree = row
                .after_tree
                .as_deref()
                .ok_or_else(|| err("incomplete selected repository checkpoint"))?;
            if !self.repositories.iter().any(|current| {
                current.scope.repository_path == row.scope.repository_path
                    && current.before_tree.as_deref() == Some(tree)
            }) {
                return Err(err(format!(
                    "repository changed since the selected fork checkpoint: {}",
                    row.scope.repository_path.display()
                )));
            }
        }
        Ok(())
    }
}
