use super::{Process, Table};
#[cfg(target_os = "linux")]
use nix::unistd::{SysconfVar, sysconf};
use std::{fs, io, path::Path};

fn read(dir: &Path, pid: u32, page: u64) -> io::Result<Process> {
    let raw = fs::read(dir.join(pid.to_string()).join("stat"))?;
    // The command name can include spaces, a right parenthesis and non-UTF-8 bytes.
    let end = raw
        .iter()
        .rposition(|&b| b == b')')
        .ok_or(io::ErrorKind::InvalidData)?;
    let fields: Vec<_> = raw[end + 1..]
        .split(|b| b.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .collect();
    let number = |i: usize| {
        fields
            .get(i)
            .and_then(|s| std::str::from_utf8(s).ok()?.parse::<u64>().ok())
            .ok_or(io::ErrorKind::InvalidData)
    };
    Ok(Process {
        parent: u32::try_from(number(1)?).map_err(|_| io::ErrorKind::InvalidData)?,
        group: u32::try_from(number(2)?).map_err(|_| io::ErrorKind::InvalidData)?,
        born: number(19)?,
        rss: number(21)?.checked_mul(page),
        zombie: matches!(fields.first().copied(), Some(b"Z" | b"X")),
    })
}
#[cfg(target_os = "linux")]
pub(super) fn birth(pid: u32) -> Option<u64> {
    Some(read(Path::new("/proc"), pid, 1).ok()?.born)
}
#[cfg(target_os = "linux")]
pub(super) fn table() -> Option<Table> {
    let page: u64 = sysconf(SysconfVar::PAGE_SIZE).ok()??.try_into().ok()?;
    if page == 0 {
        return None;
    }
    table_at(Path::new("/proc"), page)
}
fn table_at(dir: &Path, page: u64) -> Option<Table> {
    let mut rows = Table::new();
    for entry in fs::read_dir(dir).ok()? {
        let name = entry.ok()?.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        match read(dir, pid, page) {
            Ok(p) => {
                rows.insert(pid, p);
            }
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(nix::libc::ESRCH) => {}
            Err(_) => return None,
        }
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_non_utf8_process_names_races_and_partial_tables() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("10")).unwrap();
        fs::create_dir(dir.path().join("11")).unwrap();
        let path = dir.path().join("10/stat");
        let mut raw = b"10 (na)me \xff) S 1 10".to_vec();
        for field in 6..=24 {
            let count = match field {
                22 => 321,
                24 => 4,
                _ => 0,
            };
            raw.extend(format!(" {count}").bytes());
        }
        fs::write(&path, raw).unwrap();
        let table = table_at(dir.path(), 16384).unwrap();
        assert_eq!(table.len(), 1);
        let row = table[&10];
        assert_eq!(
            (row.parent, row.group, row.born, row.rss),
            (1, 10, 321, Some(65536))
        );
        fs::write(path, b"10 malformed").unwrap();
        assert!(table_at(dir.path(), 16384).is_none());
    }
}
