//! Infer operations from a full native save, then prove they account for
//! every changed package part before using them to update a newer revision.
use crate::{
    Change, OfficeEdits,
    excel::{Cell, CellEdit, sheet_index},
    slides::{Object, ObjectEdit},
    text::TextEdit,
    word::WordEdit,
};
use anyhow::{Context, Result, ensure};
use docs_engine::Document;
use sheets_engine::Workbook;
use slides_engine::Presentation;
use std::{
    collections::BTreeSet,
    fs,
    io::{Cursor, Read},
    path::Path,
};
use zip::ZipArchive;

fn same_parts(first: &[u8], second: &[u8]) -> Result<bool> {
    let mut left = ZipArchive::new(Cursor::new(first))?;
    let mut right = ZipArchive::new(Cursor::new(second))?;
    if left.len() != right.len() {
        return Ok(false);
    }
    let entries: BTreeSet<String> = left
        .file_names()
        .map(|name| name.map(|n| n.into_owned()))
        .collect::<zip::result::ZipResult<_>>()?;
    if entries.len() != left.len() {
        return Ok(false);
    }
    for name in entries {
        let mut a = left.by_name(&name)?;
        let Ok(mut b) = right.by_name(&name) else {
            return Ok(false);
        };
        if a.size() != b.size() {
            return Ok(false);
        }
        let (mut aa, mut bb) = ([0u8; 8192], [0u8; 8192]);
        let mut remaining = a.size();
        while remaining > 0 {
            let n = remaining.min(aa.len() as u64) as usize;
            a.read_exact(&mut aa[..n])?;
            b.read_exact(&mut bb[..n])?;
            if aa[..n] != bb[..n] {
                return Ok(false);
            }
            remaining -= n as u64;
        }
    }
    Ok(true)
}

fn word(before: &Path, proposed: &Path) -> Result<OfficeEdits> {
    let a = Document::open(before)?;
    let b = Document::open(proposed)?;
    ensure!(
        a.paragraphs().len() == b.paragraphs().len(),
        "paragraph structure changed"
    );
    let mut ops = Vec::new();
    let mut formatting = Vec::new();
    for (i, (old, new)) in a.paragraphs().iter().zip(b.paragraphs()).enumerate() {
        if old.text == new.text {
            let offsets: BTreeSet<_> = old
                .run_ranges()
                .chain(new.run_ranges())
                .flat_map(|(r, _)| [r.start, r.end])
                .collect();
            let offsets: Vec<_> = offsets.into_iter().collect();
            for pair in offsets.windows(2) {
                let (a, b) = (old.props_at(pair[1]), new.props_at(pair[1]));
                for property in [
                    crate::word::WordStyle::Bold,
                    crate::word::WordStyle::Italic,
                    crate::word::WordStyle::Underline,
                ] {
                    let (from, to) = (property.read(&a), property.read(&b));
                    if from != to {
                        formatting.push(crate::word::WordStyleEdit {
                            paragraph: i,
                            range: pair[0]..pair[1],
                            property,
                            change: Change {
                                base: from,
                                proposed: to,
                            },
                        });
                    }
                }
            }
            continue;
        }
        for e in TextEdit::between(&old.text, &new.text) {
            ops.push(WordEdit {
                paragraph: i,
                range: e.range,
                replacement: e.replacement,
            });
        }
    }
    Ok(OfficeEdits::WordDocument {
        text: ops,
        formatting,
    })
}

fn excel(before: &Path, proposed: &Path) -> Result<OfficeEdits> {
    let a = Workbook::open(before)?;
    let b = Workbook::open(proposed)?;
    ensure!(
        a.sheet_names().len() == b.sheet_names().len(),
        "worksheet list changed"
    );
    let mut ops = Vec::new();
    for i in 0..a.sheet_names().len() as u32 {
        let j = sheet_index(&b, a.sheet_id(i)).context("worksheet removed")?;
        let cells: BTreeSet<_> = a
            .filled_cells(i)
            .into_iter()
            .chain(b.filled_cells(j))
            .collect();
        for (r, c) in cells {
            let from = a.input(i, r, c);
            let to = b.input(j, r, c);
            let cell = Cell::at(&a, i, r, c)?;
            if from != to {
                ops.push((
                    cell.clone(),
                    CellEdit::Input(Change {
                        base: from,
                        proposed: to,
                    }),
                ))
            }
            let from = a.cell_style(i, r, c).fill;
            let to = b.cell_style(j, r, c).fill;
            if from != to {
                ops.push((
                    cell,
                    CellEdit::Fill(Change {
                        base: from,
                        proposed: to,
                    }),
                ))
            }
        }
    }
    Ok(OfficeEdits::Excel(ops))
}

