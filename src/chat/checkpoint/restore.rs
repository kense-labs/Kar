use super::*;
use std::{collections::HashSet, io::Write};

#[derive(Debug, Serialize, Deserialize)]
pub struct RollbackReport {
    pub target: CheckpointPoint,
    pub files: Vec<FileChange>,
    pub conflicts: Vec<FileChange>,
    pub applied: bool,
}
type Plan = BTreeMap<PathBuf, (Option<Leaf>, Option<Leaf>)>;
fn ancestry(state: &State, mut point: Option<CheckpointPoint>) -> Result<Vec<CheckpointPoint>> {
    let mut result = vec![];
    let mut seen = HashSet::new();
    while let Some(p) = point {
        if !seen.insert(p) {
            return Err(err("checkpoint history contains a cycle"));
        }
        let cp = state
            .find(p.turn)
            .and_then(|t| t.checkpoint.as_ref())
            .ok_or_else(|| err("this turn has no workspace checkpoint"))?;
        if !p.before && cp.after_tree.is_none() {
            return Err(err("this turn has no final workspace checkpoint"));
        }
        result.push(p);
        point = if p.before {
            cp.parent
        } else {
            Some(CheckpointPoint { before: true, ..p })
        };
    }
    Ok(result)
}
pub(super) fn read_tree<'r>(repo: &'r gix::Repository, id: &str) -> Result<gix::Tree<'r>> {
    repo.find_tree(ObjectId::from_hex(id.as_bytes()).map_err(err)?)
        .map_err(err)
}
pub(super) fn tree_files(tree: &gix::Tree<'_>) -> Result<Files> {
    let mut files = Files::new();
    for entry in tree.traverse().breadthfirst.files().map_err(err)? {
        let mode = entry.mode.kind();
        if mode != EntryKind::Tree {
            let path = gix::path::try_from_byte_slice(&entry.filepath)
                .map_err(err)?
                .to_owned();
            files.insert(
                path,
                Leaf {
                    mode,
                    id: entry.oid,
                },
            );
        }
    }
    Ok(files)
}
fn leaf_at(tree: &gix::Tree<'_>, path: &Path) -> Result<Option<Leaf>> {
    if path.as_os_str().is_empty() || protected(path) {
        return Err(err("unsafe path in checkpoint"));
    }
    let Some(entry) = tree.lookup_entry_by_path(path).map_err(err)? else {
        return Ok(None);
    };
    match entry.mode().kind() {
        EntryKind::Tree => Ok(None),
        mode @ (EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link) => Ok(Some(Leaf {
            mode,
            id: entry.id().detach(),
        })),
        _ => Err(err("unsupported checkpoint tree entry")),
    }
}
fn plan(
    state: &State,
    repo: &gix::Repository,
    target: CheckpointPoint,
) -> Result<(Plan, BTreeSet<PathBuf>)> {
    let mut current = ancestry(state, state.position)?;
    let mut dest = ancestry(state, Some(target))?;
    for point in [state.position, Some(target)].into_iter().flatten() {
        if state
            .find(point.turn)
            .and_then(|t| t.checkpoint.as_ref())
            .is_some_and(|c| c.after_tree.is_none())
        {
            return Err(err(
                "the turn ended without a final workspace snapshot; its changes cannot be safely identified",
            ));
        }
    }
    while !current.is_empty() && current.last() == dest.last() {
        current.pop();
        dest.pop();
    }
    let mut plan = Plan::new();
    let mut conflicts = BTreeSet::new();

    for (p, undo) in current
        .into_iter()
        .map(|p| (p, true))
        .chain(dest.into_iter().rev().map(|p| (p, false)))
    {
        if p.before {
            if state
                .find(p.turn)
                .and_then(|t| t.checkpoint.as_ref())
                .is_some_and(|c| c.after_tree.is_none())
            {
                return Err(err(
                    "cannot cross a turn whose final workspace snapshot is missing",
                ));
            }
            continue;
        } // Between-turn edits are owned by the user.
        let cp = state.find(p.turn).unwrap().checkpoint.as_ref().unwrap();
        let after = cp.after_tree.as_ref().unwrap();
        let before_tree = read_tree(repo, &cp.before_tree)?;
        let after_tree = read_tree(repo, after)?;
        let (from, to) = if undo {
            (&after_tree, &before_tree)
        } else {
            (&before_tree, &after_tree)
        };
        for change in &cp.files {
            let path = &change.path;
            let old = leaf_at(from, path)?;
            let next = leaf_at(to, path)?;
            if old == next {
                return Err(err("changed path does not match its Git trees"));
            }
            let net = plan.entry(path.clone()).or_insert((old, old));
            if net.1 != old {
                conflicts.insert(path.clone());
            }
            net.1 = next;
        }
    }
    // An external edit can fall between adjacent turns. Undoing the next turn would
    // otherwise install its baseline and silently claim the older target was restored.
    let tree_at = |p: CheckpointPoint| {
        let checkpoint = state.find(p.turn).unwrap().checkpoint.as_ref().unwrap();
        read_tree(
            repo,
            if p.before {
                &checkpoint.before_tree
            } else {
                checkpoint.after_tree.as_ref().unwrap()
            },
        )
    };
    let dest_tree = tree_at(target)?;
    let origin_tree = state.position.map(tree_at).transpose()?;
    for (path, (from, to)) in &mut plan {
        let destination = leaf_at(&dest_tree, path)?;
        if *to != destination
            || origin_tree
                .as_ref()
                .map(|tree| leaf_at(tree, path))
                .transpose()?
                .is_some_and(|v| v != *from)
        {
            conflicts.insert(path.clone());
            *to = destination;
        }
    }
    plan.retain(|p, (a, b)| a != b || conflicts.contains(p));
    Ok((plan, conflicts))
}
fn kind(a: Option<Leaf>, b: Option<Leaf>) -> ChangeKind {
    match (a, b) {
        (None, Some(_)) => ChangeKind::Added,
        (Some(_), None) => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    }
}
enum Disk {
    Absent,
    Leaf(Leaf),
    Directory,
    Blocked,
}
fn disk(work: &Path, rel: &Path, plan: &Plan) -> Result<Disk> {
    let mut path = work.to_path_buf();
    for component in rel.components() {
        path.push(component);
        let m = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Disk::Absent),
            Err(e) => return Err(e.into()),
        };
        if m.is_dir() {
            if path.join(".git").symlink_metadata().is_ok() {
                return Ok(Disk::Blocked);
            }
            if path == work.join(rel) {
                return Ok(Disk::Directory);
            }
        } else if path != work.join(rel) {
            // Never read through a symlink. A planned file/directory swap removes this ancestor first.
            return Ok(
                if plan
                    .get(path.strip_prefix(work).map_err(err)?)
                    .is_some_and(|(a, b)| a.is_none() || b.is_none())
                {
                    Disk::Absent
                } else {
                    Disk::Blocked
                },
            );
        } else {
            return match blob_bytes(&path, &m)? {
                None => Ok(Disk::Blocked),
                Some((mode, bytes)) => Ok(Disk::Leaf(Leaf {
                    mode,
                    id: gix::objs::compute_hash(
                        gix::hash::Kind::Sha1,
                        gix::objs::Kind::Blob,
                        &bytes,
                    )
                    .map_err(err)?,
                })),
            };
        }
    }
    Ok(Disk::Blocked)
}
fn clearable(work: &Path, rel: &Path, plan: &Plan) -> Result<bool> {
    let mut dirs = vec![work.join(rel)];
    while let Some(dir) = dirs.pop() {
        for child in fs::read_dir(dir)? {
            let child = child?;
            let path = child.path();
            if child.file_name().eq_ignore_ascii_case(".git") {
                return Ok(false);
            }
            if child.file_type()?.is_dir() {
                dirs.push(path)
            } else if !plan
                .get(path.strip_prefix(work).map_err(err)?)
                .is_some_and(|(_, b)| b.is_none())
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
fn blob(repo: &gix::Repository, leaf: Leaf) -> Result<Vec<u8>> {
    let data = repo.find_blob(leaf.id).map_err(err)?.data.clone();
    if gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, &data).map_err(err)?
        != leaf.id
    {
        return Err(err("checkpoint blob failed hash verification"));
    }
    Ok(data)
}
fn mkdir_parents(work: &Path, rel: &Path) -> Result<()> {
    let mut p = work.to_owned();
    for c in rel.parent().unwrap().components() {
        p.push(c);
        match fs::create_dir(&p) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    && fs::symlink_metadata(&p)?.is_dir()
                    && !p.join(".git").try_exists()? => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn write_leaf(work: &Path, rel: &Path, leaf: Leaf, data: &[u8]) -> Result<()> {
    mkdir_parents(work, rel)?;
    let path = work.join(rel);
    let parent = path.parent().unwrap();
    if leaf.mode == EntryKind::Link {
        let temp = tempfile::tempdir_in(parent)?;
        let from = temp.path().join("link");
        let dest = gix::path::try_from_byte_slice(data).map_err(err)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(dest, &from)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(dest, &from)?;
        fs::rename(from, path)?;
    } else {
        let builder = tempfile::Builder::new();
        #[cfg(unix)]
        let builder = {
            let mut builder = builder;
            use std::os::unix::fs::PermissionsExt;
            // The OS applies the process umask when the temporary file is created.
            let permissions = fs::symlink_metadata(&path)
                .ok()
                .filter(|m| m.is_file())
                .map_or(0o666, |m| m.permissions().mode() & 0o777);
            let mode = if leaf.mode == EntryKind::BlobExecutable {
                permissions | 0o100 | ((permissions & 0o444) >> 2)
            } else {
                permissions & !0o111
            };
            builder.permissions(fs::Permissions::from_mode(mode));
            builder
        };
        let mut f = builder.tempfile_in(parent)?;
        f.write_all(data)?;
        f.as_file().sync_all()?;
        f.persist(path).map_err(|e| e.error)?;
    }
    Ok(())
}
impl ChatStore {
    /// Restore only file changes made across the selected turns. Dry runs and conflicts never write workspace files.
    pub fn rollback(
        &self,
        chat: Uuid,
        target: CheckpointPoint,
        dry_run: bool,
    ) -> Result<RollbackReport> {
        let mut log = self.locked_log(chat)?;
        self.rollback_locked(&mut log, target, dry_run)
    }
    pub(in crate::chat) fn rollback_locked(
        &self,
        log: &mut crate::chat::log::Log,
        target: CheckpointPoint,
        dry_run: bool,
    ) -> Result<RollbackReport> {
        if !dry_run
            && (log.state.pending_handoff().is_some()
                || log.state.compacting()
                || log.state.waiting_steering()
                || log.state.observing_notifications())
        {
            return Err(crate::Error::Busy);
        }
        let _workspace = self.workspace_lease(&log.state.info.workspace)?;
        let _pin = log.state.info.pin_worktree()?;
        self.require_no_pending_fork(log.state.info.id)?;
        self.restore_leased(log, target, dry_run, false, None)
    }
    /// The caller holds the workspace lease across validation, the native fork and publication.
    pub(in crate::chat) fn restore_leased(
        &self,
        log: &mut crate::chat::log::Log,
        target: CheckpointPoint,
        dry_run: bool,
        exact: bool,
        destination_base: Option<&str>,
    ) -> Result<RollbackReport> {
        let work = log.state.info.workspace.clone();
        if log.state.restoring.is_some_and(|p| p != target) {
            return Err(err(
                "retry the pending rollback to its original target first",
            ));
        }
        ancestry(&log.state, Some(target))?;
        let repo = gix::open(self.odb_path(log.state.info.checkpoint_store())).map_err(err)?;
        let expected = if exact {
            let checkpoint = log
                .state
                .find(target.turn)
                .and_then(|t| t.checkpoint.as_ref())
                .unwrap();
            let tree = if target.before {
                &checkpoint.before_tree
            } else {
                checkpoint.after_tree.as_ref().unwrap()
            };
            Some(tree_files(&read_tree(&repo, tree)?)?)
        } else {
            None
        };
        let (plan, mut conflicts) =
            if let (Some(base), Some(expected)) = (destination_base, &expected) {
                let baseline = tree_files(&read_tree(&repo, base)?)?;
                let mut plan = Plan::new();
                for change in changes(&baseline, expected) {
                    if protected(&change.path) {
                        return Err(err("unsafe path in fork checkpoint"));
                    }
                    let pair = (
                        baseline.get(&change.path).copied(),
                        expected.get(&change.path).copied(),
                    );
                    plan.insert(change.path, pair);
                }
                (plan, BTreeSet::new())
            } else {
                plan(&log.state, &repo, target)?
            };
        let mut files = vec![];
        let mut pending = vec![];
        for (path, &(from, to)) in &plan {
            let change = FileChange {
                path: path.clone(),
                kind: kind(from, to),
            };
            files.push(change);
            if self.dir.starts_with(&work) && work.join(path).starts_with(&self.dir) {
                conflicts.insert(path.clone());
                continue;
            }
            let live = match disk(&work, path, &plan)? {
                Disk::Leaf(l) => Some(l),
                Disk::Absent => None,
                Disk::Directory if to.is_none() || clearable(&work, path, &plan)? => None,
                _ => {
                    conflicts.insert(path.clone());
                    continue;
                }
            };
            if live == to {
                conflicts.remove(path);
                continue;
            }
            if live != from {
                conflicts.insert(path.clone());
                continue;
            }
            if let Some(leaf) = to {
                blob(&repo, leaf)?;
            }
            pending.push(path);
        }
        let mut report = RollbackReport {
            target,
            conflicts: files
                .iter()
                .filter(|f| conflicts.contains(&f.path))
                .cloned()
                .collect(),
            files,
            applied: false,
        };
        if let Some(expected) = &expected {
            report.conflicts.extend(
                changes(&self.live_files(&repo, &work)?, expected)
                    .into_iter()
                    .filter(|file| !plan.contains_key(&file.path)),
            );
        }
        if dry_run || !report.conflicts.is_empty() {
            return Ok(report);
        }
        log.start_restore(target)?;
        // Delete leaves before installing new ones, never recursively remove a directory.
        for &path in &pending {
            let (from, to) = plan[path];
            if to.is_none() {
                if !matches!(disk(&work,path,&plan)?,Disk::Leaf(l) if Some(l)==from) {
                    return Err(err(format!("{} changed during restore", path.display())));
                }
                fs::remove_file(work.join(path))?;
                for parent in path
                    .ancestors()
                    .skip(1)
                    .take_while(|p| !p.as_os_str().is_empty())
                {
                    if fs::remove_dir(work.join(parent)).is_err() {
                        break;
                    }
                }
            }
        }
        for path in pending {
            let (from, to) = plan[path];
            let Some(leaf) = to else { continue };
            match disk(&work, path, &plan)? {
                Disk::Absent if from.is_none() => {}
                Disk::Leaf(l) if Some(l) == from => {}
                Disk::Directory if from.is_none() => {
                    remove_empty_dirs(&work.join(path))?;
                }
                _ => return Err(err(format!("{} changed during restore", path.display()))),
            }
            write_leaf(&work, path, leaf, &blob(&repo, leaf)?)?;
        }
        if let Some(expected) = expected
            && self.live_files(&repo, &work)? != expected
        {
            return Err(err(
                "workspace changed during fork; reconcile local edits and retry the same turn",
            ));
        }
        log.finish_restore(target)?;
        report.applied = true;
        Ok(report)
    }
}
fn remove_empty_dirs(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().eq_ignore_ascii_case(".git") {
            return Err(err("directory contains local work"));
        }
        remove_empty_dirs(&entry.path())?;
    }
    Ok(fs::remove_dir(path)?)
}
