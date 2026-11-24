//! Relocating a native conversation requires a provider-confirmed child workspace.
use super::*;
use crate::workbench_git::Repository;
use std::path::Path;

pub(super) struct Destination {
    pub workspace: PathBuf,
    pub id: Uuid,
    repo: Repository,
    _pin: File,
}

impl Destination {
    pub fn root(&self) -> &Path {
        self.repo.root()
    }

    pub fn open(parent: &ChatInfo, requested: &Path, opts: &mut SessionOptions) -> Result<Self> {
        if parent.protocol != Protocol::Codex {
            return Err(Error::Unsupported(format!(
                "forking {} into a different worktree requires a verified native directory override",
                parent.runtime
            )));
        }
        let source = Repository::open(&parent.workspace)?;
        let path = requested.canonicalize()?;
        let repo = Repository::open(&path)?;
        let expected = repo.root().join(
            parent
                .workspace
                .strip_prefix(source.root())
                .map_err(|_| Error::Refused("workspace is outside the source checkout".into()))?,
        );
        let workspace = expected.canonicalize()?;
        if !source.same_repository(&repo)
            || source.root() == repo.root()
            || workspace != expected
            || !(path == repo.root() || path == workspace)
            || source.root().starts_with(repo.root())
            || repo.root().starts_with(source.root())
        {
            return Err(Error::Refused(
                "fork destination must be a distinct linked checkout of the same repository".into(),
            ));
        }
        let (id, pin) = crate::workbench_git::pin_checkout(&workspace)?.ok_or_else(|| {
            Error::Refused("fork destination needs a Kar-managed worktree".into())
        })?;
        for dir in &mut opts.settings.additional_directories {
            if dir.is_absolute()
                && let Ok(relative) = dir.strip_prefix(source.root())
            {
                *dir = repo.root().join(relative);
            }
        }
        opts.cwd = workspace.clone();
        Ok(Self {
            workspace,
            id,
            repo,
            _pin: pin,
        })
    }

    pub fn baseline(&self, store: &ChatStore, parent: &ChatInfo) -> Result<ForkWorktreeBase> {
        let status = self.repo.status()?;
        if status
            .files
            .iter()
            .any(|entry| self.in_workspace(&entry.path))
        {
            return Err(Error::Refused(
                "fork destination contains local changes".into(),
            ));
        }
        let tree = store.fork_baseline(parent, &self.workspace)?;
        let after = self.repo.status()?;
        if after.branch != status.branch
            || after.head != status.head
            || after
                .files
                .iter()
                .any(|entry| self.in_workspace(&entry.path))
        {
            return Err(Error::Refused(
                "fork destination changed while preparing its baseline".into(),
            ));
        }
        Ok(ForkWorktreeBase {
            tree,
            head: status
                .head
                .ok_or_else(|| Error::Refused("fork destination has no Git HEAD".into()))?,
            branch: status
                .branch
                .ok_or_else(|| Error::Refused("fork destination has no local branch".into()))?,
        })
    }
    fn in_workspace(&self, relative: &Path) -> bool {
        self.repo.root().join(relative).starts_with(&self.workspace)
    }

    pub fn verify(&self, base: &ForkWorktreeBase) -> Result<()> {
        let status = self.repo.status()?;
        if status.head.as_deref() != Some(&base.head)
            || status.branch.as_deref() != Some(&base.branch)
            || status
                .files
                .iter()
                .any(|change| self.in_workspace(&change.path) && !matches!(change.index, '.' | '?'))
        {
            return Err(Error::Refused(
                "destination Git branch, HEAD or index changed during the fork".into(),
            ));
        }
        Ok(())
    }
}

pub(super) async fn confirm(session: &Session, workspace: &Path) -> Result<()> {
    let reported = session.info()["cwd"]
        .as_str()
        .or_else(|| session.info().pointer("/thread/cwd")?.as_str());
    if reported.is_some_and(|p| crate::opening::same_directory(p, workspace)) {
        return Ok(());
    }
    session.close().await?;
    Err(Error::Refused(
        "Codex did not confirm the destination worktree as the active directory".into(),
    ))
}

pub(super) fn no_conflicts(report: crate::chat::RollbackReport) -> Result<()> {
    if !report.conflicts.is_empty() {
        return Err(Error::Refused(format!(
            "workspace conflicts: {}",
            report
                .conflicts
                .iter()
                .map(|f| f.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}
