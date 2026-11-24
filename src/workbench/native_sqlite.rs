//! Read live table leaves from an already opened native SQLite snapshot.
mod projection;
use sqlite_core::{Database, Row, Value as SqlValue};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};
pub(super) fn invalid(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m)
}
pub(super) fn sqlite_err(e: sqlite_core::Error) -> io::Error {
    invalid(&format!("reading native SQLite database: {e:?}"))
}
pub(super) struct Tables {
    db: Database,
    tables: BTreeMap<String, (u32, Vec<String>)>,
}
impl Tables {
    pub(super) fn from_database(db: Database) -> Self {
        let tables = db
            .live_tables()
            .into_iter()
            .filter_map(|t| Some((t.name, (t.rootpage, t.column_names?))))
            .collect();
        Self { db, tables }
    }
    pub(super) fn has_table(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }
    pub(super) fn read(
        &self,
        name: &str,
        mut consume: impl FnMut(&Columns, Row) -> io::Result<()>,
    ) -> io::Result<()> {
        self.leaves(name, |page, _, _, columns| {
            for row in self
                .db
                .read_table(page, columns.0.len())
                .map_err(sqlite_err)?
            {
                consume(columns, row)?;
            }
            Ok(())
        })
    }
    fn leaves(
        &self,
        name: &str,
        mut visit: impl FnMut(u32, &[u8], usize, &Columns) -> io::Result<()>,
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
            let full = self
                .db
                .raw_page(page)
                .ok_or_else(|| invalid("missing SQLite page"))?;
            let start = if page == 1 { 100 } else { 0 };
            let data = full
                .get(start..)
                .ok_or_else(|| invalid("short SQLite page"))?;
            match data.first() {
                Some(13) => visit(page, &full, start, &cols)?,
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
pub(super) struct Columns<'a>(BTreeMap<&'a str, usize>);
impl Columns<'_> {
    pub(super) fn get<'r>(&self, row: &'r Row, name: &str) -> Option<&'r SqlValue> {
        row.values.get(*self.0.get(name)?)
    }
    pub(super) fn text<'r>(&self, row: &'r Row, name: &str) -> Option<&'r str> {
        match self.get(row, name)? {
            SqlValue::Text(v) => Some(v),
            _ => None,
        }
    }
    pub(super) fn int(&self, row: &Row, name: &str) -> Option<i64> {
        match self.get(row, name)? {
            SqlValue::Integer(n) => Some(*n),
            _ => None,
        }
    }
}
