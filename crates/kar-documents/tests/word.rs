use anyhow::Result;
use docs_engine::{Document, Position, Selection, Toggle};
use kar_documents::{Conflict, Outcome, word::WordEdit};
use std::{fs::File, io::Read};
use tempfile::tempdir;

fn edit(doc: &Document, i: usize, old: &str, new: &str) -> WordEdit {
    let start = doc.paragraphs()[i].text.find(old).unwrap();
    WordEdit {
        paragraph: i,
        range: start..start + old.len(),
        replacement: new.into(),
    }
}

#[test]
fn follows_duplicate_paragraphs_after_insertion_and_preserves_bold_xml() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("notes.docx");
    let mut author = Document::new();
    author.insert_text(Selection::default(), "标题\n原样\n小李\n分隔\n小李", None);
    let span = Selection {
        anchor: Position::new(1, 0),
        head: Position::new(1, "原样".len()),
    };
    author.toggle(span, Toggle::Bold);
    author.save_as(&path)?;
    let base = Document::open(&path)?;
    let mut latest = Document::open(&path)?;
    latest.insert_text(Selection::default(), "新增\n", None);
    let third = edit(&base, 2, "小李", "小刘");
    let fifth = edit(&base, 4, "小李", "小王");
    fifth.apply(&base, &mut latest)?;
    third.apply(&base, &mut latest)?;
    assert_eq!(third.apply(&base, &mut latest)?, Outcome::Unchanged);
    latest.save()?;
    let reopened = Document::open(&path)?;
    let paragraphs: Vec<_> = reopened
        .paragraphs()
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(paragraphs, ["新增", "标题", "原样", "小刘", "分隔", "小王"]);
    let mut archive = zip::ZipArchive::new(File::open(&path)?)?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")?
        .read_to_string(&mut xml)?;
    let dom = roxmltree::Document::parse(&xml)?;
    let run = dom
        .descendants()
        .find(|n| {
            n.tag_name().name() == "r"
                && n.descendants()
                    .any(|d| d.tag_name().name() == "t" && d.text() == Some("原样"))
        })
        .unwrap();
    assert!(run.descendants().any(|d| d.tag_name().name() == "b"));
    Ok(())
}

#[test]
fn merges_disjoint_ranges_in_either_order_and_reports_conflicting_values() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("meeting.docx");
    let mut doc = Document::new();
    doc.insert_text(
        Selection::default(),
        "起\n上线日是周一，负责人是小李。\n止",
        None,
    );
    doc.save_as(&file)?;
    let extracted = kar_documents::extract::document_text("sample.docx", &std::fs::read(&file)?).unwrap();
    assert!(extracted.contains("上线日是周一，负责人是小李。"));
    let base = Document::open(&file)?;
    let day = edit(&base, 1, "周一", "下周三");
    let owner = edit(&base, 1, "小李", "小林");
    for edits in [[&day, &owner], [&owner, &day]] {
        let mut current = Document::open(&file)?;
        for op in edits {
            assert_eq!(op.apply(&base, &mut current)?, Outcome::Applied);
        }
        assert_eq!(
            current.paragraphs()[1].text,
            "上线日是下周三，负责人是小林。"
        );
    }
    for (first, second) in [("周三", "周五"), ("周五", "周三")] {
        let mut current = Document::open(&file)?;
        edit(&base, 1, "周一", first).apply(&base, &mut current)?;
        let err = edit(&base, 1, "周一", second)
            .apply(&base, &mut current)
            .unwrap_err();
        let conflict = err.downcast_ref::<Conflict>().unwrap();
        assert_eq!(conflict.current, Some(serde_json::json!(first)));
        assert_eq!(conflict.base, "周一");
        assert_eq!(conflict.proposed, second);
    }
    let mut deleted = Document::new();
    deleted.insert_text(Selection::default(), "起\n止", None);
    assert_eq!(
        day.apply(&base, &mut deleted)
            .unwrap_err()
            .downcast_ref::<Conflict>()
            .unwrap()
            .current,
        None
    );
    let mut split = Document::new();
    split.insert_text(
        Selection::default(),
        "起\n上线日是\n周一，负责人是小李。\n止",
        None,
    );
    assert!(day.apply(&base, &mut split).unwrap_err().is::<Conflict>());
    Ok(())
}

#[test]
fn retry_of_prefix_preserving_replacement_does_not_duplicate_the_suffix() -> Result<()> {
    let mut base = Document::new();
    base.insert_text(Selection::default(), "项目完成", None);
    let mut current = Document::new();
    current.insert_text(Selection::default(), "项目完成", None);
    let op = edit(&base, 0, "项目", "项目甲");
    assert_eq!(op.apply(&base, &mut current)?, Outcome::Applied);
    assert_eq!(op.apply(&base, &mut current)?, Outcome::Unchanged);
    assert_eq!(current.paragraphs()[0].text, "项目甲完成");
    Ok(())
}

#[test]
fn retry_after_independent_text_edit_preserves_the_single_suffix() -> Result<()> {
    let mut base = Document::new();
    base.insert_text(Selection::default(), "项目完成，日期待定。", None);
    let mut current = Document::new();
    current.insert_text(Selection::default(), "项目甲完成，日期周五。", None);
    let op = edit(&base, 0, "项目", "项目甲");
    assert_eq!(op.apply(&base, &mut current)?, Outcome::Unchanged);
    assert_eq!(current.paragraphs()[0].text, "项目甲完成，日期周五。");
    Ok(())
}
