use super::{ChatInfo, ChatStore, log::State};
use crate::{Error, Result};
use gix::{ObjectId, objs::tree::EntryKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

mod restore;
pub use restore::RollbackReport;

/// By default a turn selects the workspace immediately after that turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointPoint {
    pub turn: Uuid,
    pub before: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub parent: Option<CheckpointPoint>,
    pub before_tree: String,
    pub after_tree: Option<String>,
    pub files: Vec<FileChange>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    #[serde(with = "git_path")]
    pub path: PathBuf,
    pub kind: ChangeKind,
}
// Normal paths remain JSON strings. Unix Git paths need not be UTF-8.
mod git_path {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Text(String),
        Raw { base64: String },
    }
    pub fn serialize<S: serde::Serializer>(p: &Path, s: S) -> std::result::Result<S::Ok, S::Error> {
        let v = match p.to_str() {
            Some(p) => Wire::Text(p.into()),
            None => Wire::Raw {
                base64: STANDARD.encode(gix::path::into_bstr(p).as_ref() as &[u8]),
            },
        };
        v.serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<PathBuf, D::Error> {
        match Wire::deserialize(d)? {
            Wire::Text(s) => Ok(s.into()),
            Wire::Raw { base64 } => {
                let bytes = STANDARD.decode(base64).map_err(serde::de::Error::custom)?;
                gix::path::try_from_byte_slice(&bytes)
                    .map(Path::to_owned)
                    .map_err(serde::de::Error::custom)
            }
        }
    }
}
fn err(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(format!("workspace checkpoint: {e}")))
}
type Files = BTreeMap<PathBuf, Leaf>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Leaf {
    mode: EntryKind,
    id: ObjectId,
}
pub(super) struct Capture {
    pub tree: String,
    files: Files,
}
fn changes(old: &Files, new: &Files) -> Vec<FileChange> {
    old.keys()
        .chain(new.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|p| {
            let kind = match (old.get(p), new.get(p)) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(_), None) => ChangeKind::Deleted,
                (a, b) if a != b => ChangeKind::Modified,
                _ => return None,
            };
            Some(FileChange {
                path: p.clone(),
                kind,
            })
        })
        .collect()
}
impl Capture {
    pub fn changes(&self, next: &Self) -> Vec<FileChange> {
        changes(&self.files, &next.files)
    }
}
fn source_repo(work: &Path) -> Result<Option<gix::Repository>> {
    for p in work.ancestors() {
        match fs::symlink_metadata(p.join(".git")) {
            Ok(_) => return gix::open(p).map(Some).map_err(err),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}
fn checked_root(work: &Path) -> Result<()> {
    if work.canonicalize()? != work || !work.is_dir() {
        return Err(err("workspace moved or was replaced"));
    }
    Ok(())
}
fn protected(path: &Path) -> bool {
    path.components().any(|part| match part {
        Component::Normal(n) => n.eq_ignore_ascii_case(".git") || n.eq_ignore_ascii_case("git~1"),
        _ => true,
    })
}
fn blob_bytes(p: &Path, meta: &fs::Metadata) -> Result<Option<(EntryKind, Vec<u8>)>> {
    if meta.is_symlink() {
        let dest = fs::read_link(p)?;
        return Ok(Some((
            EntryKind::Link,
            gix::path::into_bstr(&dest).to_vec(),
        )));
    }
    if !meta.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o100 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some((
        if executable {
            EntryKind::BlobExecutable
        } else {
            EntryKind::Blob
        },
        fs::read(p)?,
    )))
}
impl ChatStore {
    fn odb_path(&self, chat: Uuid) -> PathBuf {
        self.dir.join(format!("{chat}.git"))
    }
    pub(super) fn workspace_lease(&self, work: &Path) -> Result<WorkspaceLease> {
        self.workspace_lease_for(work, None)
    }
    pub(super) fn workspace_lease_for(
        &self,
        work: &Path,
        recovering: Option<&Path>,
    ) -> Result<WorkspaceLease> {
        checked_root(work)?;
        let source = source_repo(work)?;
        let key = source
            .as_ref()
            .and_then(|r| r.workdir())
            .unwrap_or(work)
            .canonicalize()?;
        let digest = gix::objs::compute_hash(
            gix::hash::Kind::Sha1,
            gix::objs::Kind::Blob,
            &gix::path::into_bstr(&key),
        )
        .map_err(err)?;
        // One lease per checkout across chat stores. Linked worktrees have independent working files.
        let locks = std::env::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".local/state/oar/workspace-locks");
        let dir = ChatStore::open(locks)?;
        let f = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.dir.join(digest.to_string()))?;
        match f.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let marker = dir.dir.join(format!("{digest}.pending-fork"));
        if marker.try_exists()? {
            let stage: PathBuf = serde_json::from_reader(File::open(&marker)?)?;
            if !stage.try_exists()? {
                fs::remove_file(&marker)?;
            } else if Some(stage.as_path()) != recovering {
                let info = super::log::staged_info(File::open(&stage)?)?;
                let origin = info.forked_from.ok_or_else(|| err("missing fork origin"))?;
                return Err(Error::Refused(format!(
                    "workspace has a pending fork; retry turn {} of chat {} in {}",
                    origin.turn,
                    origin.chat,
                    stage.parent().unwrap_or(&stage).display()
                )));
            }
        }
        Ok(WorkspaceLease { _lock: f, marker })
    }
    pub(super) fn capture(
        &self,
        chat: &ChatInfo,
        point: CheckpointPoint,
        required_tree: Option<&str>,
    ) -> Result<Capture> {
        let odb = self.odb_path(chat.checkpoint_store());
        let repo = if odb.exists() {
            gix::open(&odb).map_err(err)?
        } else {
            gix::init_bare(&odb).map_err(err)?
        };
        let (files, id) = if let Some(required) = required_tree {
            let tree = restore::read_tree(&repo, required)?;
            let expected = restore::tree_files(&tree)?;
            let files = self.live_files(&repo, &chat.workspace)?;
            if files != expected {
                let paths = changes(&files, &expected)
                    .into_iter()
                    .map(|f| f.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(Error::Refused(format!(
                    "workspace changed since the selected fork checkpoint: {paths}"
                )));
            }
            (files, tree.id)
        } else {
            let files = self.scan(&chat.workspace, |bytes| {
                Ok(repo.write_blob(bytes).map_err(err)?.detach())
            })?;
            let mut tree = repo
                .edit_tree(ObjectId::empty_tree(repo.object_hash()))
                .map_err(err)?;
            for (relative, leaf) in &files {
                tree.upsert(
                    gix::path::to_unix_separators_on_windows(gix::path::into_bstr(relative))
                        .as_ref(),
                    leaf.mode,
                    leaf.id,
                )
                .map_err(err)?;
            }
            let id = tree.write().map_err(err)?.detach();
            (files, id)
        };
        Self::pin_tree(&repo, chat, point, id)?;
        Ok(Capture {
            tree: id.to_string(),
            files,
        })
    }
    pub(super) fn saved_capture(&self, chat: &ChatInfo, id: &str) -> Result<Capture> {
        let repo = gix::open(self.odb_path(chat.checkpoint_store())).map_err(err)?;
        let tree = restore::read_tree(&repo, id)?;
        Ok(Capture {
            tree: id.into(),
            files: restore::tree_files(&tree)?,
        })
    }
    pub(super) fn pin_capture(
        &self,
        chat: &ChatInfo,
        point: CheckpointPoint,
        capture: &Capture,
    ) -> Result<()> {
        let repo = gix::open(self.odb_path(chat.checkpoint_store())).map_err(err)?;
        Self::pin_tree(
            &repo,
            chat,
            point,
            ObjectId::from_hex(capture.tree.as_bytes()).map_err(err)?,
        )
    }
    fn pin_tree(
        repo: &gix::Repository,
        chat: &ChatInfo,
        point: CheckpointPoint,
        id: ObjectId,
    ) -> Result<()> {
        let side = if point.before { "before" } else { "after" };
        let prefix = if chat.forked_from.is_some() {
            format!("refs/oar/chats/{}/turns", chat.id)
        } else {
            "refs/oar/turns".to_owned()
        };
        repo.reference(
            format!("{prefix}/{}/{side}", point.turn),
            id,
            gix::refs::transaction::PreviousValue::Any,
            "Kar checkpoint",
        )
        .map_err(err)?;
        Ok(())
    }
    fn live_files(&self, repo: &gix::Repository, work: &Path) -> Result<Files> {
        self.scan(work, |bytes| {
            gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, &bytes).map_err(err)
        })
    }
    fn scan(&self, work: &Path, blob: impl Fn(Vec<u8>) -> Result<ObjectId>) -> Result<Files> {
        checked_root(work)?;
        if self.dir == work {
            return Err(err(
                "choose a chat store outside the workspace or in a subdirectory of it",
            ));
        }
        let mut files = Files::new();
        let mut add = |p: &Path| -> Result<()> {
            let relative = p.strip_prefix(work).map_err(err)?;
            if protected(relative) {
                return Err(err("invalid checkpoint path"));
            }
            if let Some((mode, bytes)) = blob_bytes(p, &fs::symlink_metadata(p)?)? {
                let id = blob(bytes)?;
                files.insert(relative.to_owned(), Leaf { mode, id });
            }
            Ok(())
        };
        if let Some(source) = source_repo(work)? {
            let base = source.workdir().ok_or_else(|| err("bare workspace"))?;
            let index = source.index_or_empty().map_err(err)?;
            let mut tracked = BTreeSet::new();
            let mut gitlinks = BTreeSet::new();
            for entry in index.entries() {
                let path = gix::path::from_bstr(entry.path(&index));
                if entry.mode == gix::index::entry::Mode::COMMIT {
                    gitlinks.insert(path.to_path_buf());
                }
                tracked.extend(path.ancestors().map(Path::to_owned));
            }
            let mut excludes = source
                .excludes(&index, None, Default::default())
                .map_err(err)?;
            let mut dirs = vec![work.to_owned()];
            while let Some(dir) = dirs.pop() {
                for child in fs::read_dir(dir)? {
                    let child = child?;
                    let path = child.path();
                    let ty = child.file_type()?;
                    let rel = path.strip_prefix(base).map_err(err)?;
                    if child.file_name().eq_ignore_ascii_case(".git")
                        || path == self.dir
                        || gitlinks.contains(rel)
                    {
                        continue;
                    }
                    let mode = ty.is_dir().then_some(gix::index::entry::Mode::DIR);
                    if !tracked.contains(rel) && excludes.at_path(rel, mode)?.is_excluded() {
                        continue;
                    }
                    if ty.is_dir() {
                        if !path.join(".git").symlink_metadata().is_ok() {
                            dirs.push(path)
                        }
                    } else {
                        add(&path)?
                    }
                }
            }
        } else {
            let store = self.dir.clone();
            let mut walk = ignore::WalkBuilder::new(work);
            walk.hidden(false)
                .ignore(false)
                .parents(false)
                .git_global(false)
                .require_git(false)
                .follow_links(false)
                .filter_entry(move |e| {
                    e.depth() == 0
                        || (e.path() != store
                            && !e.file_name().eq_ignore_ascii_case(".git")
                            && !(e.file_type().is_some_and(|t| t.is_dir())
                                && e.path().join(".git").symlink_metadata().is_ok()))
                });
            for e in walk.build() {
                let e = e.map_err(err)?;
                if e.file_type().is_some_and(|t| !t.is_dir()) {
                    add(e.path())?
                }
            }
        }
        Ok(files)
    }
}

/// A process lock serializes live writers. The reservation also survives a host crash,
/// so a different chat sharing this checkout cannot send a turn on a half-restored tree.
pub(super) struct WorkspaceLease {
    _lock: File,
    marker: PathBuf,
}
impl WorkspaceLease {
    pub fn finish_fork(&self) {
        // Publication already committed. An interrupted cleanup is retried by the next lease.
        let _ = fs::remove_file(&self.marker);
    }
    pub fn reserve_fork(&self, stage: &Path) -> Result<()> {
        let dir = self.marker.parent().expect("workspace lock directory");
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(tmp.as_file_mut(), stage)?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(&self.marker).map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    }
}
