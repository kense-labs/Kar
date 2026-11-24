//! Git operations on an explicitly selected local checkout. All methods are blocking.
use crate::{Error, Result, chat::ChatStore};
use serde::Serialize;
use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
};

mod branches;
mod stacked;
pub(crate) use stacked::StackCheckout;
mod worktrees;
pub use branches::{Branch, BranchKind, BranchPage, BranchQuery};
pub use worktrees::ManagedWorktree;
pub(crate) use worktrees::pin_checkout;

#[derive(Debug, Clone)]
pub struct Repository {
    root: PathBuf,
    common: PathBuf,
    worktree_id: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub files: Vec<StatusEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusEntry {
    #[serde(with = "crate::chat::checkpoint::git_path")]
    pub path: PathBuf,
    pub index: char,
    pub worktree: char,
}

pub(crate) fn command(cwd: &Path) -> Command {
    let mut cmd = base_command(cwd);
    cmd.arg("--literal-pathspecs");
    cmd
}

fn base_command(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("KAR_WORKBENCH_TOKEN")
        .stdin(Stdio::null());
    // A Kar process launched by a Git hook may inherit another checkout's index.
    for key in [
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
        "GIT_PREFIX",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_QUARANTINE_PATH",
        "GIT_CEILING_DIRECTORIES",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

fn checked(output: Output) -> Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(Error::Refused(format!(
            "git ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        )))
    }
}
pub(crate) fn git<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> Result<Vec<u8>> {
    checked(command(cwd).args(args).output()?)
}
/// Git check-ignore forbids literal-pathspec mode. Prefix each individual filename
/// with ./ and use NUL framing, including for names beginning with a colon.
pub(crate) fn ignored_names<'a>(
    cwd: &Path,
    names: impl Iterator<Item = &'a OsStr>,
) -> Result<Vec<u8>> {
    use std::io::{Seek, Write};
    let mut paths = tempfile::tempfile()?;
    for name in names {
        paths.write_all(b"./")?;
        paths.write_all(name.as_encoded_bytes())?;
        paths.write_all(&[0])?;
    }
    paths.rewind()?;
    let out = base_command(cwd)
        .args([
            "-c",
            "core.fsmonitor=false",
            "check-ignore",
            "-z",
            "--stdin",
        ])
        .stdin(Stdio::from(paths))
        .output()?;
    if out.status.code() == Some(1) {
        Ok(Vec::new())
    } else {
        checked(out)
    }
}
fn line(buf: &[u8]) -> &[u8] {
    buf.strip_suffix(b"\n").unwrap_or(buf)
}
fn path(buf: &[u8]) -> Result<PathBuf> {
    gix::path::try_from_byte_slice(buf)
        .map(Path::to_owned)
        .map_err(|_| Error::Protocol("Git returned an unrepresentable path".into()))
}
fn utf8(buf: &[u8]) -> Result<&str> {
    std::str::from_utf8(buf)
        .map_err(|_| Error::Unsupported("Git reference name is not UTF-8".into()))
}

impl Repository {
    /// Discover the checkout containing this directory.
    pub fn open(dir: &Path) -> Result<Self> {
        let mut repo = Self::discover(dir)?;
        repo.worktree_id = pin_checkout(&repo.root)?.map(|(id, _)| id);
        Ok(repo)
    }

    fn discover(dir: &Path) -> Result<Self> {
        let root = path(line(&git(dir, &["rev-parse", "--show-toplevel"])?))?.canonicalize()?;
        let common = path(line(&git(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?))?
        .canonicalize()?;
        Ok(Self {
            root,
            common,
            worktree_id: None,
        })
    }

    pub(crate) fn same_repository(&self, other: &Self) -> bool {
        self.common == other.common
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn clone_into(source: &OsStr, destination: &Path) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        checked(
            command(&cwd)
                .args(["clone", "--"])
                .arg(source)
                .arg(destination)
                .output()?,
        )?;
        Self::open(destination)
    }

    pub fn status(&self) -> Result<Status> {
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let bytes = git(
            &self.root,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--branch",
                "--no-renames",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )?;
        let mut status = Status {
            head: None,
            branch: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
        };
        for field in bytes.split(|c| *c == 0).filter(|f| !f.is_empty()) {
            if let Some(id) = field.strip_prefix(b"# branch.oid ") {
                if id != b"(initial)" {
                    status.head = Some(utf8(id)?.to_owned());
                }
            } else if let Some(branch) = field.strip_prefix(b"# branch.head ") {
                if branch != b"(detached)" {
                    status.branch = Some(utf8(branch)?.to_owned());
                }
            } else if let Some(upstream) = field.strip_prefix(b"# branch.upstream ") {
                status.upstream = Some(utf8(upstream)?.to_owned());
            } else if let Some(ab) = field.strip_prefix(b"# branch.ab ") {
                let (a, b) = utf8(ab)?.split_once(" -").ok_or_else(bad_status)?;
                status.ahead = a
                    .strip_prefix('+')
                    .ok_or_else(bad_status)?
                    .parse()
                    .map_err(|_| bad_status())?;
                status.behind = b.parse().map_err(|_| bad_status())?;
            } else if let Some(name) = field.strip_prefix(b"? ") {
                status.files.push(StatusEntry {
                    path: path(name)?,
                    index: '?',
                    worktree: '?',
                });
            } else if !field.starts_with(b"# ") {
                let fields = match field[0] {
                    b'1' => 9,
                    b'u' => 11,
                    _ => return Err(bad_status()),
                };
                let mut parts = field.splitn(fields, |b| *b == b' ');
                let xy = parts
                    .nth(1)
                    .filter(|v| v.len() == 2)
                    .ok_or_else(bad_status)?;
                let name = parts.nth(fields - 3).ok_or_else(bad_status)?;
                status.files.push(StatusEntry {
                    path: path(name)?,
                    index: char::from(xy[0]),
                    worktree: char::from(xy[1]),
                });
            }
        }
        Ok(status)
    }

    /// Yield an initial snapshot and each observed status change, including external Git writes.
    /// Return false from the callback to stop. Slow readers receive the current state.
    pub fn watch(
        &self,
        interval: std::time::Duration,
        mut receive: impl FnMut(&Status) -> Result<bool>,
    ) -> Result<()> {
        if interval.is_zero() {
            return Err(Error::Refused("watch interval must be positive".into()));
        }
        let mut previous = None;
        loop {
            let next = self.status()?;
            if previous.as_ref() != Some(&next) {
                if !receive(&next)? {
                    return Ok(());
                }
                previous = Some(next);
            }
            std::thread::sleep(interval);
        }
    }

    /// Stage literal paths relative to this checkout, including deletions.
    pub fn stage(&self, paths: &[PathBuf]) -> Result<()> {
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        if paths.is_empty()
            || paths.iter().any(|p| {
                p.as_os_str().is_empty()
                    || p.components().any(|c| !matches!(c, Component::Normal(_)))
            })
        {
            return Err(Error::Refused(
                "stage requires literal checkout-relative paths".into(),
            ));
        }
        let _lease = ChatStore::workspace_lease_for(&self.root, None)?;
        checked(
            command(&self.root)
                .args(["add", "--"])
                .args(paths)
                .output()?,
        )?;
        Ok(())
    }

    pub fn commit(&self, message: &str) -> Result<String> {
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let _lease = ChatStore::workspace_lease_for(&self.root, None)?;
        git(&self.root, &["commit", "-m", message])?;
        self.resolve("HEAD")
    }

    fn resolve(&self, rev: &str) -> Result<String> {
        let value = git(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{rev}^{{commit}}"),
            ],
        )?;
        Ok(utf8(line(&value))?.into())
    }

    fn branch(&self) -> Result<String> {
        let value = git(&self.root, &["symbolic-ref", "--quiet", "HEAD"])?;
        utf8(line(&value))?
            .strip_prefix("refs/heads/")
            .map(str::to_owned)
            .ok_or_else(|| Error::Refused("checkout is not on a local branch".into()))
    }

    /// Publish the current branch to origin without forcing; establish its upstream.
    pub fn push(&self) -> Result<()> {
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let _lease = ChatStore::workspace_lease_for(&self.root, None)?;
        let branch = self.branch()?;
        git(
            &self.root,
            &[
                "push",
                "--set-upstream",
                "--",
                "origin",
                &format!("refs/heads/{branch}:refs/heads/{branch}"),
            ],
        )?;
        Ok(())
    }

    /// Fast-forward from the configured upstream. Divergence and conflicting local edits are preserved.
    pub fn pull(&self) -> Result<String> {
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let _lease = ChatStore::workspace_lease_for(&self.root, None)?;
        let branch = self.branch()?;
        let config = |key: &str| -> Result<String> {
            let bytes = git(
                &self.root,
                &[
                    "config",
                    "--null",
                    "--get-all",
                    &format!("branch.{branch}.{key}"),
                ],
            )?;
            let value = bytes
                .strip_suffix(&[0])
                .filter(|s| !s.is_empty() && !s.contains(&0))
                .ok_or_else(|| {
                    Error::Refused("pull requires a single configured upstream".into())
                })?;
            Ok(utf8(value)?.into())
        };
        let remote = config("remote")?;
        let upstream = config("merge")?;
        git(&self.root, &["check-ref-format", &upstream])?;
        git(&self.root, &["fetch", "--", &remote, &upstream])?;
        let fetched = self.resolve("FETCH_HEAD")?;
        git(
            &self.root,
            &[
                "merge",
                "--ff-only",
                "--no-autostash",
                "--no-overwrite-ignore",
                &fetched,
            ],
        )?;
        self.resolve("HEAD")
    }
}
fn bad_status() -> Error {
    Error::Protocol("malformed Git status record".into())
}

pub(crate) fn pin_saved_checkout(
    path: &Path,
    expected: Option<uuid::Uuid>,
) -> Result<Option<std::fs::File>> {
    let Some(expected) = expected else {
        return Ok(None);
    };
    match pin_checkout(path)? {
        Some((id, file)) if id == expected => Ok(Some(file)),
        _ => Err(Error::Refused(
            "saved worktree has been removed or replaced".into(),
        )),
    }
}
