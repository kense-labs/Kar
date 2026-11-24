use anyhow::Result;
use kar_documents::{
    Change, Conflict, Outcome,
    excel::{Cell, CellEdit},
    slides::{Object, ObjectEdit},
};
use sheets_engine::{Tab, Workbook};
use slides_engine::{
    Presentation, Xfrm,
    author::{Element, ElementKind, GeomSpec, ShapeSpec, TextSpec},
};
use std::{fs::File, io::Read, path::Path};
use tempfile::tempdir;
use zip::ZipArchive;

fn change<T>(base: T, proposed: T) -> Change<T> {
    Change { base, proposed }
}

fn text(a: &str, b: &str) -> Change<String> {
    change(a.into(), b.into())
}

fn part(file: &Path, name: &str) -> Result<Vec<u8>> {
    let mut archive = ZipArchive::new(File::open(file)?)?;
    let mut data = Vec::new();
    archive.by_name(name)?.read_to_end(&mut data)?;
    Ok(data)
}

#[test]
fn excel_input_formula_and_fill_merge_on_named_sheet_after_tab_reorder() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("预算.xlsx");
    let mut book = Workbook::new_empty()?;
    book.rename_sheet(0, "预算")?;
    book.insert_sheet(1)?;
    book.rename_sheet(1, "其他")?;
    book.set_input(0, 2, 1, "10")?;
    book.set_input(1, 2, 1, "99")?;
    book.set_input(0, 2, 3, "=B3*2")?;
    book.save_as(&path)?;
    let mut book = Workbook::open(&path)?;
    let b3 = Cell::at(&book, 0, 2, 1)?;
    let d3 = Cell::at(&book, 0, 2, 3)?;
    let edit = CellEdit::Input(text("10", "12"));
    book.move_tab(Tab::Sheet(0), 1)?;
    b3.apply(&mut book, &CellEdit::Fill(change(None, Some(0xffcc00))))?;
    assert_eq!(b3.apply(&mut book, &edit)?, Outcome::Applied);
    assert_eq!(b3.apply(&mut book, &edit)?, Outcome::Unchanged);
    d3.apply(&mut book, &CellEdit::Input(text("=B3*2", "=B3*3")))?;
    let err = b3
        .apply(&mut book, &CellEdit::Input(text("10", "=6*2")))
        .unwrap_err();
    let conflict = err.downcast_ref::<Conflict>().unwrap();
    assert!(conflict.target.contains("预算!B3"));
    assert_eq!(conflict.current, Some(serde_json::json!("12")));
    assert_eq!(conflict.proposed, "=6*2");
    book.save()?;
    let book = Workbook::open(&path)?;
    let primary = book.sheet_names().iter().position(|s| s == "预算").unwrap() as u32;
    let other = 1 - primary;
    assert_eq!(book.input(primary, 2, 1), "12");
    assert_eq!(book.input(primary, 2, 3), "=B3*3");
    assert_eq!(book.display_value(primary, 2, 3), "36");
    assert_eq!(book.cell_style(primary, 2, 1).fill, Some(0xffcc00));
    assert_eq!(book.input(other, 2, 1), "99");
    // Find the worksheet by its contents; worksheet part numbering can change at save.
    let parts: Vec<_> = [1, 2]
        .into_iter()
        .map(|i| part(&path, &format!("xl/worksheets/sheet{i}.xml")))
        .collect::<Result<_>>()?;
    assert!(parts.iter().any(|bytes| {
        let xml = roxmltree::Document::parse(std::str::from_utf8(bytes).unwrap()).unwrap();
        xml.descendants().any(|n| {
            n.tag_name().name() == "c"
                && n.attribute("r") == Some("D3")
                && n.children()
                    .any(|c| c.tag_name().name() == "f" && c.text() == Some("B3*3"))
                && n.children()
                    .any(|c| c.tag_name().name() == "v" && c.text() == Some("36"))
        })
    }));
    Ok(())
}

