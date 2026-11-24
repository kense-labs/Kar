use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedWorktree {
    pub id: Uuid,
    #[serde(with = "crate::chat::checkpoint::git_path")]
    pub path: PathBuf,
    pub branch: String,
    pub start_commit: String,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Preparing,
    Ready,
    Removing,
    Removed,
}
#[derive(Serialize, Deserialize)]
struct Record {
    tree: ManagedWorktree,
    start: String,
    phase: Phase,
    #[serde(default)]
    existing_branch: bool,
    // Original creation arguments remain immutable for retries. A switch records both
    // possible HEADs before Git writes any working files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_head: Option<String>,
}

// A task checkout may run long-lived native filters or post-checkout hooks. Own that
// command's process group so cancellation reaches its workers as well as Git.
fn checkout_while_requested(
    cmd: &mut Command,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::io::{Read, Seek};
    use std::sync::atomic::Ordering;
    if cancel.load(Ordering::SeqCst) {
        return Err(refused("worktree setup cancelled"));
    }
    let mut log = tempfile::tempfile()?;
    cmd.stdout(Stdio::null())
        .stderr(log.try_clone()?)
        .env_remove("KAR_WORKBENCH_TOKEN");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    loop {
        if cancel.load(Ordering::SeqCst) {
            #[cfg(unix)]
            {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(refused("worktree setup cancelled"));
        }
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            log.rewind()?;
            let mut bytes = vec![];
            log.take(16 * 1024).read_to_end(&mut bytes)?;
            return Err(Error::Refused(format!(
                "git ({status}): {}",
                String::from_utf8_lossy(&bytes).trim_end()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
struct Registry {
    dir: PathBuf,
    _lock: File,
}
impl Registry {
    fn open(repo: &Repository) -> Result<Self> {
        let dir = repo.common.join("kar-worktrees");
        fs::create_dir_all(&dir)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("lock"))?;
        lock.lock()?;
        Ok(Self { dir, _lock: lock })
    }
    fn load(&self, id: Uuid) -> Result<Option<Record>> {
        match File::open(self.dir.join(format!("{id}.json"))) {
            Ok(f) => {
                let record: Record = serde_json::from_reader(f)?;
                if record.tree.id != id {
                    return Err(refused("worktree record has a different ID"));
                }
                Ok(Some(record))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn require_available(&self, path: &Path, branch: &str) -> Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let path_on_disk = entry?.path();
            if path_on_disk.extension() != Some(OsStr::new("json")) {
                continue;
            }
            let record: Record = serde_json::from_reader(File::open(path_on_disk)?)?;
            if record.phase != Phase::Removed
                && (record.tree.path == path || record.tree.branch == branch)
            {
                return Err(refused(
                    "another worktree request already reserves the branch or directory",
                ));
            }
        }
        Ok(())
    }
    fn save(&self, record: &Record) -> Result<()> {
        let mut f = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(f.as_file_mut(), record)?;
        f.as_file().sync_all()?;
        f.persist(self.dir.join(format!("{}.json", record.tree.id)))
            .map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

#[derive(Default)]
struct Registration {
    path: PathBuf,
    branch: Option<Vec<u8>>,
    lock: Option<Vec<u8>>,
    prunable: bool,
}
fn refused(s: &str) -> Error {
    Error::Refused(s.to_owned())
}
fn reason(id: Uuid) -> String {
    format!("kar-worktree-{id}")
}

const ID_FILE: &str = "kar-worktree-id";
fn marker_id(f: &File) -> Result<Uuid> {
    let mut buf = String::new();
    f.take(64).read_to_string(&mut buf)?;
    buf.parse()
        .map_err(|_| refused("invalid Kar worktree identity"))
}
fn install_marker(admin: &Path, id: Uuid) -> Result<()> {
    let marker = admin.join(ID_FILE);
    if let Some(f) = open_marker(&marker)? {
        if marker_id(&f)? != id {
            return Err(refused("worktree identity changed"));
        }
        return Ok(());
    }
    let mut tmp = tempfile::NamedTempFile::new_in(admin)?;
    write!(tmp, "{id}")?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(marker).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(admin)?.sync_all()?;
    Ok(())
}
fn open_marker(p: &Path) -> Result<Option<File>> {
    match File::open(p) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn lock_error(e: fs::TryLockError) -> Error {
    match e {
        fs::TryLockError::WouldBlock => refused("managed worktree is in use"),
        fs::TryLockError::Error(e) => e.into(),
    }
}
pub(crate) fn pin_checkout(dir: &Path) -> Result<Option<(Uuid, File)>> {
    for p in dir.ancestors() {
        let dot = p.join(".git");
        if !exists(&dot)? {
            continue;
        }
        if !dot.is_file() {
            return Ok(None);
        }
        let repo = gix::open(p).map_err(|e| Error::Refused(e.to_string()))?;
        let admin = repo.git_dir();
        if let Some(f) = open_marker(&admin.join(ID_FILE))? {
            f.try_lock_shared().map_err(lock_error)?;
            let id = marker_id(&f)?;
            let live = File::open(admin.join(ID_FILE))?;
            if marker_id(&live)? != id {
                return Err(refused("worktree identity changed"));
            }
            return Ok(Some((id, f)));
        }
        if let Some(mut f) = open_marker(&admin.join("locked"))? {
            let mut buf = Vec::new();
            (&mut f).take(64).read_to_end(&mut buf)?;
            if buf.starts_with(b"kar-worktree-") {
                return Err(refused("worktree preparation is incomplete"));
            }
        }
        return Ok(None);
    }
    Ok(None)
}

fn destination(path: &Path) -> Result<PathBuf> {
    let abs = std::env::current_dir()?.join(path);
    let name = abs
        .file_name()
        .ok_or_else(|| refused("worktree requires a new directory name"))?;
    Ok(abs
        .parent()
        .ok_or_else(|| refused("worktree requires a parent directory"))?
        .canonicalize()?
        .join(name))
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

impl Repository {
    fn registrations(&self) -> Result<Vec<Registration>> {
        let bytes = git(&self.root, &["worktree", "list", "--porcelain", "-z"])?;
        let mut rows = Vec::new();
        let mut row = Registration::default();
        for field in bytes.split(|b| *b == 0) {
            if field.is_empty() {
                if !row.path.as_os_str().is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
            } else if let Some(p) = field.strip_prefix(b"worktree ") {
                row.path = path(p)?;
            } else if let Some(b) = field.strip_prefix(b"branch ") {
                row.branch = Some(b.to_vec());
            } else if field == b"prunable" || field.starts_with(b"prunable ") {
                row.prunable = true;
            } else if field == b"locked" {
                row.lock = Some(Vec::new());
            } else if let Some(l) = field.strip_prefix(b"locked ") {
                row.lock = Some(l.to_vec());
            }
        }
        Ok(rows)
    }

    pub(super) fn branch_paths(&self) -> Result<std::collections::HashMap<Vec<u8>, PathBuf>> {
        Ok(self
            .registrations()?
            .into_iter()
            .filter(|r| !r.prunable)
            .filter_map(|r| Some((r.branch?, r.path)))
            .collect())
    }

    pub(super) fn changing_head(
        &self,
        expected: &str,
        action: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let Some(id) = self.worktree_id else {
            return action();
        };
        let registry = Registry::open(self)?;
        let mut record = registry
            .load(id)?
            .ok_or_else(|| refused("worktree has no creation record"))?;
        if record.phase != Phase::Ready {
            return Err(refused("worktree is not ready"));
        }
        let registration = || {
            self.registrations()?
                .into_iter()
                .find(|r| r.path == self.root)
                .ok_or_else(|| refused("Git registration is missing"))
        };
        let before = registration()?;
        self.verify_worktree(&record, &before, false)?;
        let before = before
            .branch
            .as_deref()
            .map(utf8)
            .transpose()?
            .unwrap_or("HEAD")
            .to_string();
        record.current_head = Some(before);
        record.pending_head = Some(expected.into());
        registry.save(&record)?;
        let result = action();
        let after = registration()?;
        self.verify_worktree(&record, &after, false)?;
        record.current_head = Some(
            after
                .branch
                .as_deref()
                .map(utf8)
                .transpose()?
                .unwrap_or("HEAD")
                .to_string(),
        );
        record.pending_head = None;
        registry.save(&record)?;
        result
    }

    fn verify_worktree(
        &self,
        record: &Record,
        entry: &Registration,
        unlocking: bool,
    ) -> Result<PathBuf> {
        let tree = &record.tree;
        let lock = reason(tree.id);
        let initial = format!("refs/heads/{}", tree.branch);
        let expected = record.current_head.as_deref().unwrap_or(&initial);
        let actual = entry.branch.as_deref().unwrap_or(b"HEAD");
        if (actual != expected.as_bytes()
            && record.pending_head.as_deref().map(str::as_bytes) != Some(actual))
            || (entry.lock.as_deref() != Some(lock.as_bytes())
                && !(unlocking && entry.lock.is_none()))
        {
            return Err(refused(
                "worktree branch or ownership lock changed; inspect the checkout",
            ));
        }
        let target = Self::discover(&tree.path)?;
        if target.root != tree.path || target.common != self.common {
            return Err(refused("worktree directory belongs to another checkout"));
        }
        // Both halves of the native registration must point to each other.
        let admin = path(line(&git(
            &target.root,
            &["rev-parse", "--absolute-git-dir"],
        )?))?
        .canonicalize()?;
        if admin.parent() != Some(self.common.join("worktrees").as_path()) {
            return Err(refused("refusing to manage the primary checkout"));
        }
        let backlink = path(line(&fs::read(admin.join("gitdir"))?))?;
        if admin.join(backlink).canonicalize()? != tree.path.join(".git") {
            return Err(refused("worktree administrative link changed"));
        }
        Ok(admin)
    }

    /// Create a new branch and an independent checkout. Repeat with the same ID and arguments
    /// after a disconnect. A completed removal permanently retires the ID.
    pub fn create_worktree(
        &self,
        id: Uuid,
        directory: &Path,
        branch: &str,
        start: &str,
    ) -> Result<ManagedWorktree> {
        self.create_worktree_for(id, directory, branch, start, false, None)
    }

    pub(crate) fn create_worktree_for(
        &self,
        id: Uuid,
        directory: &Path,
        branch: &str,
        start: &str,
        allow_existing: bool,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<ManagedWorktree> {
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let dir = destination(directory)?;
        let registry = Registry::open(self)?;
        let entries = self.registrations()?;
        let mut record = match registry.load(id)? {
            Some(r)
                if r.tree.path == dir
                    && r.tree.branch == branch
                    && r.start == start
                    && (!r.existing_branch || allow_existing) =>
            {
                r
            }
            Some(_) => return Err(refused("worktree ID already owns a different request")),
            None => {
                registry.require_available(&dir, branch)?;
                // Git accepts some revision shorthand for check-ref-format --branch. Only
                // literal, new local branches are valid for this operation.
                if branch.starts_with('-') || branch == "HEAD" || branch.is_empty() {
                    return Err(refused("invalid local branch name"));
                }
                git(
                    &self.root,
                    &["check-ref-format", &format!("refs/heads/{branch}")],
                )?;
                let local_branch = format!("refs/heads/{branch}");
                let existing_branch = self.ref_exists(&local_branch)?;
                if existing_branch && !allow_existing {
                    return Err(refused("branch already exists"));
                }
                if exists(&dir)?
                    || dir.starts_with(&self.common)
                    || entries
                        .iter()
                        .any(|e| dir.starts_with(&e.path) || e.path.starts_with(&dir))
                {
                    return Err(refused(
                        "worktree requires a new directory outside existing checkouts",
                    ));
                }
                let r = Record {
                    tree: ManagedWorktree {
                        id,
                        path: dir.clone(),
                        branch: branch.into(),
                        start_commit: self.resolve(if existing_branch {
                            &local_branch
                        } else {
                            start
                        })?,
                    },
                    start: start.into(),
                    existing_branch,
                    phase: Phase::Preparing,
                    current_head: None,
                    pending_head: None,
                };
                registry.save(&r)?;
                r
            }
        };
        if matches!(record.phase, Phase::Removing | Phase::Removed) {
            return Err(refused("worktree is being removed or has been retired"));
        }
        let admin = if let Some(entry) = entries.iter().find(|e| e.path == dir) {
            self.verify_worktree(&record, entry, false)?
        } else {
            if record.phase == Phase::Ready || exists(&dir)? {
                return Err(refused(
                    "registered checkout is missing or destination has been replaced",
                ));
            }
            if record.existing_branch {
                if self.resolve(&format!("refs/heads/{branch}"))? != record.tree.start_commit {
                    return Err(refused("the intended existing worktree branch moved"));
                }
            } else {
                self.reserve_branch(&record.tree)?;
            }
            let mut cmd = command(&self.root);
            cmd.args([
                "-c",
                "worktree.useRelativePaths=false",
                "worktree",
                "add",
                "--lock",
                "--reason",
                &reason(id),
            ]);
            cmd.arg("--").arg(&dir).arg(branch);
            if let Some(cancel) = cancel {
                checkout_while_requested(&mut cmd, cancel)?;
            } else {
                checked(cmd.output()?)?;
            }
            let entry = self
                .registrations()?
                .into_iter()
                .find(|e| e.path == dir)
                .ok_or_else(|| refused("Git did not register the checkout"))?;
            self.verify_worktree(&record, &entry, false)?
        };
        if record.phase == Phase::Preparing {
            // Git can leave an initializing registration if the Git process itself was
            // killed. Never mark an incomplete checkout as ready or reset a user's edits.
            let target = Self::discover(&dir)?;
            if target.resolve("HEAD")? != record.tree.start_commit
                || !target.status()?.files.is_empty()
            {
                return Err(refused(
                    "checkout preparation is incomplete or has local edits; inspect the worktree",
                ));
            }
            install_marker(&admin, id)?;
            record.phase = Phase::Ready;
            registry.save(&record)?;
        } else if marker_id(&File::open(admin.join(ID_FILE))?)? != id {
            return Err(refused("worktree identity changed"));
        }
        if !record.existing_branch {
            self.release_reservation(&record.tree)?;
        }
        Ok(record.tree)
    }

    fn reserve_branch(&self, tree: &ManagedWorktree) -> Result<()> {
        let branch = format!("refs/heads/{}", tree.branch);
        let reservation = format!("refs/kar/worktrees/{}", tree.id);
        if self.ref_exists(&reservation)? {
            if self.resolve(&reservation)? != tree.start_commit
                || self.resolve(&branch)? != tree.start_commit
            {
                return Err(refused("reserved branch moved during checkout preparation"));
            }
            return Ok(());
        }
        // The two creates share a Git reference transaction. Even a pre-existing branch at
        // exactly the requested commit cannot be mistaken for this request after a restart.
        let mut child = command(&self.root)
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let input = format!(
            "create {branch} {0}\ncreate {reservation} {0}\n",
            tree.start_commit
        );
        let write = child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input.as_bytes());
        checked(child.wait_with_output()?)?;
        write?;
        Ok(())
    }

    fn release_reservation(&self, tree: &ManagedWorktree) -> Result<()> {
        let reservation = format!("refs/kar/worktrees/{}", tree.id);
        if self.ref_exists(&reservation)? {
            git(
                &self.root,
                &["update-ref", "-d", &reservation, &tree.start_commit],
            )?;
        }
        Ok(())
    }

    pub(crate) fn ref_exists(&self, name: &str) -> Result<bool> {
        let out = command(&self.root)
            .args(["show-ref", "--verify", "--quiet", name])
            .output()?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => checked(out).map(|_| false),
        }
    }

    /// Remove only the registered checkout. Branches and their commits are retained.
    pub fn remove_worktree(&self, id: Uuid) -> Result<()> {
        self.remove_worktree_if(id, |_| Ok(()))
    }

    pub(crate) fn remove_worktree_if(
        &self,
        id: Uuid,
        authorize: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let registry = Registry::open(self)?;
        let mut r = registry
            .load(id)?
            .ok_or_else(|| refused("unknown Kar worktree ID"))?;
        authorize(&r.tree.path)?;
        if r.phase == Phase::Removed {
            return Ok(());
        }
        if r.phase == Phase::Preparing {
            return Err(refused("retry worktree preparation before removal"));
        }
        self.release_reservation(&r.tree)?;
        let path = &r.tree.path;
        let entry = self.registrations()?.into_iter().find(|e| &e.path == path);
        if let Some(entry) = entry {
            if self.root.starts_with(path) {
                return Err(refused("remove this worktree from another checkout"));
            }
            let admin = self.verify_worktree(&r, &entry, r.phase == Phase::Removing)?;
            let pin = File::open(admin.join(ID_FILE))?;
            pin.try_lock().map_err(lock_error)?;
            if marker_id(&pin)? != id {
                return Err(refused("worktree identity changed"));
            }
            let _lease = ChatStore::workspace_lease_for(path, None)?;
            let changes = git(
                path,
                &[
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.untrackedCache=false",
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--ignored",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ],
            )?;
            let flags = git(path, &["ls-files", "-v", "-z"])?;
            if !changes.is_empty()
                || flags.split(|b| *b == 0).any(|f| {
                    f.first()
                        .is_some_and(|c| *c == b'S' || c.is_ascii_lowercase())
                })
            {
                return Err(refused(
                    "worktree has local, ignored, or index-hidden files; cleanup refused",
                ));
            }
            r.phase = Phase::Removing;
            registry.save(&r)?;
            if entry.lock.is_some() {
                checked(
                    command(&self.root)
                        .args(["worktree", "unlock", "--"])
                        .arg(path)
                        .output()?,
                )?;
            }
            checked(
                command(&self.root)
                    .args(["worktree", "remove", "--"])
                    .arg(path)
                    .output()?,
            )?;
        } else if r.phase != Phase::Removing || exists(path)? {
            return Err(refused(
                "worktree registration changed; directory preserved",
            ));
        }
        r.phase = Phase::Removed;
        registry.save(&r)
    }
}
