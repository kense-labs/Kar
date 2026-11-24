//! Persist raw PTY bytes in bounded segments. Cursors are absolute byte offsets.
use crate::{Error, Result};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const SEGMENT: u64 = 128 * 1024;
const RETAIN: u64 = 8 * 1024 * 1024;
pub(super) const PAGE: usize = 512 * 1024;

pub(super) struct History {
    dir: PathBuf,
    chunks: VecDeque<(u64, u64)>,
    pub end: u64,
    clears: VecDeque<u64>,
}

impl History {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let clears: VecDeque<u64> = match fs::File::open(dir.join("clears.json")) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => VecDeque::new(),
            Err(e) => return Err(e.into()),
        };
        let cleared = clears.back().copied().unwrap_or(0);
        let mut chunks = Vec::new();
        for entry in fs::read_dir(dir)? {
            let file = entry?;
            let Some(start) = file
                .file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".bin")?.parse::<u64>().ok())
            else {
                continue;
            };
            if start < cleared {
                fs::remove_file(file.path())?;
            } else {
                chunks.push((start, file.metadata()?.len()));
            }
        }
        chunks.sort_unstable();
        if chunks
            .windows(2)
            .any(|pair| pair[0].0.checked_add(pair[0].1) != Some(pair[1].0))
        {
            return Err(Error::Protocol("non-contiguous terminal history".into()));
        }
        let end = chunks.last().map_or(cleared, |(start, len)| start + len);
        Ok(Self {
            dir: dir.into(),
            chunks: chunks.into(),
            end,
            clears,
        })
    }
    // A clear consumes one cursor position even if the PTY has not written any bytes.
    // Commit the barrier before unlinking files, so restart cannot resurrect cleared output.
    pub fn clear(&mut self) -> Result<u64> {
        let cursor = self.end.checked_add(1).ok_or(Error::InvalidCursor)?;
        let mut clears = self.clears.clone();
        clears.push_back(cursor);
        if clears.len() > 1024 {
            clears.pop_front();
        }
        super::write_json(&self.dir.join("clears.json"), &clears)?;
        self.clears = clears;
        self.end = cursor;
        for (start, _) in std::mem::take(&mut self.chunks) {
            fs::remove_file(self.path(start))?;
        }
        Ok(cursor)
    }
    pub fn cleared_since(&self, after: u64) -> impl Iterator<Item = u64> + '_ {
        self.clears.iter().copied().filter(move |c| *c > after)
    }
    fn path(&self, start: u64) -> PathBuf {
        self.dir.join(format!("{start:020}.bin"))
    }
    pub fn first(&self) -> u64 {
        self.chunks.front().map_or(self.end, |c| c.0)
    }
    pub fn append(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            if self.chunks.back().is_none_or(|c| c.1 >= SEGMENT) {
                self.chunks.push_back((self.end, 0));
            }
            let (start, len) = *self.chunks.back().unwrap();
            let n = bytes.len().min((SEGMENT - len) as usize);
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.path(start))?;
            file.write_all(&bytes[..n])?;
            file.sync_data()?;
            self.chunks.back_mut().unwrap().1 += n as u64;
            self.end += n as u64;
            bytes = &bytes[n..];
        }
        while let Some(&(start, len)) = self.chunks.front() {
            if start + len > self.end.saturating_sub(RETAIN) {
                break;
            }
            fs::remove_file(self.path(start))?;
            self.chunks.pop_front();
        }
        Ok(())
    }
    pub fn page(&self, from: u64) -> Result<(u64, Vec<u8>)> {
        if from > self.end {
            return Err(Error::InvalidCursor);
        }
        let start = from.max(self.first());
        let stop = self.end.min(start.saturating_add(PAGE as u64));
        let mut bytes = Vec::with_capacity((stop - start) as usize);
        for &(offset, len) in &self.chunks {
            let a = start.max(offset);
            let b = stop.min(offset + len);
            if a >= b {
                continue;
            }
            let mut file = File::open(self.path(offset))?;
            file.seek(SeekFrom::Start(a - offset))?;
            file.take(b - a).read_to_end(&mut bytes)?;
        }
        Ok((start, bytes))
    }
}
