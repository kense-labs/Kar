//! Read a native SQLite database and the last committed WAL snapshot without mutating either.
use super::{Tables, invalid, sqlite_err};
use sqlite_core::Database;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};
pub(crate) fn wal_path(path: &Path) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push("-wal");
    path.into()
}
pub(crate) fn bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return Err(invalid("usage data exceeds the snapshot size limit"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("usage data grew beyond the snapshot size limit"));
    }
    Ok(bytes)
}
pub(crate) fn open(path: &Path) -> io::Result<Tables> {
    let wal = wal_path(path);
    let db = match fs::symlink_metadata(&wal) {
        Ok(m) if !m.is_file() => return Err(invalid("usage WAL is not a regular file")),
        Ok(m) if m.len() > 0 => {
            Database::open_with_wal(bounded(path, 512 << 20)?, &bounded(&wal, 128 << 20)?)
        }
        Ok(_) => Database::open_path(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Database::open_path(path),
        Err(e) => return Err(e),
    }
    .map_err(sqlite_err)?;
    Ok(Tables::from_database(db))
}
