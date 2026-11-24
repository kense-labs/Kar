use crate::{
    Outcome,
    excel::{Cell, CellEdit},
    merge::Comparison,
    slides::{Object, ObjectEdit},
    word::WordEdit,
};
use anyhow::{Result, ensure};
use docs_engine::Document;
use serde::{Deserialize, Serialize};
use sheets_engine::Workbook;
use slides_engine::Presentation;
use std::fs;
use tempfile::tempdir;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OfficeEdits {
    Word(Vec<WordEdit>),
    Excel(Vec<(Cell, CellEdit)>),
    Slides(Vec<(Object, ObjectEdit)>),
}

/// Prepare a complete candidate file against immutable revision snapshots.
/// On success, None is a no-op. On conflict the caller can downcast the
/// error to Conflict. Neither snapshot nor the working file is modified.
pub fn prepare(base: &[u8], current: &[u8], edits: &OfficeEdits) -> Result<Option<Vec<u8>>> {
    let dir = tempdir()?;
    let ext = match edits {
        OfficeEdits::Word(_) => "docx",
        OfficeEdits::Excel(_) => "xlsx",
        OfficeEdits::Slides(_) => "pptx",
    };
    let before = dir.path().join(format!("base.{ext}"));
    let candidate = dir.path().join(format!("current.{ext}"));
    fs::write(&before, base)?;
    fs::write(&candidate, current)?;
    let mut changed = false;
    match edits {
        OfficeEdits::Word(ops) => {
            let base = Document::open(&before)?;
            let mut model = Document::open(&candidate)?;
            for op in ops {
                changed |= op.apply(&base, &mut model)? == Outcome::Applied;
            }
            if changed {
                model.save()?;
            }
        }
        OfficeEdits::Excel(ops) => {
            let mut base = Workbook::open(&before)?;
            let mut model = Workbook::open(&candidate)?;
            for (target, op) in ops {
                target.apply_at(&mut base, op, Comparison::Base)?;
                changed |= target.apply(&mut model, op)? == Outcome::Applied;
            }
            if changed {
                ensure!(
                    !model.has_macros(),
                    "an XLSX save would remove workbook macros"
                );
                model.save()?;
                ensure!(
                    model.save_notes().is_empty(),
                    "lossy workbook save: {}",
                    model.save_notes().join(", ")
                );
            }
        }
        OfficeEdits::Slides(ops) => {
            let mut base = Presentation::open(&before)?;
            let mut model = Presentation::open(&candidate)?;
            for (target, op) in ops {
                target.apply_at(&mut base, op, Comparison::Base)?;
                changed |= target.apply(&mut model, op)? == Outcome::Applied;
            }
            if changed {
                model.save()?;
            }
        }
    }
    if changed {
        Ok(Some(fs::read(candidate)?))
    } else {
        Ok(None)
    }
}
