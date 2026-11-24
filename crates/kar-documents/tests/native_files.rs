use anyhow::Result;
use docs_engine::{Document, Selection};
use kar_documents::{
    Change, DocumentEdits, GridAction, GridChange,
    excel::{Cell, CellEdit},
    prepare,
    store::{Action, Command, Decision, DocumentStore, Status},
    text::TextEdit,
    word::WordEdit,
};
use sheets_engine::Workbook;
use std::fs;
use tempfile::tempdir;

fn command(id: &str, rev: u64, action: Action) -> Command {
    Command {
        author: "editor".into(),
        operation_id: id.into(),
        base_revision: rev,
        action,
    }
}
#[test]
fn offline_markdown_proposal_merges_with_external_crlf_document_and_retries() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("brief.md");
    let original = "\u{feff}# 标题\r\n\r\n共用段落\r\n上线日是周一，负责人是小李。\r\n共用段落\r\n";
    fs::write(&path, original)?;
    let mut store = DocumentStore::open(&dir.path().join("state"), &path)?;
    let offline = original.replace("小李", "小林");
    let sha256 = store.stage(offline.as_bytes())?;
    let human = original
        .replace("共用段落\r\n上线", "文首插段\r\n共用段落\r\n上线")
        .replace("周一", "下周三");
    fs::write(&path, &human)?;
    let request = command("ai", 0, Action::Save { sha256 });
    let done = store.submit(request.clone())?;
    assert_eq!(done.status, Status::Applied);
    assert_eq!(fs::read_to_string(&path)?, human.replace("小李", "小林"));
    assert_eq!(store.submit(request)?.cursor, done.cursor);
    let alt = original.replace("小李", "小王");
    let sha256 = store.stage(alt.as_bytes())?;
    let receipt = store.submit(command(
        "other",
        0,
        Action::Save {
            sha256: sha256.clone(),
        },
    ))?;
    assert_eq!(receipt.status, Status::Conflict);
    assert_eq!(store.proposal(&sha256)?, alt.as_bytes());
    let prior = fs::read(&path)?;
    let old = std::str::from_utf8(&prior)?;
    let at = old.find("小林").unwrap();
    let manual = DocumentEdits::Text(vec![TextEdit {
        range: at..at + "小林".len(),
        replacement: "小唐".into(),
    }]);
    let resolved = store.submit(command(
        "pick",
        done.revision,
        Action::Resolve {
            conflict: receipt.cursor,
            decision: Decision::Manual { edits: manual },
        },
    ))?;
    assert_eq!(resolved.resolves, Some(receipt.cursor));
    assert_eq!(fs::read_to_string(&path)?, old.replace("小林", "小唐"));
    Ok(())
}
#[test]
fn word_native_proposal_merges_reproducible_changes_and_retains_rejected_bytes() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("draft.docx");
    let mut doc = Document::new();
    doc.insert_text(Selection::default(), "甲乙\n丁戊", None);
    doc.save_as(&path)?;
    let base = fs::read(&path)?;
    let mut store = DocumentStore::open(&dir.path().join("state"), &path)?;
    let edit = |p, value: &str| {
        DocumentEdits::Word(vec![WordEdit {
            paragraph: p,
            range: 0..3,
            replacement: value.into(),
        }])
    };
    let offline = prepare(&base, &base, &edit(1, "六"))?.unwrap();
    let sha256 = store.stage(&offline)?;
    store.submit(command(
        "human",
        0,
        Action::Edit {
            edits: edit(0, "五"),
        },
    ))?;
    let done = store.submit(command("ai", 0, Action::Save { sha256 }))?;
    assert_eq!(done.status, Status::Applied);
    let reread = Document::open(&path)?;
    assert_eq!(reread.paragraphs()[0].text, "五乙");
    assert_eq!(reread.paragraphs()[1].text, "六戊");
    let latest = fs::read(&path)?;
    let bad = b"damaged export";
    let sha256 = store.stage(bad)?;
    let receipt = store.submit(command(
        "broken",
        done.revision,
        Action::Save {
            sha256: sha256.clone(),
        },
    ))?;
    assert_eq!(receipt.status, Status::Conflict);
    assert_eq!(receipt.revision, done.revision);
    assert_eq!(fs::read(&path)?, latest);
    assert_eq!(store.proposal(&sha256)?, bad);
    Ok(())
}

#[test]
fn spreadsheet_service_carries_committed_grid_history_into_stale_native_formula() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("budget.xlsx");
    let mut book = Workbook::new_empty()?;
    book.set_input(0, 2, 1, "10")?;
    book.set_input(0, 2, 3, "=B3*2")?;
    book.save_as(&path)?;
    let b = Cell::at(&book, 0, 2, 1)?;
    let d = Cell::at(&book, 0, 2, 3)?;
    let formulas = DocumentEdits::Excel(vec![(
        d,
        CellEdit::Input(Change {
            base: "=B3*2".into(),
            proposed: "=B3*3".into(),
        }),
    )]);
    let base = fs::read(&path)?;
    let proposal = prepare(&base, &base, &formulas)?.unwrap();
    let mut store = DocumentStore::open(&dir.path().join("state"), &path)?;
    let sha256 = store.stage(&proposal)?;
    store.submit(command(
        "new row",
        0,
        Action::Grid {
            change: GridChange {
                sheet_id: b.sheet_id,
                action: GridAction::InsertRows,
                at: 0,
                count: 1,
            },
        },
    ))?;
    let value = CellEdit::Input(Change {
        base: "10".into(),
        proposed: "12".into(),
    });
    store.submit(command(
        "human",
        0,
        Action::Edit {
            edits: DocumentEdits::Excel(vec![(b, value)]),
        },
    ))?;
    let receipt = store.submit(command("ai file", 0, Action::Save { sha256 }))?;
    assert_eq!(receipt.status, Status::Applied);
    let verified = Workbook::open(&path)?;
    assert_eq!(verified.input(0, 3, 3), "=B4*3");
    assert_eq!(verified.display_value(0, 3, 3), "36");
    Ok(())
}
