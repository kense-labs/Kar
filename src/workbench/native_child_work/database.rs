use super::{
    Batch, CUT, Child, Launch, MAX_STEP, describe,
    profile::{self, Profile},
    valid_id,
};
use crate::workbench::{
    antigravity_proto::Fields,
    native_sqlite::{Tables, sqlite_err},
};
use sqlite_core::Database;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io,
};

const MAX_ROWS: usize = 200;
const MAX_SCAN: usize = 8 << 20;
fn plain(data: &Fields<'_>, field: u32) -> Option<String> {
    std::str::from_utf8(data.bytes(field)?)
        .ok()
        .map(str::to_owned)
}
struct Invoke {
    call_id: Option<String>,
    launches: Vec<Launch>,
    children: Vec<(String, String)>,
}
impl Invoke {
    fn decode(payload: &[u8]) -> Option<Self> {
        let row = Fields::decode(payload).ok()?;
        let invoke = Fields::decode(row.bytes(143)?).ok()?;
        let launches = invoke
            .repeated(9)
            .map(|s| {
                let s = Fields::decode(s.ok()?).ok()?;
                Some(Launch {
                    role: string_or_empty(&s, 2)?,
                    prompt: string_or_empty(&s, 3)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let children = invoke
            .repeated(10)
            .map(|a| {
                let a = Fields::decode(a.ok()?).ok()?;
                let id = plain(&a, 1)?;
                valid_id(&id).then_some((id, string_or_empty(&a, 2)?))
            })
            .collect::<Option<Vec<_>>>()?;
        let call_id = plain(&row.nested(5).ok()?.nested(4).ok()?, 1);
        Some(Self {
            call_id,
            launches,
            children,
        })
    }
    fn complete(self) -> Option<Vec<Child>> {
        if self.children.len() > 16 || self.children.len() != self.launches.len() {
            return None;
        }
        Some(
            self.children
                .into_iter()
                .zip(self.launches)
                .map(|((id, uri), launch)| Child {
                    id,
                    uri: Some(uri),
                    launch,
                })
                .collect(),
        )
    }
}
fn string_or_empty(s: &Fields<'_>, n: u32) -> Option<String> {
    if s.bytes(n).is_some() {
        plain(s, n)
    } else {
        Some(String::new())
    }
}
fn planner(payload: &[u8]) -> Option<Vec<String>> {
    let row = Fields::decode(payload).ok()?.nested(20).ok()?;
    let mut calls = vec![];
    for call in row.repeated(7) {
        let c = Fields::decode(call.ok()?).ok()?;
        if plain(&c, 2).as_deref() == Some("invoke_subagent")
            && let Some(id) = plain(&c, 1)
        {
            calls.push(id);
        }
    }
    Some(calls)
}
fn step(root: &str, id: &str) -> Option<i64> {
    let (r, n) = id.rsplit_once(':')?;
    if r.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(if r == root {
        n.parse().unwrap_or(-1)
    } else {
        -1
    })
}
fn paged(file: File) -> io::Result<Database> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        Database::open_path(format!("/dev/fd/{}", file.as_raw_fd())).map_err(sqlite_err)
    }
    #[cfg(not(unix))]
    {
        Database::open(profile::bytes(file, 512 << 20)?).map_err(sqlite_err)
    }
}
fn lookup(db: &Tables, root: &str, target: &Batch, siblings: &[Batch]) -> Option<Vec<Child>> {
    let index = step(root, target.id);
    if index.is_some_and(|n| n < 0) {
        return None;
    }
    let mut needed = HashSet::new();
    if let Some(index) = index {
        needed.insert(index);
    } else {
        let launch = target.description.as_ref()?;
        for sibling in siblings {
            if sibling.description.as_ref() == Some(launch) {
                return None;
            }
            match step(root, sibling.id) {
                Some(n) if n >= 0 => {
                    needed.insert(n);
                }
                None => {
                    let other = sibling.description.as_ref()?;
                    if other.title.contains(CUT)
                        || (other.prompt.contains(CUT) && other.title == launch.title)
                    {
                        return None;
                    }
                }
                _ => {}
            }
        }
    }
    let mut planners = HashMap::new();
    // Bound candidate metadata first. Native idx is not necessarily the table rowid.
    let mut rows = BTreeMap::new();
    db.records("steps", |row| {
        let idx = row.integer("idx")?.unwrap_or(row.rowid);
        let is_invoke = row.integer("step_type")? == Some(127);
        if !is_invoke && !needed.contains(&idx) {
            return Ok(());
        }
        let format = row.integer("step_format")?.ok_or_else(profile::invalid)?;
        let data = row.blob("step_payload")?;
        if format == 0 && needed.contains(&idx) {
            let calls = data
                .filter(|b| b.len() <= MAX_STEP)
                .map(|b| b.read(db, MAX_STEP))
                .transpose()?
                .and_then(|bytes| planner(&bytes));
            if planners.insert(idx, calls).is_some() {
                return Err(profile::invalid());
            }
        }
        if is_invoke && index.is_none_or(|start| idx > start && format == 0) {
            if rows.insert(idx, (data, format)).is_some() {
                return Err(profile::invalid());
            }
            if rows.len() > MAX_ROWS {
                if index.is_none() {
                    return Err(profile::invalid());
                }
                rows.pop_last();
            }
        }
        Ok(())
    })
    .ok()?;
    let mut claimed = HashSet::new();
    for i in needed {
        let ids = planners.remove(&i)??;
        if ids.is_empty() || (index.is_some() && ids.len() != 1) {
            return None;
        }
        claimed.extend(ids);
    }
    let mut bytes = 0usize;
    let mut matched = None;
    for (_idx, (blob, format)) in rows {
        let size = blob.as_ref().map_or(usize::MAX, |b| b.len());
        bytes = bytes.saturating_add(size);
        if size > MAX_STEP || bytes > MAX_SCAN || format != 0 {
            if index.is_some() {
                break;
            } else {
                return None;
            }
        }
        let record = Invoke::decode(&blob?.read(db, MAX_STEP).ok()?);
        if index.is_some() {
            if let Some(record) = record
                && record
                    .call_id
                    .as_ref()
                    .is_some_and(|id| claimed.contains(id))
            {
                return record.complete();
            }
        } else {
            let record = record?;
            if record
                .call_id
                .as_ref()
                .is_some_and(|id| claimed.contains(id))
            {
                continue;
            }
            if target.description.as_ref() == Some(&describe(&record.launches)) {
                if matched.is_some() {
                    return None;
                }
                matched = Some(record);
            }
        }
    }
    matched?.complete()
}
type Stamp = (u64, u64, u64, Option<std::time::SystemTime>);
fn stamp(file: &File) -> io::Result<Stamp> {
    let m = file.metadata()?;
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    };
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    Ok((device, inode, m.len(), m.modified().ok()))
}
pub(super) fn read(
    p: &Profile,
    root: &str,
    target: &Batch,
    siblings: &[Batch],
) -> Option<Vec<Child>> {
    let dir = p.dir(&["conversations"]).ok()?;
    let name = format!("{root}.db");
    let wal_name = format!("{name}-wal");
    let wal = || match profile::file(&dir, &wal_name) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    let file = profile::file(&dir, &name).ok()?;
    let wal_file = wal().ok()?;
    let prior = (
        stamp(&file).ok()?,
        wal_file.as_ref().map(stamp).transpose().ok()?,
    );
    let db = if let Some(wal) = wal_file.filter(|_| prior.1.as_ref().is_some_and(|s| s.2 > 0)) {
        Database::open_with_wal(
            profile::bytes(file, 512 << 20).ok()?,
            &profile::bytes(wal, 128 << 20).ok()?,
        )
        .ok()?
    } else {
        paged(file).ok()?
    };
    let found = lookup(&Tables::from_database(db), root, target, siblings)?;
    let after = (
        stamp(&profile::file(&dir, &name).ok()?).ok()?,
        wal().ok()?.as_ref().map(stamp).transpose().ok()?,
    );
    (prior == after).then_some(found)
}
