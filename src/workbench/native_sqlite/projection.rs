//! A bound on a native BLOB must be checked before sqlite-core reassembles its overflow pages.
//! SQLite file format sections 1.6 (table leaf cells), 1.7 (overflow), 2.1 (record serial types).
use super::{Columns, Tables, invalid};
use std::{collections::HashSet, io, ops::Range};

fn malformed() -> io::Error {
    invalid("invalid SQLite table record")
}
fn varint(bytes: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut out = 0u64;
    for n in 0..9 {
        let byte = *bytes.get(*at).ok_or_else(malformed)?;
        *at += 1;
        if n == 8 {
            return Ok((out << 8) | u64::from(byte));
        }
        out = (out << 7) | u64::from(byte & 0x7f);
        if byte < 128 {
            return Ok(out);
        }
    }
    unreachable!()
}
fn word(b: &[u8], at: usize) -> io::Result<usize> {
    Ok(usize::from(u16::from_be_bytes(
        b.get(at..at + 2)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(malformed)?,
    )))
}
fn next(b: &[u8], at: usize) -> io::Result<u32> {
    Ok(u32::from_be_bytes(
        b.get(at..at + 4)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(malformed)?,
    ))
}
#[derive(Clone, Copy)]
pub(in crate::workbench) struct Blob {
    page: u32,
    local_at: usize,
    local_len: usize,
    start: usize,
    size: usize,
}
impl Blob {
    pub(in crate::workbench) fn len(&self) -> usize {
        self.size
    }
    pub(in crate::workbench) fn read(&self, tables: &Tables, max: usize) -> io::Result<Vec<u8>> {
        if self.size > max {
            return Err(invalid("SQLite selected column exceeds limit"));
        }
        let page = tables.db.raw_page(self.page).ok_or_else(malformed)?;
        let local = page
            .get(self.local_at..self.local_at + self.local_len)
            .ok_or_else(malformed)?;
        let end = self.start.checked_add(self.size).ok_or_else(malformed)?;
        if self.size == 0 {
            return Ok(vec![]);
        }
        if end <= self.local_len {
            return Ok(local[self.start..end].to_vec());
        }
        let mut out = Vec::with_capacity(self.size);
        if self.start < self.local_len {
            out.extend_from_slice(&local[self.start..]);
        }
        let mut overflow = next(&page, self.local_at + self.local_len)?;
        let usable = tables.db.header().usable_size() as usize;
        let mut offset = self.local_len;
        let mut seen = HashSet::new();
        while offset < end {
            if overflow == 0 || !seen.insert(overflow) {
                return Err(malformed());
            }
            let page = tables.db.raw_page(overflow).ok_or_else(malformed)?;
            let payload = page.get(4..usable).ok_or_else(malformed)?;
            let from = self.start.saturating_sub(offset).min(payload.len());
            let through = end.saturating_sub(offset).min(payload.len());
            out.extend_from_slice(&payload[from..through]);
            offset = offset.checked_add(payload.len()).ok_or_else(malformed)?;
            overflow = next(&page, 0)?;
        }
        Ok(out)
    }
}
struct Field {
    serial: u64,
    range: Range<usize>,
}
pub(in crate::workbench) struct Record<'a> {
    pub rowid: i64,
    columns: &'a Columns<'a>,
    fields: Vec<Field>,
    payload: Blob,
    db: &'a Tables,
}
fn length(serial: u64) -> io::Result<usize> {
    Ok(match serial {
        0 | 8 | 9 => 0,
        1..=4 => serial as usize,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => return Err(malformed()),
        s => usize::try_from((s - 12) / 2).map_err(|_| malformed())?,
    })
}
impl Record<'_> {
    fn field(&self, key: &str) -> io::Result<&Field> {
        self.columns
            .0
            .get(key)
            .and_then(|i| self.fields.get(*i))
            .ok_or_else(malformed)
    }
    fn slice(&self, field: &Field) -> Blob {
        Blob {
            start: field.range.start,
            size: field.range.len(),
            ..self.payload
        }
    }
    pub(in crate::workbench) fn integer(&self, key: &str) -> io::Result<Option<i64>> {
        let field = self.field(key)?;
        match field.serial {
            0 => Ok(None),
            8 | 9 => Ok(Some(field.serial as i64 - 8)),
            1..=6 => {
                let b = self.slice(field).read(self.db, 8)?;
                let mut signed = if b[0] & 128 != 0 { -1i64 } else { 0 };
                for n in b {
                    signed = (signed << 8) | i64::from(n);
                }
                Ok(Some(signed))
            }
            _ => Err(malformed()),
        }
    }
    pub(in crate::workbench) fn blob(&self, key: &str) -> io::Result<Option<Blob>> {
        let field = self.field(key)?;
        Ok((field.serial >= 12 && field.serial % 2 == 0).then(|| self.slice(field)))
    }
}
fn parse<'a>(
    tables: &'a Tables,
    cols: &'a Columns<'a>,
    pg: u32,
    bytes: &[u8],
    mut at: usize,
) -> io::Result<Record<'a>> {
    let usable = tables.db.header().usable_size() as usize;
    let total = usize::try_from(varint(bytes, &mut at)?).map_err(|_| malformed())?;
    let rowid = varint(bytes, &mut at)? as i64;
    let max_local = usable.checked_sub(35).ok_or_else(malformed)?;
    let local_len = if total <= max_local {
        total
    } else {
        let min = (usable - 12) * 32 / 255 - 23;
        let candidate = min + (total - min) % (usable - 4);
        if candidate <= max_local {
            candidate
        } else {
            min
        }
    };
    let local_end = at
        .checked_add(local_len)
        .filter(|n| *n <= usable)
        .ok_or_else(malformed)?;
    let local = bytes.get(at..local_end).ok_or_else(malformed)?;
    let mut header = 0;
    let header_len = usize::try_from(varint(local, &mut header)?).map_err(|_| malformed())?;
    // The small fixed trajectory schema always keeps its column headers on the leaf.
    let h = local.get(..header_len).ok_or_else(malformed)?;
    if header > header_len {
        return Err(malformed());
    }
    let mut fields = Vec::new();
    let mut start = header_len;
    while header < header_len {
        let serial = varint(h, &mut header)?;
        let end = start
            .checked_add(length(serial)?)
            .filter(|n| *n <= total)
            .ok_or_else(malformed)?;
        fields.push(Field {
            serial,
            range: start..end,
        });
        start = end;
    }
    if start != total {
        return Err(malformed());
    }
    Ok(Record {
        rowid,
        columns: cols,
        fields,
        payload: Blob {
            page: pg,
            local_at: at,
            local_len,
            start: 0,
            size: total,
        },
        db: tables,
    })
}
impl Tables {
    pub(in crate::workbench) fn records(
        &self,
        name: &str,
        mut consume: impl FnMut(Record) -> io::Result<()>,
    ) -> io::Result<()> {
        self.leaves(name, |pg, bytes, h, cols| {
            for i in 0..word(bytes, h + 3)? {
                let at = word(bytes, h + 8 + 2 * i)?;
                consume(parse(self, cols, pg, bytes, at)?)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlite_core::{
        Database, Value,
        rebuild::{RecoveredTable, build_recovered_db_tables},
    };
    #[test]
    fn unrelated_overflow_is_never_followed_and_bounded_blobs_cross_pages_intact() {
        let payload = (0..28000).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        let mut bytes = build_recovered_db_tables(&[RecoveredTable {
            name: "steps".into(),
            columns: vec!["idx".into(), "step_payload".into()],
            rows: vec![
                vec![Value::Integer(-32769), Value::Blob(vec![42; 1_100_000])],
                vec![Value::Integer(1), Value::Blob(payload.clone())],
            ],
        }]);
        let db = Tables::from_database(Database::open(bytes.clone()).unwrap());
        let mut big = None;
        db.records("steps", |r| {
            let b = r.blob("step_payload")?.unwrap();
            if r.integer("idx")? == Some(-32769) {
                big = Some(b);
                assert!(b.read(&db, 1 << 20).is_err());
            }
            Ok(())
        })
        .unwrap();
        let big = big.unwrap();
        let ptr = (big.page - 1) as usize * db.db.header().page_size as usize
            + big.local_at
            + big.local_len;
        bytes[ptr..ptr + 4].fill(0);
        let db = Tables::from_database(Database::open(bytes).unwrap());
        let mut hits = 0;
        db.records("steps", |r| {
            if r.integer("idx")? == Some(1) {
                hits += 1;
                assert_eq!(
                    r.blob("step_payload")?.unwrap().read(&db, 1 << 20)?,
                    payload
                );
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(hits, 1);
    }
}
