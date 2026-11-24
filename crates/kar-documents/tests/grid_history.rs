use anyhow::Result;
use kar_documents::{
    Change, Conflict, GridAction, GridChange, OfficeEdits,
    excel::{Cell, CellEdit},
    prepare_with_grid_history,
};
use sheets_engine::Workbook;
use std::fs;
use tempfile::tempdir;

fn input(a: &str, b: &str) -> CellEdit {
    CellEdit::Input(Change {
        base: a.into(),
        proposed: b.into(),
    })
}

#[test]
fn old_edit_follows_inserted_row_and_recalculates_translated_formula() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("预算.xlsx");
    let mut book = Workbook::new_empty()?;
    book.rename_sheet(0, "预算")?;
    book.set_input(0, 2, 1, "10")?;
    book.set_input(0, 2, 3, "=B3*2")?;
    book.save_as(&file)?;
    let base = fs::read(&file)?;
    let b3 = Cell::at(&book, 0, 2, 1)?;
    let d3 = Cell::at(&book, 0, 2, 3)?;
    let shift = GridChange {
        sheet_id: b3.sheet_id,
        action: GridAction::InsertRows,
        at: 1,
        count: 1,
    };
    let ops = OfficeEdits::Excel(vec![(b3, input("10", "12")), (d3, input("=B3*2", "=B3*3"))]);
    book.insert_rows(0, 1, 1)?;
    book.set_input(0, 2, 1, "99")?;
    book.save()?;
    let current = fs::read(&file)?;
    let result = prepare_with_grid_history(&base, &current, &ops, &[shift])?.unwrap();
    assert!(prepare_with_grid_history(&base, &result, &ops, &[shift])?.is_none());
    fs::write(&file, result)?;
    let verified = Workbook::open(&file)?;
    assert_eq!(verified.input(0, 2, 1), "99");
    assert_eq!(verified.input(0, 3, 1), "12");
    assert_eq!(verified.input(0, 3, 3), "=B4*3");
    assert_eq!(verified.display_value(0, 3, 3), "36");
    Ok(())
}

#[test]
fn deleted_logical_column_conflicts_even_if_adjacent_cell_has_the_base_value() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("income.xlsx");
    let mut book = Workbook::new_empty()?;
    book.set_input(0, 0, 1, "10")?;
    book.set_input(0, 0, 2, "10")?;
    book.save_as(&path)?;
    let base = fs::read(&path)?;
    let cell = Cell::at(&book, 0, 0, 1)?;
    let delete = GridChange {
        sheet_id: cell.sheet_id,
        action: GridAction::DeleteColumns,
        at: 1,
        count: 1,
    };
    let ops = OfficeEdits::Excel(vec![(cell, input("10", "12"))]);
    book.delete_columns(0, 1, 1)?;
    book.save()?;
    let current = fs::read(&path)?;
    let untracked = kar_documents::prepare(&base, &current, &ops).unwrap_err();
    assert_eq!(
        untracked.downcast_ref::<Conflict>().unwrap().reason,
        "worksheet structure history unavailable"
    );
    let error = prepare_with_grid_history(&base, &current, &ops, &[delete]).unwrap_err();
    let conflict = error.downcast_ref::<Conflict>().unwrap();
    assert!(conflict.target.contains("B1"));
    assert_eq!(conflict.current, None);
    assert_eq!(fs::read(&path)?, current);
    Ok(())
}
