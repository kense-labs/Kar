//! Read the provider's live SQLite tables and committed WAL without a SQLite process.
use super::*;
use crate::workbench::native_sqlite::{
    Tables, invalid,
    store::{bounded, open, wal_path},
};
use sqlite_core::Value as SqlValue;
use std::{fs, io};
pub(super) mod antigravity;
mod opencode;
pub(super) use opencode::legacy;
pub(super) fn fingerprint(path: &Path) -> io::Result<String> {
    let single = |path: &Path| {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_file() {
            return Err(invalid("usage database or WAL is not a regular file"));
        }
        Ok(format!(
            "{}/{}",
            scans::identity(&meta),
            scans::signature(&meta)
        ))
    };
    let wal = match single(&wal_path(path)) {
        Ok(id) => id,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    Ok(format!("{}/{}", single(path)?, wal))
}
pub(super) fn read(path: &Path, format: &str) -> io::Result<Vec<Entry>> {
    let db = open(path)?;
    if format == "opencode" {
        opencode::rows(&db)
    } else {
        antigravity::rows(&db, path)
    }
}
