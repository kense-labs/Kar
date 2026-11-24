//! Headless document operations used after Kar resolves an edit target.
mod candidate;
pub use candidate::{OfficeEdits, prepare};
pub mod excel;
mod merge;
pub mod slides;
pub mod word;
pub use merge::{Change, Conflict, Outcome};

use anyhow::{Result, ensure};
use docs_engine::{Document, Position, Selection};
use std::ops::Range;

/// Apply a checked text edit inside one resolved Word paragraph.
/// Offsets are UTF-8 byte offsets, as used by the document engine.
pub fn replace_word_text(
    document: &mut Document,
    paragraph: usize,
    range: Range<usize>,
    expected: &str,
    replacement: &str,
) -> Result<()> {
    let current = document
        .paragraphs()
        .get(paragraph)
        .and_then(|p| p.text.get(range.clone()));
    ensure!(
        current == Some(expected),
        "the selected paragraph text has changed or the range is invalid"
    );
    ensure!(
        !replacement.contains(['\r', '\n']),
        "a paragraph text edit cannot split the paragraph"
    );
    if expected != replacement {
        let selection = Selection {
            anchor: Position::new(paragraph, range.start),
            head: Position::new(paragraph, range.end),
        };
        document.insert_text(selection, replacement, None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_survive_docx_reopen_and_conflicting_text_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("minutes.docx");
        let mut original = Document::new();
        original.insert_text(Selection::default(), "周一由小李主持\n其他段落", None);
        original.save_as(&file).unwrap();
        let mut current = Document::open(&file).unwrap();
        replace_word_text(&mut current, 0, 0..6, "周一", "周三").unwrap();
        replace_word_text(&mut current, 0, 9..15, "小李", "小林").unwrap();
        assert!(replace_word_text(&mut current, 0, 0..6, "周一", "周五").is_err());
        assert!(replace_word_text(&mut current, 0, 1..3, "周", "X").is_err());
        current.save().unwrap();
        let reopened = Document::open(&file).unwrap();
        assert_eq!(reopened.paragraphs()[0].text, "周三由小林主持");
        assert_eq!(reopened.paragraphs()[1].text, "其他段落");
    }
}
