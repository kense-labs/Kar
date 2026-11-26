//! Crash-safe framed Write-Ahead Log (`WalStore<T>`) paired with atomic JSON snapshots.
//!
//! Eliminates `O(N)` full-JSON rewrites and double-`fsync` overhead on hot workbench
//! mutations (`projects.json` + `projects.wal`, `workflows.json` + `workflows.wal`)
//! while preserving 100% on-disk `.json` compatibility.

use crate::{Error, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAGIC: &[u8; 4] = b"KWAL";
const VERSION: u8 = 1;
const KIND_SNAPSHOT: u8 = 1;
const KIND_PATCH: u8 = 2;
const KIND_MERGE_PATCH: u8 = 3;
const HEADER_LEN: usize = 14;
const MAX_FRAME_PAYLOAD: usize = 64 * 1024 * 1024;

pub const DEFAULT_MAX_WAL_FRAMES: usize = 128;
pub const DEFAULT_MAX_WAL_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalStats {
    pub frames: usize,
    pub wal_bytes: u64,
    pub json_exists: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct MapDelta {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    upsert: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remove: Vec<String>,
}

/// Lossless 2-level object delta that preserves nested `null` JSON values verbatim.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WalPatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    set: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    del: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    maps: BTreeMap<String, MapDelta>,
}

struct WalInner {
    json_path: PathBuf,
    wal_path: PathBuf,
    wal_file: Option<File>,
    frames: usize,
    wal_bytes: u64,
    json_exists: bool,
    max_frames: usize,
    max_bytes: u64,
    last_value: Option<Value>,
}

pub struct WalStore<T> {
    inner: Mutex<WalInner>,
    _marker: PhantomData<fn() -> T>,
}

pub fn wal_path_for(json_path: &Path) -> PathBuf {
    json_path.with_extension("wal")
}

fn open_wal_rw(path: &Path, create: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(create).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        opts.custom_flags(nix::libc::O_NOFOLLOW);
    }
    opts.open(path)
}

fn encode_frame(kind: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .map_err(|_| Error::Refused("WAL frame exceeds u32 capacity".into()))?;
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(Error::Refused(
            "WAL frame exceeds maximum payload size".into(),
        ));
    }
    let crc = crc32fast::hash(payload);
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.push(kind);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

fn compute_patch(prev: &Map<String, Value>, next: &Map<String, Value>) -> WalPatch {
    let mut patch = WalPatch::default();
    for key in prev.keys() {
        if !next.contains_key(key) {
            patch.del.push(key.clone());
        }
    }
    for (key, next_val) in next {
        match prev.get(key) {
            Some(prev_val) if prev_val == next_val => continue,
            Some(Value::Object(prev_sub)) => {
                if let Value::Object(next_sub) = next_val
                    && !prev_sub.is_empty()
                    && !next_sub.is_empty()
                {
                    let mut delta = MapDelta::default();
                    for sub_key in prev_sub.keys() {
                        if !next_sub.contains_key(sub_key) {
                            delta.remove.push(sub_key.clone());
                        }
                    }
                    for (sub_key, sub_val) in next_sub {
                        if prev_sub.get(sub_key) != Some(sub_val) {
                            delta.upsert.insert(sub_key.clone(), sub_val.clone());
                        }
                    }
                    if delta.upsert.len().saturating_add(delta.remove.len())
                        <= (next_sub.len() / 2).saturating_add(1)
                    {
                        patch.maps.insert(key.clone(), delta);
                        continue;
                    }
                }
                patch.set.insert(key.clone(), next_val.clone());
            }
            _ => {
                patch.set.insert(key.clone(), next_val.clone());
            }
        }
    }
    patch
}

