use crate::merge::{Change, Outcome, unique_index};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sheets_engine::{CellRange, MAX_COLS, MAX_ROWS, Workbook, cell_name, format::StyleChange};

/// Sheet identity comes from workbook.xml. Coordinates are zero-based.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub sheet_id: u32,
    pub sheet_name: String,
    pub row: u32,
    pub column: u32,
}

impl Cell {
    pub fn at(book: &Workbook, sheet: u32, row: u32, column: u32) -> Result<Self> {
        ensure!(
            row < MAX_ROWS && column < MAX_COLS,
            "cell outside worksheet"
        );
        let names = book.sheet_names();
        let name = names
            .get(sheet as usize)
            .ok_or_else(|| anyhow::anyhow!("unknown sheet"))?;
        Ok(Self {
            sheet_id: book.sheet_id(sheet),
            sheet_name: name.clone(),
            row,
            column,
        })
    }

    pub fn apply(&self, book: &mut Workbook, edit: &CellEdit) -> Result<Outcome> {
        ensure!(
            self.row < MAX_ROWS && self.column < MAX_COLS,
            "cell outside worksheet"
        );
        let sheets = book.sheet_names();
        let sheet = unique_index(
            (0..sheets.len()).filter(|&index| book.sheet_id(index as u32) == self.sheet_id),
        )
        .map(|i| i as u32);
        let name = sheet
            .map(|i| &sheets[i as usize])
            .unwrap_or(&self.sheet_name);
        let target = format!(
            "{name}!{} (sheetId {})",
            cell_name(self.row, self.column),
            self.sheet_id
        );
        match edit {
            CellEdit::Input(change) => {
                let value = sheet.map(|s| book.input(s, self.row, self.column));
                let result = change.compare(value.as_ref(), &target, "input")?;
                if let (Outcome::Applied, Some(s)) = (result, sheet) {
                    book.set_input(s, self.row, self.column, &change.proposed)?;
                }
                Ok(result)
            }
            CellEdit::Fill(change) => {
                ensure!(
                    change.proposed.is_none_or(|c| c <= 0xff_ffff),
                    "invalid RGB fill"
                );
                let value = sheet.map(|s| book.cell_style(s, self.row, self.column).fill);
                let result = change.compare(value.as_ref(), &target, "fill")?;
                if let (Outcome::Applied, Some(s)) = (result, sheet) {
                    let cell = (self.row, self.column);
                    book.set_range_style(
                        s,
                        CellRange::from_corners(cell, cell),
                        StyleChange::Fill(change.proposed),
                    )?;
                }
                Ok(result)
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CellEdit {
    Input(Change<String>),
    Fill(Change<Option<u32>>),
}
