use crate::{
    Change, Outcome,
    excel::{Cell, CellEdit, sheet_index},
    merge::Comparison,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sheets_engine::{MAX_COLS, MAX_ROWS, Workbook};
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum GridAction {
    InsertRows,
    DeleteRows,
    InsertColumns,
    DeleteColumns,
}

/// One committed structural operation between the base and current
/// revisions. Position is zero-based in the revision immediately before it.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct GridChange {
    pub sheet_id: u32,
    pub action: GridAction,
    pub at: u32,
    pub count: u32,
}

impl GridChange {
    fn is_row(self) -> bool {
        matches!(self.action, GridAction::InsertRows | GridAction::DeleteRows)
    }
    fn is_insert(self) -> bool {
        matches!(
            self.action,
            GridAction::InsertRows | GridAction::InsertColumns
        )
    }
    fn limit(self) -> u32 {
        if self.is_row() { MAX_ROWS } else { MAX_COLS }
    }

    fn end(self) -> Result<u32> {
        ensure!(
            self.count != 0 && self.at < self.limit() && self.count <= self.limit() - self.at,
            "invalid structural operation"
        );
        Ok(self.at + self.count)
    }

    fn replay(self, book: &mut Workbook) -> Result<()> {
        let last = self.end()? - 1;
        let s = sheet_index(book, self.sheet_id)
            .context("structural operation references missing sheet")?;
        match self.action {
            GridAction::InsertRows => book.insert_rows(s, self.at, self.count),
            GridAction::DeleteRows => book.delete_rows(s, self.at, last),
            GridAction::InsertColumns => book.insert_columns(s, self.at, self.count),
            GridAction::DeleteColumns => book.delete_columns(s, self.at, last),
        }
    }

    fn translate(self, cell: &mut Cell) -> Result<bool> {
        let end = self.end()?;
        if cell.sheet_id != self.sheet_id {
            return Ok(true);
        }
        let pos = if self.is_row() {
            &mut cell.row
        } else {
            &mut cell.column
        };
        if *pos < self.at {
            return Ok(true);
        }
        if self.is_insert() {
            if *pos >= self.limit() - self.count {
                return Ok(false);
            }
            *pos += self.count;
        } else if *pos >= end {
            *pos -= self.count;
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}

pub(crate) fn merge_excel(
    base_file: &Path,
    latest: &mut Workbook,
    edits: &[(Cell, CellEdit)],
    history: &[GridChange],
    comparison: Comparison,
) -> Result<bool> {
    let mut base = Workbook::open(base_file)?;
    for (cell, edit) in edits {
        cell.apply_at(&mut base, edit, Comparison::Base)?;
    }
    let mut changed = false;
    if history.is_empty() {
        for (cell, edit) in edits {
            changed |= cell.apply_at(latest, edit, comparison)? == Outcome::Applied;
        }
        return Ok(changed);
    }
    // Replay the same grid operations on two independent models so Kantor
    // translates formula references, including references to another sheet.
    let mut proposed = Workbook::open(base_file)?;
    for (cell, edit) in edits {
        cell.apply(&mut proposed, edit)?;
    }
    for shift in history {
        shift.replay(&mut base)?;
        shift.replay(&mut proposed)?;
    }
    for (original, edit) in edits {
        let mut cell = original.clone();
        for shift in history {
            if !shift.translate(&mut cell)? {
                return Err(original
                    .missing(edit, "target row or column deleted")
                    .into());
            }
        }
        let sheet =
            sheet_index(&base, cell.sheet_id).context("target missing from projected base")?;
        let (r, c) = (cell.row, cell.column);
        let mapped = match edit {
            CellEdit::Input(_) => CellEdit::Input(Change {
                base: base.input(sheet, r, c),
                proposed: proposed.input(sheet, r, c),
            }),
            CellEdit::Fill(_) => CellEdit::Fill(Change {
                base: base.cell_style(sheet, r, c).fill,
                proposed: proposed.cell_style(sheet, r, c).fill,
            }),
        };
        changed |= cell.apply_at(latest, &mapped, comparison)? == Outcome::Applied;
    }
    Ok(changed)
}

pub(crate) fn apply_grid(bytes: &[u8], change: GridChange) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("workbook.xlsx");
    std::fs::write(&path, bytes)?;
    let mut workbook = Workbook::open(&path)?;
    ensure!(
        !workbook.has_macros(),
        "an XLSX save would remove workbook macros"
    );
    change.replay(&mut workbook)?;
    workbook.save()?;
    ensure!(
        workbook.save_notes().is_empty(),
        "lossy workbook save: {}",
        workbook.save_notes().join(", ")
    );
    Ok(std::fs::read(path)?)
}
