use anyhow::Result;
use docs_engine::{Document, Selection};
use kar_documents::{
    Change, DocumentEdits, prepare,
    slides::{Object, ObjectEdit},
    word::{WordEdit, WordStyle, WordStyleEdit},
};
use slides_engine::{Paint, Presentation, Rgb};
use std::fs;
use tempfile::tempdir;

#[test]
fn word_text_and_direct_character_formatting_merge_at_a_run_boundary_in_either_order() -> Result<()>
{
    let dir = tempdir()?;
    let path = dir.path().join("minutes.docx");
    let mut doc = Document::new();
    doc.insert_text(Selection::default(), "日期周一", None);
    doc.save_as(&path)?;
    let base = fs::read(&path)?;
    let style = DocumentEdits::WordDocument {
        text: vec![],
        formatting: vec![WordStyleEdit {
            paragraph: 0,
            range: 6..12,
            property: WordStyle::Bold,
            change: Change {
                base: None,
                proposed: Some(true),
            },
        }],
    };
    let text = DocumentEdits::Word(vec![WordEdit {
        paragraph: 0,
        range: 6..12,
        replacement: "周三".into(),
    }]);
    for (first, second) in [(&style, &text), (&text, &style)] {
        let cur = prepare(&base, &base, first)?.unwrap();
        let both = prepare(&base, &cur, second)?.unwrap();
        fs::write(&path, both)?;
        let d = Document::open(&path)?;
        assert_eq!(d.paragraphs()[0].text, "日期周三");
        assert_eq!(d.paragraphs()[0].props_at(3).bold, None);
        assert_eq!(d.paragraphs()[0].props_at(12).bold, Some(true));
    }
    Ok(())
}

#[test]
fn shape_fill_updates_independently_of_text_and_preserves_competing_fill_proposal() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("review.pptx");
    let mut deck = Presentation::new();
    let i = deck.add_text_box(0)?;
    deck.set_shape_text((0, i), "文")?;
    deck.set_fill((0, i), Paint::Rgb(Rgb([17, 34, 51])))?;
    deck.save_as(&file)?;
    let obj = Object::at(&deck, 0, i)?;
    let base = fs::read(&file)?;
    let fill = |v| {
        DocumentEdits::Slides(vec![(
            obj.clone(),
            ObjectEdit::Fill(Change {
                base: Some(0x112233),
                proposed: Some(v),
            }),
        )])
    };
    let updated = prepare(&base, &base, &fill(0x334455))?.unwrap();
    assert!(
        prepare(&base, &updated, &fill(0xaabbcc))
            .unwrap_err()
            .is::<kar_documents::Conflict>()
    );
    let text = DocumentEdits::Slides(vec![(
        obj,
        ObjectEdit::Text(Change {
            base: "文".into(),
            proposed: "稿".into(),
        }),
    )]);
    fs::write(&file, prepare(&base, &updated, &text)?.unwrap())?;
    let deck = Presentation::open(&file)?;
    let shape = &deck.slide(0).shapes[i];
    assert_eq!(shape.look.fill, Some(Rgb([0x33, 0x44, 0x55])));
    assert_eq!(shape.text.as_ref().unwrap().plain_text(), "稿");
    Ok(())
}
