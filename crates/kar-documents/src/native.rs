//! Stage one registered document for one native agent turn. The caller
//! configures the provider's filesystem policy and manages its session.
use crate::store::{Action, Command, DocumentStore, Receipt, Rejection, atomic_write, hash};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRequest {
    pub author: String,
    pub operation_id: String,
    pub base_revision: u64,
    pub runtime: String,
    pub model: Option<String>,
    pub instructions: String,
}

impl NativeRequest {
    pub fn prompt(&self, file_name: &str) -> String {
        format!(
            "Edit the provided copy in the current directory and save the complete result to {file_name}.\n\n{}",
            self.instructions
        )
    }

    /// Stable identifier for Session::prompt_with_id and its native turn receipt.
    pub fn input_id(&self, document_id: &str) -> Uuid {
        let mut digest = Sha256::new();
        digest.update(b"kar-document-input/1\0");
        digest.update(document_id.as_bytes());
        digest.update([0]);
        digest.update(self.operation_id.as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.finalize()[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes)
    }
    fn command(&self, digest: String) -> Command {
        Command {
            author: self.author.clone(),
            operation_id: self.operation_id.clone(),
            base_revision: self.base_revision,
            action: Action::Save { sha256: digest },
        }
    }
}

pub enum NativeBegin {
    Ready(NativeJob),
    Finished(Receipt),
    Running,
    /// Server-only recovery context. The path is the retained copy, and
    /// prompt is the exact text dispatched for this operation.
    Interrupted {
        native_session_id: Option<String>,
        cwd: PathBuf,
        prompt: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct Workspace {
    path: PathBuf,
    device: u64,
    inode: u64,
    filename: String,
}

impl Workspace {
    fn create(extension: &str, bytes: &[u8]) -> Result<(Self, File)> {
        let scratch = tempfile::Builder::new()
            .prefix("kar-document-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let directory = directory(scratch.path())?;
        let stat = directory.metadata()?;
        let filename = format!("document.{extension}");
        let mut output = File::from(openat(
            &directory,
            filename.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        output.write_all(bytes)?;
        output.sync_all()?;
        directory.sync_all()?;
        let path = scratch.keep();
        Ok((
            Self {
                path,
                device: stat.dev(),
                inode: stat.ino(),
                filename,
            },
            directory,
        ))
    }
    fn reopen(&self) -> Result<File> {
        let d = directory(&self.path)?;
        let info = d.metadata()?;
        ensure!(
            info.dev() == self.device && info.ino() == self.inode,
            "native edit workspace was replaced"
        );
        Ok(d)
    }
    fn remove(&self) {
        // An editor may have removed or replaced this path. Never clean up
        // a different directory, even if it now has the same name.
        if self.reopen().is_ok() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
    fn output(&self, directory: &File) -> Result<Vec<u8>> {
        let fd = openat(
            directory,
            self.filename.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut f = File::from(fd);
        let before = f.metadata()?;
        ensure!(
            before.is_file() && before.nlink() == 1,
            "native output must be a private regular file"
        );
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let after = f.metadata()?;
        ensure!(
            before.len() == after.len()
                && after.len() == bytes.len() as u64
                && before.modified()? == after.modified()?,
            "native output changed during capture"
        );
        Ok(bytes)
    }
}

fn directory(path: &Path) -> Result<File> {
    Ok(File::from(open(
        path,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

#[derive(Clone, Serialize, Deserialize)]
enum Phase {
    Prepared,
    Dispatched { session_id: Option<String> },
    Candidate { sha256: String },
    Failed { reason: String },
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    request: NativeRequest,
    document: String,
    workspace: Workspace,
    phase: Phase,
}
impl Entry {
    fn persist(&self, slot: &Path) -> Result<()> {
        atomic_write(&slot.join("run.json"), &serde_json::to_vec(self)?)
    }
    fn read(slot: &Path) -> Result<Option<Self>> {
        let path = slot.join("run.json");
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

fn slot_path(state: &Path, operation: &str) -> PathBuf {
    state.join("native-edits").join(hash(operation.as_bytes()))
}

fn try_claim(slot: &Path) -> Result<Option<File>> {
    DirBuilder::new().recursive(true).mode(0o700).create(slot)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(slot.join("run.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

// A normal file save may not steal the ID of an in-flight native turn.
// The only accepted save for it is the candidate journaled by finish().
pub(crate) fn check_reservation(state: &Path, command: &Command) -> Result<()> {
    if let Some(entry) = Entry::read(&slot_path(state, &command.operation_id))? {
        if let Phase::Candidate { sha256 } = entry.phase
            && command.author == entry.request.author
            && command.base_revision == entry.request.base_revision
            && matches!(&command.action,Action::Save{sha256:d} if d==&sha256)
        {
            return Ok(());
        }
        return Err(Rejection::ReusedId.into());
    }
    Ok(())
}

pub struct NativeJob {
    slot: PathBuf,
    entry: Entry,
    _lock: File,
    directory: File,
}

impl NativeJob {
    /// The isolated working directory. It is not an operating-system sandbox.
    pub fn cwd(&self) -> &Path {
        &self.entry.workspace.path
    }
    pub fn filename(&self) -> &str {
        &self.entry.workspace.filename
    }
    pub fn input_id(&self) -> Uuid {
        self.entry.request.input_id(&self.entry.document)
    }
    pub fn prompt(&self) -> String {
        self.entry.request.prompt(self.filename())
    }
    /// Persist admission intent before dispatching the prompt. An interrupted
    /// turn must be reconciled using the same native session and input ID.
    pub fn mark_dispatched(&mut self, native_session_id: Option<String>) -> Result<()> {
        ensure!(
            matches!(self.entry.phase, Phase::Prepared),
            "native prompt already dispatched"
        );
        self.entry.phase = Phase::Dispatched {
            session_id: native_session_id,
        };
        self.entry.persist(&self.slot)
    }
    /// Call only after the provider confirmed this input's completed turn
    /// and finished writing the file. The same working document can receive
    /// human edits while the native turn is active.
    pub fn finish(mut self, store: &mut DocumentStore) -> Result<Receipt> {
        ensure!(
            matches!(self.entry.phase, Phase::Dispatched { .. }),
            "native edit was not dispatched"
        );
        ensure!(
            self.slot == slot_path(store.native_state_dir(), &self.entry.request.operation_id),
            "wrong document store for native edit"
        );
        let candidate = self.entry.workspace.output(&self.directory)?;
        let digest = store.stage(&candidate)?;
        self.entry.phase = Phase::Candidate {
            sha256: digest.clone(),
        };
        self.entry.persist(&self.slot)?;
        let result = store.submit(self.entry.request.command(digest))?;
        self.entry.workspace.remove();
        Ok(result)
    }
    /// Record a known native failure or cancellation without publishing the copy.
    pub fn fail(mut self, reason: String) -> Result<()> {
        ensure!(!reason.trim().is_empty(), "failure reason required");
        self.entry.phase = Phase::Failed { reason };
        self.entry.persist(&self.slot)?;
        self.entry.workspace.remove();
        Ok(())
    }
}

impl Drop for NativeJob {
    fn drop(&mut self) {
        if matches!(self.entry.phase, Phase::Prepared) {
            self.entry.workspace.remove();
        }
    }
}

impl DocumentStore {
    pub fn begin_native(&mut self, request: NativeRequest) -> Result<NativeBegin> {
        ensure!(
            !request.author.trim().is_empty()
                && !request.operation_id.trim().is_empty()
                && !request.runtime.trim().is_empty()
                && !request.instructions.trim().is_empty(),
            "native author, operation ID, runtime and instructions required"
        );
        let slot = slot_path(self.native_state_dir(), &request.operation_id);
        let claim = try_claim(&slot)?;
        let entry = Entry::read(&slot)?;
        if entry.as_ref().is_some_and(|old| old.request != request) {
            return Err(Rejection::ReusedId.into());
        }
        let Some(lock) = claim else {
            return Ok(NativeBegin::Running);
        };
        if let Some(entry) = entry {
            match entry.phase {
                Phase::Candidate { sha256 } => {
                    let receipt = self.submit(request.command(sha256))?;
                    entry.workspace.remove();
                    return Ok(NativeBegin::Finished(receipt));
                }
                Phase::Dispatched { session_id } => {
                    entry
                        .workspace
                        .reopen()
                        .context("retained native workspace is unavailable or has changed")?;
                    let prompt = entry.request.prompt(&entry.workspace.filename);
                    return Ok(NativeBegin::Interrupted {
                        native_session_id: session_id,
                        cwd: entry.workspace.path,
                        prompt,
                    });
                }
                Phase::Failed { reason } => {
                    entry.workspace.remove();
                    return Ok(NativeBegin::Failed { reason });
                }
                Phase::Prepared => entry.workspace.remove(),
            }
        } else if self.operation_exists(&request.operation_id)? {
            return Err(Rejection::ReusedId.into());
        }
        let base = self.snapshot(request.base_revision)?;
        let document = self.state()?.document;
        let (workspace, directory) = Workspace::create(&self.native_file_extension()?, &base)?;
        let entry = Entry {
            request,
            document,
            workspace,
            phase: Phase::Prepared,
        };
        let job = NativeJob {
            entry,
            slot,
            _lock: lock,
            directory,
        };
        job.entry.persist(&job.slot)?;
        Ok(NativeBegin::Ready(job))
    }

    /// Resume capture of an interrupted proposal after the caller verified
    /// a completed native turn for request.input_id(document_id). This never
    /// resends the prompt or substitutes a new directory.
    pub fn recover_native(
        &mut self,
        request: &NativeRequest,
        native_session_id: &str,
    ) -> Result<NativeJob> {
        ensure!(
            !native_session_id.is_empty(),
            "native session identity required"
        );
        let slot = slot_path(self.native_state_dir(), &request.operation_id);
        let lock = try_claim(&slot)?.context("native edit still running")?;
        let entry = Entry::read(&slot)?.context("unknown native edit")?;
        if entry.request != *request {
            return Err(Rejection::ReusedId.into());
        }
        match &entry.phase {
            Phase::Dispatched {
                session_id: Some(s),
            } if s == native_session_id => {}
            _ => bail!("no interrupted turn with this native session identity"),
        }
        let directory = entry.workspace.reopen()?;
        Ok(NativeJob {
            slot,
            entry,
            _lock: lock,
            directory,
        })
    }
}
