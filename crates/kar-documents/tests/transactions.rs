use anyhow::Result;
use docs_engine::{Document, Selection};
use kar_documents::{
    OfficeEdits,
    store::{Action, Command, Decision, DocumentStore, Rejection, Status},
    word::WordEdit,
};
use std::fs;
use tempfile::tempdir;

fn word(paragraph: usize, old: &str, value: &str) -> OfficeEdits {
    OfficeEdits::Word(vec![WordEdit {
        paragraph,
        range: 0..old.len(),
        replacement: value.into(),
    }])
}

fn edit(id: &str, rev: u64, p: usize, old: &str, new: &str) -> Command {
    Command {
        author: "人".into(),
        operation_id: id.into(),
        base_revision: rev,
        action: Action::Edit {
            edits: word(p, old, new),
        },
    }
}

fn resolution(id: &str, rev: u64, cursor: u64, decision: Decision) -> Command {
    Command {
        author: "作者".into(),
        operation_id: id.into(),
        base_revision: rev,
        action: Action::Resolve {
            conflict: cursor,
            decision,
        },
    }
}

#[test]
fn revision_conflict_and_resolution_receipts_survive_reopen() -> Result<()> {
    for (decision, expected) in [
        (Decision::AdoptProposed, "五"),
        (Decision::KeepCurrent, "三"),
        (
            Decision::Manual {
                edits: word(0, "三", "四"),
            },
            "四",
        ),
    ] {
        let dir = tempdir()?;
        let file = dir.path().join("minutes.docx");
        let store = dir.path().join("state");
        let mut doc = Document::new();
        doc.insert_text(Selection::default(), "一\n甲", None);
        doc.save_as(&file)?;
        let mut server = DocumentStore::open(&store, &file)?;
        let first = edit("one", 0, 0, "一", "三");
        let mut next = edit("two", 0, 0, "一", "五");
        next.author = "AI".into();
        let accepted = server.submit(first.clone())?;
        assert_eq!(accepted.revision, 1);
        let bytes = fs::read(&file)?;
        let conflict = server.submit(next.clone())?;
        assert_eq!(conflict.status, Status::Conflict);
        assert_eq!(conflict.revision, 1);
        assert_eq!(
            conflict.conflict.as_ref().unwrap().current,
            Some(serde_json::json!("三"))
        );
        assert_eq!(fs::read(&file)?, bytes);
        drop(server);
        let mut server = DocumentStore::open(&store, &file)?;
        assert_eq!(server.submit(first.clone())?.cursor, accepted.cursor);
        assert_eq!(server.submit(next.clone())?.cursor, conflict.cursor);
        let mut other = first;
        other.author = "different".into();
        assert!(matches!(
            server
                .submit(other)
                .unwrap_err()
                .downcast_ref::<Rejection>(),
            Some(Rejection::ReusedId)
        ));
        assert_eq!(server.state()?.pending_conflicts, vec![conflict.cursor]);
        server.submit(edit("three", 1, 1, "甲", "乙"))?;
        let stale = resolution("resolve", 1, conflict.cursor, decision.clone());
        assert!(matches!(
            server
                .submit(stale)
                .unwrap_err()
                .downcast_ref::<Rejection>(),
            Some(Rejection::StaleResolution { current: 2, .. })
        ));
        let resolve = resolution("resolve", 2, conflict.cursor, decision);
        let receipt = server.submit(resolve.clone())?;
        assert_eq!(receipt.resolves, Some(conflict.cursor));
        assert_eq!(server.events(2)?.len(), 2);
        drop(server);
        let mut server = DocumentStore::open(&store, &file)?;
        assert!(server.state()?.pending_conflicts.is_empty());
        assert_eq!(server.submit(resolve)?.cursor, receipt.cursor);
        let latest = Document::open(&file)?;
        assert_eq!(latest.paragraphs()[0].text, expected);
        assert_eq!(latest.paragraphs()[1].text, "乙");
        assert_eq!(server.snapshot(1)?, bytes);
        assert_eq!(fs::read(&file)?, server.snapshot(receipt.revision)?);
    }
    Ok(())
}

