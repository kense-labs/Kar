//! Append a single complete change batch before advancing the resumable shell cursor.
use super::*;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Header {
    format: String,
    stream: Uuid,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Batch {
    pub sequence: u64,
    pub changes: Vec<Change>,
}

pub(super) struct Journal {
    pub stream: Uuid,
    pub rows: BTreeMap<Key, Value>,
    path: PathBuf,
    file: File,
    offsets: Vec<u64>,
    end: u64,
}
impl Journal {
    pub fn open(state: &Path, scope: &str) -> Result<Self> {
        let path = state.join(format!("shell-{scope}.jsonl"));
        if !path.exists() {
            let mut fresh = tempfile::NamedTempFile::new_in(state)?;
            serde_json::to_writer(
                &mut fresh,
                &Header {
                    format: "kar-shell/1".into(),
                    stream: Uuid::new_v4(),
                },
            )?;
            fresh.write_all(b"\n")?;
            fresh.as_file().sync_all()?;
            fresh.persist_noclobber(&path).map_err(|e| e.error)?;
            #[cfg(unix)]
            File::open(state)?.sync_all()?;
        }
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let mut reader = BufReader::new(File::open(&path)?);
        let mut bytes = Vec::new();
        reader.read_until(b'\n', &mut bytes)?;
        let header: Header = serde_json::from_slice(&bytes)?;
        if bytes.last() != Some(&b'\n') || header.format != "kar-shell/1" {
            return Err(Error::Protocol("invalid Kar shell journal".into()));
        }
        let mut journal = Self {
            stream: header.stream,
            rows: BTreeMap::new(),
            path,
            file,
            offsets: vec![],
            end: bytes.len() as u64,
        };
        loop {
            bytes.clear();
            if reader.read_until(b'\n', &mut bytes)? == 0 {
                break;
            }
            if bytes.last() != Some(&b'\n') {
                journal.file.set_len(journal.end)?;
                journal.file.sync_data()?;
                break;
            }
            let batch: Batch = serde_json::from_slice(&bytes)?;
            if batch.sequence != journal.sequence() + 1 {
                return Err(Error::Protocol("non-contiguous shell sequence".into()));
            }
            for c in batch.changes {
                match c.row {
                    Some(value) => {
                        journal.rows.insert(c.key, value);
                    }
                    None => {
                        journal.rows.remove(&c.key);
                    }
                }
            }
            journal.offsets.push(journal.end);
            journal.end += bytes.len() as u64;
        }
        Ok(journal)
    }
    pub fn sequence(&self) -> u64 {
        self.offsets.len() as u64
    }
    pub fn commit(&mut self, rows: BTreeMap<Key, Value>) -> Result<bool> {
        let mut changes = Vec::new();
        for key in self.rows.keys().filter(|k| !rows.contains_key(k)) {
            changes.push(Change {
                key: *key,
                row: None,
            });
        }
        for (key, value) in &rows {
            if self.rows.get(key) != Some(value) {
                changes.push(Change {
                    key: *key,
                    row: Some(value.clone()),
                });
            }
        }
        if changes.is_empty() {
            return Ok(false);
        }
        let batch = Batch {
            sequence: self.sequence() + 1,
            changes,
        };
        let mut record = serde_json::to_vec(&batch)?;
        record.push(b'\n');
        // A failed append may have left a prefix, including after a full write whose sync failed.
        self.file.set_len(self.end)?;
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&record)?;
        self.file.sync_data()?;
        self.offsets.push(self.end);
        self.end += record.len() as u64;
        self.rows = rows;
        Ok(true)
    }
    pub fn page(&self, after: u64) -> Result<VecDeque<Batch>> {
        if after > self.sequence() {
            return Err(Error::InvalidCursor);
        }
        let Some(offset) = self.offsets.get(after as usize) else {
            return Ok(VecDeque::new());
        };
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(*offset))?;
        let mut reader = BufReader::new(file);
        let mut batches = VecDeque::new();
        let mut buf = Vec::new();
        let mut read = 0;
        while batches.len() < 32
            && after + (batches.len() as u64) < self.sequence()
            && read < 256 * 1024
        {
            buf.clear();
            read += reader.read_until(b'\n', &mut buf)?;
            batches.push_back(serde_json::from_slice(&buf)?);
        }
        Ok(batches)
    }
}
