use anyhow::Result;
use docs_engine::{Document, Selection};
use kar_documents::{
    Change, Conflict, OfficeEdits,
    excel::{Cell, CellEdit},
    prepare,
    slides::{Object, ObjectEdit},
    word::WordEdit,
};
use sheets_engine::Workbook;
use slides_engine::Presentation;
use std::fs;
use tempfile::tempdir;

fn text(old: &str, new: &str) -> Change<String> {
    Change {
        base: old.into(),
        proposed: new.into(),
    }
}

#[test]
fn word_candidate_is_atomic_and_accepts_both_arrival_orders() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("minutes.docx");
    let mut doc = Document::new();
    doc.insert_text(Selection::default(), "人\n分段\n助手", None);
    doc.save_as(&path)?;
    let base = fs::read(&path)?;
    let op = |paragraph, range, value: &str| WordEdit {
        paragraph,
        range,
        replacement: value.into(),
    };
    let a = op(0, 0..3, "作者");
    let b = op(2, 0..6, "读者");
    for (first, second) in [(&a, &b), (&b, &a)] {
        let current = prepare(&base, &base, &OfficeEdits::Word(vec![first.clone()]))?.unwrap();
        let proposed = prepare(&base, &current, &OfficeEdits::Word(vec![second.clone()]))?.unwrap();
        let output = dir.path().join("output.docx");
        fs::write(&output, &proposed)?;
        let doc = Document::open(&output)?;
        assert_eq!(doc.paragraphs()[0].text, "作者");
        assert_eq!(doc.paragraphs()[2].text, "读者");
        assert!(prepare(&base, &proposed, &OfficeEdits::Word(vec![second.clone()]))?.is_none());
    }
    let current = prepare(&base, &base, &OfficeEdits::Word(vec![a]))?.unwrap();
    let batch = OfficeEdits::Word(vec![b, op(0, 0..3, "主编")]);
    let err = prepare(&base, &current, &batch).unwrap_err();
    assert!(err.is::<Conflict>());
    assert_eq!(fs::read(&path)?, base);
    fs::write(&path, current)?;
    assert_eq!(Document::open(&path)?.paragraphs()[2].text, "助手");
    Ok(())
}

#[test]
fn excel_and_slides_use_immutable_base_and_current_snapshots() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("budget.xlsx");
    let mut workbook = Workbook::new_empty()?;
    workbook.set_input(0, 0, 0, "7")?;
    workbook.save_as(&file)?;
    let cell = Cell::at(&workbook, 0, 0, 0)?;
    let change = OfficeEdits::Excel(vec![(cell.clone(), CellEdit::Input(text("7", "=7*2")))]);
    let base = fs::read(&file)?;
    let current = prepare(&base, &base, &change)?.unwrap();
    assert!(kar_documents::prepare_with_grid_history(&base, &current, &change, &[])?.is_none());
    let forged = OfficeEdits::Excel(vec![(cell, CellEdit::Input(text("=7*2", "99")))]);
    assert!(kar_documents::prepare_with_grid_history(&base, &current, &forged, &[]).is_err());
    fs::write(&file, current)?;
    assert_eq!(Workbook::open(&file)?.display_value(0, 0, 0), "14");

    let file = dir.path().join("review.pptx");
    let mut deck = Presentation::new();
    let i = deck.add_text_box(0)?;
    deck.set_shape_text((0, i), "初稿")?;
    let object = Object::at(&deck, 0, i)?;
    deck.save_as(&file)?;
    let base = fs::read(&file)?;
    let change = OfficeEdits::Slides(vec![(
        object.clone(),
        ObjectEdit::Text(text("初稿", "二稿")),
    )]);
    let current = prepare(&base, &base, &change)?.unwrap();
    let incompatible = OfficeEdits::Slides(vec![(object, ObjectEdit::Text(text("初稿", "三稿")))]);
    let err = prepare(&base, &current, &incompatible).unwrap_err();
    assert_eq!(
        err.downcast_ref::<Conflict>().unwrap().current,
        Some(serde_json::json!("二稿"))
    );
    fs::write(&file, current)?;
    assert_eq!(
        Presentation::open(&file)?.slide(0).shapes[i]
            .text
            .as_ref()
            .unwrap()
            .plain_text(),
        "二稿"
    );
    Ok(())
}