#[test]
fn external_save_is_observed_before_rebasing_a_stale_proposal() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("notes.docx");
    let mut doc = Document::new();
    doc.insert_text(Selection::default(), "甲\n乙", None);
    doc.save_as(&file)?;
    let mut store = DocumentStore::open(&dir.path().join("store"), &file)?;
    doc.insert_text(Selection::default(), "外部\n", None);
    doc.save()?;
    let receipt = store.submit(edit("from-base", 0, 1, "乙", "丙"))?;
    assert_eq!(receipt.revision, 2);
    assert_eq!(store.events(0)?[0].receipt.status, Status::External);
    assert_eq!(
        Document::open(&file)?
            .paragraphs()
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        ["外部", "甲", "丙"]
    );
    Ok(())
}

#[test]
fn interrupted_publication_recovers_on_either_side_of_the_file_rename() -> Result<()> {
    for window in ["before rename", "after rename", "external save"] {
        let dir = tempdir()?;
        let file = dir.path().join("minutes.docx");
        let state = dir.path().join("store");
        let mut doc = Document::new();
        doc.insert_text(Selection::default(), "初", None);
        doc.save_as(&file)?;
        let old_bytes = fs::read(&file)?;
        let mut store = DocumentStore::open(&state, &file)?;
        let manifest = state.join("manifest.json");
        let mut before: serde_json::Value = serde_json::from_slice(&fs::read(&manifest)?)?;
        let command = edit("publish", 0, 0, "初", "定");
        let receipt = store.submit(command.clone())?;
        let new_bytes = fs::read(&file)?;
        let after: serde_json::Value = serde_json::from_slice(&fs::read(&manifest)?)?;
        drop(store);
        // Reconstruct the exact durable intent using its real snapshot and
        // command from the successful normal-entry-point transaction.
        before["pending"] = serde_json::json!({"candidate":after["revisions"][1]["digest"],"record":after["records"][0]});
        fs::write(manifest, serde_json::to_vec(&before)?)?;
        if window == "before rename" {
            fs::write(&file, &old_bytes)?;
        }
        if window == "external save" {
            let mut external = Document::new();
            external.insert_text(Selection::default(), "外部", None);
            external.save_as(&file)?;
        }
        let mut store = DocumentStore::open(&state, &file)?;
        let recovered = store.submit(command)?;
        if window == "external save" {
            assert_eq!(recovered.status, Status::Conflict);
            assert_eq!(Document::open(&file)?.paragraphs()[0].text, "外部");
            assert_eq!(store.events(0)?.len(), 2);
        } else {
            assert_eq!(receipt.cursor, recovered.cursor);
            assert_eq!(store.state()?.revision, 1);
            assert_eq!(store.events(0)?.len(), 1);
            assert_eq!(fs::read(&file)?, new_bytes);
        }
    }
    Ok(())
}

#[test]
fn capability_backend_uses_identity_only_for_metadata_and_format() -> Result<()> {
    use kar_documents::store::WorkingFile;
    use std::sync::{Arc, Mutex};
    struct Handle(Arc<Mutex<Vec<u8>>>);
    impl WorkingFile for Handle {
        fn read(&self) -> Result<Vec<u8>> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn publish(&self, bytes: &[u8]) -> Result<()> {
            *self.0.lock().unwrap() = bytes.to_vec();
            Ok(())
        }
    }
    let dir = tempdir()?;
    let identity = dir.path().join("metadata-only.docx");
    let mut doc = Document::new();
    doc.insert_text(Selection::default(), "一", None);
    let bytes = Arc::new(Mutex::new(doc.to_docx_bytes()?));
    let mut store = DocumentStore::open_with_file(
        &dir.path().join("state"),
        &identity,
        Box::new(Handle(bytes.clone())),
    )?;
    let receipt = store.submit(edit("change", 0, 0, "一", "三"))?;
    assert_eq!(receipt.revision, 1);
    assert_eq!(store.snapshot(1)?, *bytes.lock().unwrap());
    assert!(!identity.try_exists()?);
    Ok(())
}
