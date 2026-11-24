//! Read the provider's live SQLite tables and committed WAL without a SQLite process.
use super::*;
use sqlite_core::{Database, Row, Value as SqlValue};
use std::{
    fs,
    io::{self, Read},
};
pub(super) mod antigravity;
mod opencode;
pub(super) use opencode::legacy;
fn invalid(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m)
}
fn sqlite_err(e: sqlite_core::Error) -> io::Error {
    invalid(&format!("reading SQLite usage: {e:?}"))
}
fn wal_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push("-wal");
    p.into()
}
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
fn bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
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
struct Tables {
    db: Database,
    tables: BTreeMap<String, (u32, Vec<String>)>,
}
impl Tables {
    fn open(path: &Path) -> io::Result<Self> {
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
        let tables = db
            .live_tables()
            .into_iter()
            .filter_map(|t| Some((t.name, (t.rootpage, t.column_names?))))
            .collect();
        Ok(Self { db, tables })
    }
    fn read(
        &self,
        name: &str,
        mut consume: impl FnMut(&Columns, Row) -> io::Result<()>,
    ) -> io::Result<()> {
        let Some((root, names)) = self.tables.get(name) else {
            return Ok(());
        };
        let cols = Columns(
            names
                .iter()
                .enumerate()
                .map(|(i, k)| (k.as_str(), i))
                .collect(),
        );
        let mut stack = vec![*root];
        let mut visited = BTreeSet::new();
        // Decode one leaf at a time. SQLite table interior cells start with a four-byte child page.
        while let Some(page) = stack.pop() {
            if !visited.insert(page) || visited.len() > self.db.page_count() as usize {
                return Err(invalid("invalid SQLite table traversal"));
            }
            let data = self
                .db
                .raw_page(page)
                .ok_or_else(|| invalid("missing SQLite page"))?;
            let start = if page == 1 { 100 } else { 0 };
            let data = data
                .get(start..)
                .ok_or_else(|| invalid("short SQLite page"))?;
            match data.first() {
                Some(13) => {
                    for row in self.db.read_table(page, names.len()).map_err(sqlite_err)? {
                        consume(&cols, row)?;
                    }
                }
                Some(5) => {
                    let get16 = |at: usize| {
                        data.get(at..at + 2)
                            .and_then(|s| s.try_into().ok())
                            .map(u16::from_be_bytes)
                            .ok_or_else(|| invalid("short SQLite interior cell"))
                    };
                    let get32 = |at: usize| {
                        data.get(at..at + 4)
                            .and_then(|s| s.try_into().ok())
                            .map(u32::from_be_bytes)
                            .ok_or_else(|| invalid("short SQLite child page"))
                    };
                    stack.push(get32(8)?);
                    for i in (0..usize::from(get16(3)?)).rev() {
                        let p = usize::from(get16(12 + 2 * i)?)
                            .checked_sub(start)
                            .ok_or_else(|| invalid("invalid SQLite cell offset"))?;
                        stack.push(get32(p)?);
                    }
                }
                _ => return Err(invalid("unsupported SQLite table storage")),
            }
        }
        Ok(())
    }
}
struct Columns<'a>(BTreeMap<&'a str, usize>);
impl Columns<'_> {
    fn get<'r>(&self, row: &'r Row, name: &str) -> Option<&'r SqlValue> {
        row.values.get(*self.0.get(name)?)
    }
    fn text<'r>(&self, row: &'r Row, name: &str) -> Option<&'r str> {
        match self.get(row, name)? {
            SqlValue::Text(v) => Some(v),
            _ => None,
        }
    }
    fn int(&self, row: &Row, name: &str) -> Option<i64> {
        match self.get(row, name)? {
            SqlValue::Integer(n) => Some(*n),
            _ => None,
        }
    }
}
pub(super) fn read(path: &Path, format: &str) -> io::Result<Vec<Entry>> {
    let db = Tables::open(path)?;
    if format == "opencode" {
        opencode::rows(&db)
    } else {
        antigravity::rows(&db, path)
    }
}
