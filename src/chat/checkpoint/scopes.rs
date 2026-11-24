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
        let Some(lookup) = &self.repository_scopes else {
            return Ok(Vec::new());
        };
        Ok(lookup(info)?
            .into_iter()
            .map(|scope| RepositoryCheckpoint {
                scope,
                before_tree: None,
                after_tree: None,
                error: None,
            })
            .collect())
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