fn slides(before: &Path, proposed: &Path) -> Result<OfficeEdits> {
    let a = Presentation::open(before)?;
    let b = Presentation::open(proposed)?;
    ensure!(a.len() == b.len(), "slide list changed");
    let mut ops = Vec::new();
    for (i, slide) in a.slides().iter().enumerate() {
        let j = crate::merge::unique_index(
            b.slides()
                .iter()
                .enumerate()
                .filter_map(|(j, s)| (s.sld_id == slide.sld_id).then_some(j)),
        )
        .context("slide missing or ambiguous")?;
        ensure!(
            slide.shapes.len() == b.slide(j).shapes.len(),
            "slide object list changed"
        );
        for (k, s) in slide.shapes.iter().enumerate() {
            let l = crate::merge::unique_index(
                b.slide(j)
                    .shapes
                    .iter()
                    .enumerate()
                    .filter_map(|(l, t)| (t.id == s.id).then_some(l)),
            )
            .context("shape missing or ambiguous")?;
            let t = &b.slide(j).shapes[l];
            let target = Object::at(&a, i, k)?;
            if let (Some(from), Some(to)) =
                (crate::slides::solid_fill(s), crate::slides::solid_fill(t))
                && from != to
            {
                ops.push((
                    target.clone(),
                    ObjectEdit::Fill(Change {
                        base: from,
                        proposed: to,
                    }),
                ));
            }
            if let (Some(x), Some(y)) = (&s.text, &t.text)
                && x.plain_text() != y.plain_text()
            {
                ops.push((
                    target.clone(),
                    ObjectEdit::Text(Change {
                        base: x.plain_text(),
                        proposed: y.plain_text(),
                    }),
                ));
            }
            if let (Some(x), Some(y)) = (a.effective_xfrm(i, s), b.effective_xfrm(j, t)) {
                for edit in [
                    (x.x != y.x).then_some(ObjectEdit::X(Change {
                        base: x.x,
                        proposed: y.x,
                    })),
                    (x.y != y.y).then_some(ObjectEdit::Y(Change {
                        base: x.y,
                        proposed: y.y,
                    })),
                    (x.cx != y.cx).then_some(ObjectEdit::Width(Change {
                        base: x.cx,
                        proposed: y.cx,
                    })),
                    (x.cy != y.cy).then_some(ObjectEdit::Height(Change {
                        base: x.cy,
                        proposed: y.cy,
                    })),
                ]
                .into_iter()
                .flatten()
                {
                    ops.push((target.clone(), edit))
                }
            }
        }
    }
    Ok(OfficeEdits::Slides(ops))
}

pub(crate) fn extract(kind: &OfficeEdits, base: &[u8], proposal: &[u8]) -> Result<OfficeEdits> {
    if matches!(kind, OfficeEdits::Text(_)) {
        return Ok(OfficeEdits::Text(TextEdit::between(
            std::str::from_utf8(base)?,
            std::str::from_utf8(proposal)?,
        )));
    }
    let dir = tempfile::tempdir()?;
    let (ext, read) = match kind {
        OfficeEdits::Word(_) | OfficeEdits::WordDocument { .. } => {
            ("docx", word as fn(&Path, &Path) -> Result<OfficeEdits>)
        }
        OfficeEdits::Excel(_) => ("xlsx", excel as _),
        OfficeEdits::Slides(_) => ("pptx", slides as _),
        OfficeEdits::Text(_) => unreachable!(),
    };
    let before = dir.path().join(format!("base.{ext}"));
    let after = dir.path().join(format!("proposal.{ext}"));
    fs::write(&before, base)?;
    fs::write(&after, proposal)?;
    let edits = read(&before, &after)?;
    let replay = crate::prepare(base, base, &edits)?;
    ensure!(
        same_parts(replay.as_deref().unwrap_or(base), proposal)?,
        "proposed package includes changes outside the identified properties"
    );
    Ok(edits)
}

pub(crate) fn validate(format: &OfficeEdits, bytes: &[u8]) -> Result<()> {
    let part = match format {
        OfficeEdits::Word(_) | OfficeEdits::WordDocument { .. } => Some("word/document.xml"),
        OfficeEdits::Excel(_) => Some("xl/workbook.xml"),
        OfficeEdits::Slides(_) => Some("ppt/presentation.xml"),
        OfficeEdits::Text(_) => None,
    };
    if let Some(main) = part {
        ZipArchive::new(Cursor::new(bytes))?.by_name(main)?;
    }
    crate::prepare(bytes, bytes, format)?;
    Ok(())
}