fn apply_wal_patch(target: &mut Value, patch: WalPatch) -> Result<()> {
    let Some(map) = target.as_object_mut() else {
        return Err(Error::Protocol(
            "cannot apply WAL object patch to non-object state".into(),
        ));
    };
    for key in patch.del {
        map.remove(&key);
    }
    for (key, val) in patch.set {
        map.insert(key, val);
    }
    for (key, delta) in patch.maps {
        let entry = map.entry(key).or_insert_with(|| Value::Object(Map::new()));
        let Some(sub) = entry.as_object_mut() else {
            return Err(Error::Protocol(
                "cannot apply WAL map delta to non-object field".into(),
            ));
        };
        for rm in delta.remove {
            sub.remove(&rm);
        }
        for (sub_key, sub_val) in delta.upsert {
            sub.insert(sub_key, sub_val);
        }
    }
    Ok(())
}

fn apply_merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(patch_map) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let target_map = target.as_object_mut().expect("object");
            for (k, v) in patch_map {
                if v.is_null() {
                    target_map.remove(k);
                } else {
                    apply_merge_patch(target_map.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        other => *target = other.clone(),
    }
}

fn replay_wal_bytes(base: &mut Value, bytes: &[u8]) -> (usize, usize) {
    let mut offset = 0usize;
    let mut frames = 0usize;
    while offset + HEADER_LEN <= bytes.len() {
        if &bytes[offset..offset + 4] != MAGIC {
            break;
        }
        let version = bytes[offset + 4];
        let kind = bytes[offset + 5];
        let len = u32::from_le_bytes(
            bytes[offset + 6..offset + 10]
                .try_into()
                .expect("4-byte len"),
        ) as usize;
        let expected_crc = u32::from_le_bytes(
            bytes[offset + 10..offset + 14]
                .try_into()
                .expect("4-byte crc"),
        );
        if version != VERSION
            || !matches!(kind, KIND_SNAPSHOT | KIND_PATCH | KIND_MERGE_PATCH)
            || len > MAX_FRAME_PAYLOAD
            || offset + HEADER_LEN + len > bytes.len()
        {
            break;
        }
        let payload = &bytes[offset + HEADER_LEN..offset + HEADER_LEN + len];
        if crc32fast::hash(payload) != expected_crc {
            break;
        }
        let applied = match kind {
            KIND_SNAPSHOT => serde_json::from_slice::<Value>(payload).map(|v| {
                *base = v;
            }),
            KIND_PATCH => serde_json::from_slice::<WalPatch>(payload).and_then(|patch| {
                let mut candidate = base.clone();
                if apply_wal_patch(&mut candidate, patch).is_err() {
                    return Err(serde::de::Error::custom("invalid WAL patch target"));
                }
                *base = candidate;
                Ok(())
            }),
            KIND_MERGE_PATCH => serde_json::from_slice::<Value>(payload).map(|patch| {
                apply_merge_patch(base, &patch);
            }),
            _ => unreachable!(),
        };
        if applied.is_err() {
            break;
        }
        offset += HEADER_LEN + len;
        frames += 1;
    }
    (offset, frames)
}

fn write_atomic_json(json_path: &Path, value: &Value) -> Result<()> {
    let parent = json_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut tmp, value)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(json_path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

impl WalInner {
    fn ensure_wal_handle(&mut self) -> Result<&mut File> {
        if self.wal_file.is_none() {
            if let Some(parent) = self.wal_path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let mut file = open_wal_rw(&self.wal_path, true)?;
            let end = file.seek(SeekFrom::End(0))?;
            self.wal_bytes = end;
            self.wal_file = Some(file);
            #[cfg(unix)]
            if let Some(parent) = self.wal_path.parent().filter(|p| !p.as_os_str().is_empty()) {
                File::open(parent)?.sync_all()?;
            }
        }
        Ok(self.wal_file.as_mut().expect("open WAL handle"))
    }

    fn truncate_wal(&mut self) -> Result<u64> {
        let reclaimed = self.wal_bytes;
        if let Some(file) = self.wal_file.as_mut() {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.sync_data()?;
        } else {
            match open_wal_rw(&self.wal_path, false) {
                Ok(mut file) => {
                    let prev = file.metadata()?.len();
                    file.set_len(0)?;
                    file.seek(SeekFrom::Start(0))?;
                    file.sync_data()?;
                    self.wal_file = Some(file);
                    self.frames = 0;
                    self.wal_bytes = 0;
                    return Ok(prev);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.frames = 0;
        self.wal_bytes = 0;
        Ok(reclaimed)
    }

    fn checkpoint_value(&mut self, next_val: &Value) -> Result<u64> {
        write_atomic_json(&self.json_path, next_val)?;
        self.json_exists = true;
        let reclaimed = self.truncate_wal()?;
        self.last_value = Some(next_val.clone());
        Ok(reclaimed)
    }

    fn append_raw_frame(&mut self, kind: u8, payload: &[u8], next_val: Value) -> Result<()> {
        let frame = encode_frame(kind, payload)?;
        let frame_len = frame.len() as u64;
        if !self.json_exists
            || self.frames.saturating_add(1) >= self.max_frames
            || self.wal_bytes.saturating_add(frame_len) >= self.max_bytes
        {
            self.checkpoint_value(&next_val)?;
            return Ok(());
        }
        let file = self.ensure_wal_handle()?;
        file.write_all(&frame)?;
        file.sync_data()?;
        self.frames += 1;
        self.wal_bytes = self.wal_bytes.saturating_add(frame_len);
        self.last_value = Some(next_val);
        Ok(())
    }
}

impl<T> WalStore<T>
where
    T: Serialize + DeserializeOwned + Default,
{
    pub fn open(json_path: &Path) -> Result<(T, Self)> {
        Self::with_limits(json_path, DEFAULT_MAX_WAL_FRAMES, DEFAULT_MAX_WAL_BYTES)
    }

    pub fn with_limits(json_path: &Path, max_frames: usize, max_bytes: u64) -> Result<(T, Self)> {
        let max_frames = max_frames.max(1);
        let max_bytes = max_bytes.max(1024);
        let wal_path = wal_path_for(json_path);
        let (mut current_val, mut json_exists) = match File::open(json_path) {
            Ok(file) => (serde_json::from_reader::<_, Value>(file)?, true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                (serde_json::to_value(T::default())?, false)
            }
            Err(e) => return Err(e.into()),
        };

        let mut wal_file = None;
        let mut frames = 0usize;
        let mut wal_bytes = 0u64;

        match open_wal_rw(&wal_path, false) {
            Ok(mut file) => {
                let mut raw = Vec::new();
                file.read_to_end(&mut raw)?;
                let (valid_offset, replayed) = replay_wal_bytes(&mut current_val, &raw);
                if valid_offset < raw.len() {
                    file.set_len(valid_offset as u64)?;
                    file.seek(SeekFrom::End(0))?;
                    file.sync_data()?;
                } else {
                    file.seek(SeekFrom::End(0))?;
                }
                frames = replayed;
                wal_bytes = valid_offset as u64;
                wal_file = Some(file);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let state: T = serde_json::from_value(current_val.clone())?;
        if !json_exists && frames > 0 {
            write_atomic_json(json_path, &current_val)?;
            json_exists = true;
            if let Some(file) = wal_file.as_mut() {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                file.sync_data()?;
            }
            frames = 0;
            wal_bytes = 0;
        }

        let store = Self {
            inner: Mutex::new(WalInner {
                json_path: json_path.to_path_buf(),
                wal_path,
                wal_file,
                frames,
                wal_bytes,
                json_exists,
                max_frames,
                max_bytes,
                last_value: json_exists.then_some(current_val),
            }),
            _marker: PhantomData,
        };
        Ok((state, store))
    }

    /// Persist `state` via a compact CRC32-framed WAL delta (or full frame / automatic checkpoint).
    pub fn record(&self, state: &T) -> Result<()> {
        let next_val = serde_json::to_value(state)?;
        let mut inner = self.inner.lock().unwrap();
        if !inner.json_exists {
            inner.checkpoint_value(&next_val)?;
            return Ok(());
        }
        let (kind, payload) = match (&inner.last_value, &next_val) {
            (Some(Value::Object(prev_map)), Value::Object(next_map)) => {
                let patch = compute_patch(prev_map, next_map);
                let patch_bytes = serde_json::to_vec(&patch)?;
                let full_bytes = serde_json::to_vec(&next_val)?;
                if patch_bytes.len() < full_bytes.len() {
                    (KIND_PATCH, patch_bytes)
                } else {
                    (KIND_SNAPSHOT, full_bytes)
                }
            }
            _ => (KIND_SNAPSHOT, serde_json::to_vec(&next_val)?),
        };
        inner.append_raw_frame(kind, &payload, next_val)
    }

    /// Persist an explicit full-state WAL frame (`KIND_SNAPSHOT`) or checkpoint if at threshold.
    pub fn record_snapshot(&self, state: &T) -> Result<()> {
        let next_val = serde_json::to_value(state)?;
        let payload = serde_json::to_vec(&next_val)?;
        let mut inner = self.inner.lock().unwrap();
        inner.append_raw_frame(KIND_SNAPSHOT, &payload, next_val)
    }

    /// Persist an explicit RFC 7396 JSON merge patch (`KIND_MERGE_PATCH`) along with updated `state`.
    pub fn record_merge_patch(&self, state: &T, merge_patch: &Value) -> Result<()> {
        let next_val = serde_json::to_value(state)?;
        let payload = serde_json::to_vec(merge_patch)?;
        let mut inner = self.inner.lock().unwrap();
        inner.append_raw_frame(KIND_MERGE_PATCH, &payload, next_val)
    }

    /// Atomically checkpoint `state` to `<name>.json` and truncate `<name>.wal` to 0 bytes.
    pub fn checkpoint(&self, state: &T) -> Result<()> {
        let next_val = serde_json::to_value(state)?;
        let mut inner = self.inner.lock().unwrap();
        inner.checkpoint_value(&next_val)?;
        Ok(())
    }

    /// Explicit compaction for `/storage` GC: checkpoints `state` and returns reclaimed WAL bytes.
    pub fn compact(&self, state: &T) -> Result<u64> {
        let next_val = serde_json::to_value(state)?;
        let mut inner = self.inner.lock().unwrap();
        if !inner.json_exists && inner.wal_bytes == 0 {
            return Ok(0);
        }
        inner.checkpoint_value(&next_val)
    }

    pub fn stats(&self) -> WalStats {
        let inner = self.inner.lock().unwrap();
        WalStats {
            frames: inner.frames,
            wal_bytes: inner.wal_bytes,
            json_exists: inner.json_exists,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct SampleState {
        revision: u64,
        items: BTreeMap<String, Value>,
        note: Option<String>,
    }

    #[test]
    fn wal_replays_patches_and_truncates_torn_tail_on_recovery() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let json_path = dir.path().join("sample.json");
        let wal_path = wal_path_for(&json_path);

        let (mut state, store) = WalStore::<SampleState>::with_limits(&json_path, 16, 1024 * 1024)?;
        assert_eq!(state, SampleState::default());
        assert!(!store.stats().json_exists);

        // First write creates base snapshot on disk for external .json readers.
        state.revision = 1;
        for i in 0..20 {
            state.items.insert(
                format!("k{i}"),
                serde_json::json!({"v": i, "nullable": null}),
            );
        }
        store.record(&state)?;
        assert!(json_path.exists());
        assert_eq!(store.stats().frames, 0);

        // Subsequent mutations append compact delta frames to sample.wal.
        state.revision = 2;
        state
            .items
            .insert("k3".into(), serde_json::json!({"v": 300, "nullable": null}));
        state.items.remove("k0");
        state.note = Some("updated".into());
        store.record(&state)?;

        state.revision = 3;
        state.note = None;
        store.record(&state)?;
        assert_eq!(store.stats().frames, 2);
        let valid_wal_len = fs::metadata(&wal_path)?.len();
        assert!(valid_wal_len > HEADER_LEN as u64);

        drop(store);

        // Simulate a crash mid-append by writing a torn trailing frame header + partial payload.
        {
            let mut f = OpenOptions::new().append(true).open(&wal_path)?;
            f.write_all(b"KWAL\x01\x02\x40\x00\x00\x00\xde\xad\xbe\xef{\"partial\":")?;
            f.sync_all()?;
        }
        assert!(fs::metadata(&wal_path)?.len() > valid_wal_len);

        // Reopen: valid frames replay cleanly and torn tail is truncated to exact valid_wal_len.
        let (recovered, reopened) = WalStore::<SampleState>::open(&json_path)?;
        assert_eq!(recovered, state);
        assert_eq!(fs::metadata(&wal_path)?.len(), valid_wal_len);
        assert_eq!(reopened.stats().frames, 2);

        // Explicit compact() checkpoints to sample.json and reclaims WAL bytes.
        let reclaimed = reopened.compact(&recovered)?;
        assert_eq!(reclaimed, valid_wal_len);
        assert_eq!(fs::metadata(&wal_path)?.len(), 0);
        assert_eq!(reopened.stats().frames, 0);

        let disk_state: SampleState = serde_json::from_slice(&fs::read(&json_path)?)?;
        assert_eq!(disk_state, state);
        Ok(())
    }

    #[test]
    fn wal_auto_checkpoints_at_frame_threshold_and_preserves_corrupt_json_errors() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let json_path = dir.path().join("workflows.json");
        let (mut state, store) = WalStore::<SampleState>::with_limits(&json_path, 4, 1024 * 1024)?;

        for rev in 1..=6 {
            state.revision = rev;
            state
                .items
                .insert(format!("r{rev}"), serde_json::json!(rev));
            store.record(&state)?;
        }
        // rev=1 initial checkpoint -> frames=0; rev=2,3,4 -> frames=1,2,3; rev=5 hits max_frames=4 -> checkpoint (frames=0); rev=6 -> frames=1.
        assert_eq!(store.stats().frames, 1);
        drop(store);

        let (reloaded, _) = WalStore::<SampleState>::open(&json_path)?;
        assert_eq!(reloaded.revision, 6);
        assert_eq!(reloaded.items.len(), 6);

        // Corrupt base JSON must still surface an error rather than silently resetting state.
        let broken_path = dir.path().join("broken.json");
        fs::write(&broken_path, b"{not valid json")?;
        assert!(WalStore::<SampleState>::open(&broken_path).is_err());
        Ok(())
    }

    #[test]
    fn wal_detects_corrupted_crc_frame_and_recovers_prefix() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let json_path = dir.path().join("crc.json");
        let wal_path = wal_path_for(&json_path);
        let (mut state, store) = WalStore::<SampleState>::open(&json_path)?;

        state.revision = 1;
        store.record(&state)?;
        state.revision = 2;
        state.note = Some("good-frame".into());
        store.record(&state)?;
        let good_len = fs::metadata(&wal_path)?.len();

        state.revision = 3;
        state.note = Some("corrupted-frame".into());
        store.record(&state)?;
        drop(store);

        // Flip one bit in the second WAL frame's payload so CRC32 fails.
        let mut raw = fs::read(&wal_path)?;
        let last_idx = raw.len() - 2;
        raw[last_idx] ^= 0x01;
        fs::write(&wal_path, &raw)?;

        let (recovered, _) = WalStore::<SampleState>::open(&json_path)?;
        assert_eq!(recovered.revision, 2);
        assert_eq!(recovered.note.as_deref(), Some("good-frame"));
        assert_eq!(fs::metadata(&wal_path)?.len(), good_len);
        Ok(())
    }
}
