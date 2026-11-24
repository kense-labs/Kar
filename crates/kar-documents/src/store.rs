//! A single writer publishes Office revisions for one registered working file.
use crate::{Conflict, OfficeEdits, candidate::prepare_staged, merge::Comparison};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Command {
    pub author: String,
    pub operation_id: String,
    pub base_revision: u64,
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Edit { edits: OfficeEdits },
    Save { sha256: String },
    Grid { change: crate::GridChange },
    Resolve { conflict: u64, decision: Decision },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    KeepCurrent,
    AdoptProposed,
    Manual { edits: OfficeEdits },
    ManualFile { sha256: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Applied,
    Unchanged,
    Conflict,
    External,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub document: String,
    pub cursor: u64,
    pub author: String,
    pub operation_id: Option<String>,
    pub base_revision: u64,
    pub revision: u64,
    pub status: Status,
    pub conflict: Option<Conflict>,
    /// For conflicts this is the original pending dispute. Another failed
    /// resolution has its own event cursor but keeps the same conflict ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_id: Option<u64>,
    /// Cursor of the conflict successfully settled by this command.
    pub resolves: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub command: Option<Command>,
    pub receipt: Receipt,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DocumentState {
    pub document: String,
    pub revision: u64,
    pub cursor: u64,
    pub sha256: String,
    pub pending_conflicts: Vec<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum Rejection {
    #[error("operation ID already belongs to a different request")]
    ReusedId,
    #[error("resolution was based on revision {provided}; review current revision {current}")]
    StaleResolution { provided: u64, current: u64 },
    #[error("conflict {0} is missing or already resolved")]
    ClosedConflict(u64),
}

#[derive(Clone, Serialize, Deserialize)]
struct Revision {
    #[serde(default)]
    grid: Option<crate::GridChange>,
    digest: String,
    external: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    #[serde(default)]
    grid: Option<crate::GridChange>,
    #[serde(default)]
    layout_unknown: bool,
    candidate: String,
    record: Record,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    document: String,
    file: PathBuf,
    revisions: Vec<Revision>,
    records: Vec<Record>,
    pending: Option<Pending>,
}

impl Manifest {
    fn head(&self) -> u64 {
        self.revisions.len() as u64 - 1
    }
    fn revision(&self, n: u64) -> Result<&Revision> {
        usize::try_from(n)
            .ok()
            .and_then(|i| self.revisions.get(i))
            .context("unknown base revision")
    }
    fn next_cursor(&self) -> u64 {
        self.records.len() as u64 + 1
    }
    fn conflict(&self, cursor: u64) -> Result<&Record> {
        let record = cursor
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.records.get(i));
        match record {
            Some(record)
                if record.receipt.status == Status::Conflict
                    && record.receipt.conflict_id.unwrap_or(cursor) == cursor
                    && !self
                        .records
                        .iter()
                        .any(|r| r.receipt.resolves == Some(cursor)) =>
            {
                Ok(record)
            }
            _ => Err(Rejection::ClosedConflict(cursor).into()),
        }
    }
}

/// I/O for the registered working file. A workspace implementation can
/// keep all lookups, temporary files and renames relative to its directory
/// capability. Publication must atomically replace the entire file.
pub trait WorkingFile: Send + Sync {
    fn read(&self) -> Result<Vec<u8>>;
    fn publish(&self, bytes: &[u8]) -> Result<()>;
}

struct LocalFile(PathBuf);
impl WorkingFile for LocalFile {
    fn read(&self) -> Result<Vec<u8>> {
        Ok(fs::read(&self.0)?)
    }
    fn publish(&self, bytes: &[u8]) -> Result<()> {
        atomic_write(&self.0, bytes)
    }
}

pub struct DocumentStore {
    working: Box<dyn WorkingFile>,
    dir: PathBuf,
    file: PathBuf,
    // Held for the lifetime of the writable store. The release is automatic on process exit.
    _lock: File,
}

impl DocumentStore {
    pub fn open(state: &Path, file: &Path) -> Result<Self> {
        let file = file.canonicalize()?;
        ensure!(file.is_file(), "expected a regular working file");
        Self::open_with_file(state, &file, Box::new(LocalFile(file.clone())))
    }

    /// Register an already validated canonical identity. Working content
    /// is read and published exclusively through the supplied I/O object.
    pub fn open_with_file(
        state: &Path,
        identity: &Path,
        working: Box<dyn WorkingFile>,
    ) -> Result<Self> {
        let file = identity.to_path_buf();
        fs::create_dir_all(state)?;
        let dir = state.canonicalize()?;
        ensure!(
            !file.starts_with(&dir),
            "working file is inside the document store"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("writer.lock"))?;
        lock.try_lock()
            .context("another writer owns this document store")?;
        let store = Self {
            working,
            dir,
            file,
            _lock: lock,
        };
        let path = store.dir.join("manifest.json");
        fs::create_dir_all(store.dir.join("blobs"))?;
        if !path.try_exists()? {
            let bytes = store.working.read()?;
            let empty = store.empty_edits()?;
            crate::prepare(&bytes, &bytes, &empty)?;
            let digest = store.put_blob(&bytes)?;
            let document = hash(store.file.as_os_str().as_encoded_bytes());
            store.persist(&Manifest {
                format: "kar-documents/1".into(),
                document,
                file: store.file.clone(),
                revisions: vec![Revision {
                    grid: None,
                    digest,
                    external: false,
                }],
                records: Vec::new(),
                pending: None,
            })?;
        }
        store.load()?;
        Ok(store)
    }

    fn empty_edits(&self) -> Result<OfficeEdits> {
        match self
            .file
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("txt" | "md" | "markdown") => Ok(OfficeEdits::Text(vec![])),
            Some("docx") => Ok(OfficeEdits::Word(vec![])),
            Some("xlsx") => Ok(OfficeEdits::Excel(vec![])),
            Some("pptx") => Ok(OfficeEdits::Slides(vec![])),
            _ => bail!("expected a txt, md, markdown, docx, xlsx or pptx working file"),
        }
    }

    fn persist(&self, manifest: &Manifest) -> Result<()> {
        atomic_write(
            &self.dir.join("manifest.json"),
            &serde_json::to_vec(manifest)?,
        )
    }

    /// Stage a native editor's complete proposed file without touching the working file.
    pub fn stage(&mut self, bytes: &[u8]) -> Result<String> {
        self.put_blob(bytes)
    }
    pub fn proposal(&mut self, sha256: &str) -> Result<Vec<u8>> {
        self.blob(sha256)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let digest = hash(bytes);
        let path = self.dir.join("blobs").join(&digest);
        if !path.try_exists()? {
            atomic_write(&path, bytes)?;
        }
        Ok(digest)
    }

    fn blob(&self, digest: &str) -> Result<Vec<u8>> {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid snapshot digest"
        );
        let bytes = fs::read(self.dir.join("blobs").join(digest))?;
        ensure!(hash(&bytes) == digest, "damaged document snapshot");
        Ok(bytes)
    }

    // Reload after every operation. An I/O error after durable intent leaves a
    // recoverable state, including when the service keeps running.
    fn load(&self) -> Result<Manifest> {
        let mut m: Manifest = serde_json::from_slice(&fs::read(self.dir.join("manifest.json"))?)?;
        ensure!(
            m.format == "kar-documents/1" && m.file == self.file && !m.revisions.is_empty(),
            "unexpected document manifest"
        );
        let live = self.working.read()?;
        let digest = hash(&live);
        let previous = m.revision(m.head())?.digest.clone();
        if let Some(mut pending) = m.pending.take() {
            if digest == previous || digest == pending.candidate {
                if digest != pending.candidate {
                    let candidate = self.blob(&pending.candidate)?;
                    self.working.publish(&candidate)?;
                }
                m.revisions.push(Revision {
                    grid: pending.grid,
                    digest: pending.candidate,
                    external: pending.layout_unknown,
                });
            } else {
                self.record_external(&mut m, &live)?;
                let r = &mut pending.record.receipt;
                r.status = Status::Conflict;
                let original_conflict = r.resolves;
                r.revision = m.head();
                r.cursor = m.next_cursor();
                r.conflict_id = Some(original_conflict.unwrap_or(r.cursor));
                r.resolves = None;
                r.conflict = Some(Conflict {
                    target: m
                        .file
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    property: "file".into(),
                    reason: "working file changed during publication".into(),
                    base: json!(previous),
                    current: Some(json!(digest)),
                    proposed: json!(pending.candidate),
                });
            }
            m.records.push(pending.record);
            self.persist(&m)?;
        } else if digest != previous {
            self.record_external(&mut m, &live)?;
            self.persist(&m)?;
        }
        Ok(m)
    }

    fn record_external(&self, m: &mut Manifest, bytes: &[u8]) -> Result<()> {
        let parent = m.head();
        let previous = self.blob(&m.revision(parent)?.digest)?;
        let layout_unknown =
            crate::file_proposal::extract(&self.empty_edits()?, &previous, bytes).is_err();
        let digest = self.put_blob(bytes)?;
        m.revisions.push(Revision {
            grid: None,
            digest,
            external: layout_unknown,
        });
        m.records.push(Record {
            command: None,
            receipt: Receipt {
                document: m.document.clone(),
                cursor: m.next_cursor(),
                author: "external editor".into(),
                operation_id: None,
                base_revision: parent,
                revision: m.head(),
                status: Status::External,
                conflict: None,
                conflict_id: None,
                resolves: None,
            },
        });
        Ok(())
    }

    pub fn state(&mut self) -> Result<DocumentState> {
        let m = self.load()?;
        Ok(DocumentState {
            document: m.document.clone(),
            revision: m.head(),
            cursor: m.records.len() as u64,
            sha256: m.revision(m.head())?.digest.clone(),
            pending_conflicts: m
                .records
                .iter()
                .filter(|r| {
                    r.receipt.status == Status::Conflict && m.conflict(r.receipt.cursor).is_ok()
                })
                .map(|r| r.receipt.cursor)
                .collect(),
        })
    }

    pub fn snapshot(&mut self, revision: u64) -> Result<Vec<u8>> {
        let m = self.load()?;
        self.blob(&m.revision(revision)?.digest)
    }

    pub fn events(&mut self, after: u64) -> Result<Vec<Record>> {
        let m = self.load()?;
        ensure!(
            after <= m.records.len() as u64,
            "event cursor is ahead of document"
        );
        Ok(m.records.into_iter().skip(after as usize).collect())
    }

    fn process_edits(
        &self,
        m: &Manifest,
        base: u64,
        edits: &OfficeEdits,
        comparison: Comparison,
    ) -> Result<Option<Vec<u8>>> {
        ensure!(
            edits.extension() == self.empty_edits()?.extension(),
            "wrong edit type for document"
        );
        let before = self.blob(&m.revision(base)?.digest)?;
        let current = self.blob(&m.revision(m.head())?.digest)?;
        let external = m.revisions[base as usize + 1..].iter().any(|r| r.external);
        if matches!(edits, OfficeEdits::Excel(_)) && external {
            return crate::prepare(&before, &current, edits);
        }
        let history: Vec<_> = m.revisions[base as usize + 1..]
            .iter()
            .filter_map(|r| r.grid)
            .collect();
        let result = prepare_staged(&before, &current, edits, &history, comparison, !external);
        if result.as_ref().is_err_and(|e| !e.is::<Conflict>())
            && crate::prepare(&before, &before, edits).is_ok()
        {
            return Err(Conflict {
                target: self
                    .file
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                property: "file".into(),
                reason: format!(
                    "cannot apply proposal losslessly on the latest revision: {}",
                    result.unwrap_err()
                ),
                base: json!(m.revision(base)?.digest),
                current: Some(json!(m.revision(m.head())?.digest)),
                proposed: serde_json::to_value(edits)?,
            }
            .into());
        }
        result
    }

    fn process_file(
        &self,
        m: &Manifest,
        base: u64,
        digest: &str,
        mode: Comparison,
    ) -> Result<Option<Vec<u8>>> {
        let base_digest = &m.revision(base)?.digest;
        let current_digest = &m.revision(m.head())?.digest;
        let before = self.blob(base_digest)?;
        let proposed = self.blob(digest)?;
        let format = self.empty_edits()?;
        let file_conflict = |reason: String| {
            anyhow::Error::new(Conflict {
                target: self
                    .file
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                property: "file".into(),
                reason,
                base: json!(base_digest),
                current: Some(json!(current_digest)),
                proposed: json!(digest),
            })
        };
        if digest == current_digest || digest == base_digest {
            return Ok(None);
        }
        if base_digest == current_digest {
            crate::file_proposal::validate(&format, &proposed)
                .map_err(|e| file_conflict(format!("invalid proposed file: {e}")))?;
            return Ok(Some(proposed));
        }
        let edits = crate::file_proposal::extract(&format, &before, &proposed)
            .map_err(|e| file_conflict(format!("cannot reproduce entire native proposal: {e}")))?;
        self.process_edits(m, base, &edits, mode).map_err(|e| {
            if e.is::<Conflict>() {
                e
            } else {
                file_conflict(format!("cannot merge native proposal: {e}"))
            }
        })
    }

    fn candidate(&self, m: &Manifest, command: &Command) -> Result<Option<Vec<u8>>> {
        match &command.action {
            Action::Grid { change } => {
                ensure!(
                    matches!(self.empty_edits()?, OfficeEdits::Excel(_)),
                    "grid command requires a workbook"
                );
                ensure!(
                    command.base_revision == m.head(),
                    "worksheet structure changed; review revision {}",
                    m.head()
                );
                let current = self.blob(&m.revision(m.head())?.digest)?;
                Ok(Some(crate::grid_history::apply_grid(&current, *change)?))
            }
            Action::Save { sha256 } => {
                self.process_file(m, command.base_revision, sha256, Comparison::Current)
            }
            Action::Edit { edits } => {
                self.process_edits(m, command.base_revision, edits, Comparison::Current)
            }
            Action::Resolve { conflict, decision } => {
                let record = m.conflict(*conflict)?;
                if command.base_revision != m.head() {
                    return Err(Rejection::StaleResolution {
                        provided: command.base_revision,
                        current: m.head(),
                    }
                    .into());
                }
                match decision {
                    Decision::KeepCurrent => Ok(None),
                    Decision::ManualFile { sha256 } => {
                        self.process_file(m, command.base_revision, sha256, Comparison::Current)
                    }
                    Decision::Manual { edits } => {
                        self.process_edits(m, command.base_revision, edits, Comparison::Current)
                    }
                    Decision::AdoptProposed => {
                        let original = record
                            .command
                            .as_ref()
                            .context("conflict has no proposal")?;
                        match &original.action {
                            Action::Save { sha256 }
                            | Action::Resolve {
                                decision: Decision::ManualFile { sha256 },
                                ..
                            } => self.process_file(
                                m,
                                original.base_revision,
                                sha256,
                                Comparison::Adopt,
                            ),
                            Action::Edit { edits } => self.process_edits(
                                m,
                                original.base_revision,
                                edits,
                                Comparison::Adopt,
                            ),
                            Action::Resolve {
                                decision: Decision::Manual { edits },
                                ..
                            } => self.process_edits(
                                m,
                                original.base_revision,
                                edits,
                                Comparison::Adopt,
                            ),
                            _ => bail!(
                                "this resolution requires a manual selection in the latest revision"
                            ),
                        }
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn native_state_dir(&self) -> &Path {
        &self.dir
    }
    #[cfg(unix)]
    pub(crate) fn native_file_extension(&self) -> Result<String> {
        self.empty_edits()?;
        Ok(self
            .file
            .extension()
            .and_then(|v| v.to_str())
            .context("missing document extension")?
            .to_ascii_lowercase())
    }
    #[cfg(unix)]
    pub(crate) fn operation_exists(&self, id: &str) -> Result<bool> {
        Ok(self
            .load()?
            .records
            .iter()
            .any(|r| r.receipt.operation_id.as_deref() == Some(id)))
    }

    pub fn submit(&mut self, command: Command) -> Result<Receipt> {
        ensure!(
            !command.author.trim().is_empty() && !command.operation_id.trim().is_empty(),
            "author and operation ID required"
        );
        let mut m = self.load()?;
        if let Some(old) = m
            .records
            .iter()
            .find(|r| r.receipt.operation_id.as_deref() == Some(&command.operation_id))
        {
            if serde_json::to_value(&old.command)? != serde_json::to_value(&command)? {
                return Err(Rejection::ReusedId.into());
            }
            return Ok(old.receipt.clone());
        }
        #[cfg(unix)]
        crate::native::check_reservation(&self.dir, &command)?;
        m.revision(command.base_revision)?;
        let result = self.candidate(&m, &command);
        let (candidate, conflict) = match result {
            Ok(bytes) => (bytes, None),
            Err(e) => (None, Some(e.downcast::<Conflict>()?)),
        };
        let status = if conflict.is_some() {
            Status::Conflict
        } else if candidate.is_some() {
            Status::Applied
        } else {
            Status::Unchanged
        };
        let resolves = if conflict.is_none()
            && let Action::Resolve { conflict, .. } = &command.action
        {
            Some(*conflict)
        } else {
            None
        };
        let receipt = Receipt {
            document: m.document.clone(),
            cursor: m.next_cursor(),
            author: command.author.clone(),
            operation_id: Some(command.operation_id.clone()),
            base_revision: command.base_revision,
            revision: m.head() + u64::from(candidate.is_some()),
            conflict_id: conflict.as_ref().map(|_| {
                if let Action::Resolve { conflict, .. } = &command.action {
                    *conflict
                } else {
                    m.next_cursor()
                }
            }),
            status,
            conflict,
            resolves,
        };
        let grid = if let Action::Grid { change } = &command.action {
            Some(*change)
        } else {
            None
        };
        let record = Record {
            command: Some(command),
            receipt: receipt.clone(),
        };
        if let Some(bytes) = candidate {
            // Managed selection edits preserve paragraph/cell identities. A
            // whole-file publication may also have inserted or removed them.
            let whole_file = matches!(
                record.command.as_ref().map(|c| &c.action),
                Some(
                    Action::Save { .. }
                        | Action::Resolve {
                            decision: Decision::ManualFile { .. } | Decision::AdoptProposed,
                            ..
                        }
                )
            );
            let layout_unknown = whole_file
                && crate::file_proposal::extract(
                    &self.empty_edits()?,
                    &self.blob(&m.revision(m.head())?.digest)?,
                    &bytes,
                )
                .is_err();
            let pending = Pending {
                grid,
                layout_unknown,
                candidate: self.put_blob(&bytes)?,
                record,
            };
            m.pending = Some(pending);
            self.persist(&m)?;
            let completed = self.load()?;
            let result = completed
                .records
                .last()
                .ok_or_else(|| anyhow!("missing published document receipt"))?;
            return Ok(result.receipt.clone());
        }
        m.records.push(record);
        self.persist(&m)?;
        Ok(receipt)
    }
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(value, "{byte:02x}").expect("infallible string write");
    }
    value
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    if let Ok(meta) = fs::metadata(path) {
        tmp.as_file().set_permissions(meta.permissions())?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
