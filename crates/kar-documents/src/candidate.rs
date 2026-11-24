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
    Text(Vec<crate::text::TextEdit>),
    Word(Vec<WordEdit>),
    WordDocument {
        text: Vec<WordEdit>,
        formatting: Vec<crate::word::WordStyleEdit>,
    },
    Excel(Vec<(Cell, CellEdit)>),
    Slides(Vec<(Object, ObjectEdit)>),
}

impl OfficeEdits {
    pub(crate) fn extension(&self) -> &'static str {
        match self {
            Self::Text(_) => "txt",
            Self::Word(_) | Self::WordDocument { .. } => "docx",
            Self::Excel(_) => "xlsx",
            Self::Slides(_) => "pptx",
        }
    }
}

/// Prepare a complete candidate file against immutable revision snapshots.
/// On success, None is a no-op. On conflict the caller can downcast the
/// error to Conflict. Neither snapshot nor the working file is modified.
pub fn prepare(base: &[u8], current: &[u8], edits: &OfficeEdits) -> Result<Option<Vec<u8>>> {
    // A changed worksheet package cannot prove that an address still names
    // the same logical cell. Internal property-only histories use the
    // explicit entry point below with an empty structural journal.
    if let OfficeEdits::Excel(ops) = edits
        && base != current
        && let Some((cell, edit)) = ops.first()
    {
        return Err(cell
            .missing(edit, "worksheet structure history unavailable")
            .into());
    }
    prepare_with_grid_history(base, current, edits, &[])
}

/// Apply a proposal using the service's committed row/column history.
/// An empty history asserts that the logical cell coordinates did not shift.
pub fn prepare_with_grid_history(
    base: &[u8],
    current: &[u8],
    edits: &OfficeEdits,
    history: &[crate::GridChange],
) -> Result<Option<Vec<u8>>> {
    prepare_staged(base, current, edits, history, Comparison::Current, false)
}

pub(crate) fn prepare_staged(
    base: &[u8],
    current: &[u8],
    edits: &OfficeEdits,
    history: &[crate::GridChange],
    comparison: Comparison,
    paragraphs_stable: bool,
) -> Result<Option<Vec<u8>>> {
    ensure!(
        matches!(edits, OfficeEdits::Excel(_)) || history.is_empty(),
        "grid history supplied for a non-spreadsheet edit"
    );
    if let OfficeEdits::Text(ops) = edits {
        let base = std::str::from_utf8(base)?;
        let original = std::str::from_utf8(current)?;
        let mut candidate = original.to_owned();
        for op in ops {
            op.apply(base, &mut candidate, comparison)?;
        }
        return Ok((candidate != original).then(|| candidate.into_bytes()));
    }
    let dir = tempdir()?;
    let ext = edits.extension();
    let before = dir.path().join(format!("base.{ext}"));
    let candidate = dir.path().join(format!("current.{ext}"));
    fs::write(&before, base)?;
    fs::write(&candidate, current)?;
    let mut changed = false;
    match edits {
        OfficeEdits::Text(_) => unreachable!(),
        OfficeEdits::Word(ops) | OfficeEdits::WordDocument { text: ops, .. } => {
            let base = Document::open(&before)?;
            let mut model = Document::open(&candidate)?;
            for op in ops {
                changed |= op.apply_at(&base, &mut model, comparison, paragraphs_stable)?
                    == Outcome::Applied;
            }
            if let OfficeEdits::WordDocument { formatting, .. } = edits {
                for op in formatting {
                    changed |= op.apply(&base, &mut model, comparison, paragraphs_stable)?
                        == Outcome::Applied;
                }
            }
            if changed {
                model.save()?;
            }
        }
        OfficeEdits::Excel(ops) => {
            let mut model = Workbook::open(&candidate)?;
            changed =
                crate::grid_history::merge_excel(&before, &mut model, ops, history, comparison)?;
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
                changed |= target.apply_at(&mut model, op, comparison)? == Outcome::Applied;
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