fn named_box(deck: &mut Presentation, text: &str, x: i64) -> Result<usize> {
    deck.add_element(
        0,
        &Element {
            kind: ElementKind::Shape(ShapeSpec {
                geom: GeomSpec::preset("rect"),
                text_box: true,
                fill: None,
                line: None,
                text: Some(TextSpec::plain(text)),
                style: Default::default(),
            }),
            name: Some("同名".into()),
            xfrm: Xfrm::new(x, 100, 200_000, 100_000),
        },
    )
}

#[test]
fn slides_merge_after_slide_and_layer_reorder_preserves_media_and_relationships() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("会议.pptx");
    let mut deck = Presentation::new();
    let first = named_box(&mut deck, "甲", 1000)?;
    let second = named_box(&mut deck, "乙", 2000)?;
    let object = Object::at(&deck, 0, first)?;
    let other = Object::at(&deck, 0, second)?;
    let png = include_bytes!("pixel.png");
    deck.add_picture(0, png.to_vec())?;
    deck.new_slide(Some(0))?;
    deck.save_as(&path)?;
    let source_part = deck.slide(0).part.clone();
    let rel_part = format!(
        "ppt/slides/_rels/{}.rels",
        source_part.rsplit('/').next().unwrap()
    );
    let rels = part(&path, &rel_part)?;
    let mut deck = Presentation::open(&path)?;
    deck.move_slide(0, 1)?;
    deck.reorder_shape(1, first, second)?;
    let set_text = ObjectEdit::Text(text("甲", "提案"));
    object.apply(&mut deck, &set_text)?;
    object.apply(&mut deck, &ObjectEdit::X(change(1000, 3500)))?;
    assert_eq!(object.apply(&mut deck, &set_text)?, Outcome::Unchanged);
    let err = object
        .apply(&mut deck, &ObjectEdit::X(change(1000, 6000)))
        .unwrap_err();
    let conflict = err.downcast_ref::<Conflict>().unwrap();
    assert_eq!(conflict.property, "x");
    assert_eq!(conflict.current, Some(serde_json::json!(3500)));
    assert!(conflict.target.contains("同名"));
    deck.save()?;
    let mut deck = Presentation::open(&path)?;
    let i = deck.shape_index(1, object.shape_id).unwrap();
    let j = deck.shape_index(1, other.shape_id).unwrap();
    assert_eq!(
        deck.slide(1).shapes[i].text.as_ref().unwrap().plain_text(),
        "提案"
    );
    assert_eq!(deck.slide(1).shapes[i].xfrm.unwrap().x, 3500);
    assert_eq!(
        deck.slide(1).shapes[j].text.as_ref().unwrap().plain_text(),
        "乙"
    );
    assert_eq!(deck.slide(1).shapes[j].xfrm.unwrap().x, 2000);
    let bytes = part(&path, &source_part)?;
    let xml = roxmltree::Document::parse(std::str::from_utf8(&bytes)?)?;
    let shape = xml
        .descendants()
        .find(|n| {
            n.tag_name().name() == "sp"
                && n.descendants().any(|d| {
                    d.tag_name().name() == "cNvPr"
                        && d.attribute("id") == Some(object.shape_id.to_string().as_str())
                })
        })
        .unwrap();
    assert!(
        shape
            .descendants()
            .any(|n| n.tag_name().name() == "off" && n.attribute("x") == Some("3500"))
    );
    assert!(
        shape
            .descendants()
            .any(|n| n.tag_name().name() == "t" && n.text() == Some("提案"))
    );
    assert_eq!(part(&path, &rel_part)?, rels);
    let rels = roxmltree::Document::parse(std::str::from_utf8(&rels)?)?;
    let media = rels
        .descendants()
        .find(|n| n.attribute("Type").is_some_and(|s| s.ends_with("/image")))
        .unwrap()
        .attribute("Target")
        .unwrap();
    assert_eq!(
        part(
            &path,
            &format!("ppt/{}", media.strip_prefix("../").unwrap())
        )?,
        png
    );
    deck.delete_shape((1, i))?;
    let err = object.apply(&mut deck, &set_text).unwrap_err();
    assert_eq!(err.downcast_ref::<Conflict>().unwrap().current, None);
    Ok(())
}
