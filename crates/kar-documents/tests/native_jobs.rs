#![cfg(unix)]
use anyhow::{Result, bail};
use kar_documents::{
    native::{NativeBegin, NativeJob, NativeRequest},
    store::{Action, Command, DocumentStore, Rejection, Status},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
use tempfile::tempdir;

fn request(id: &str) -> NativeRequest {
    NativeRequest {
        author: "AI".into(),
        operation_id: id.into(),
        base_revision: 0,
        runtime: "codex".into(),
        model: None,
        instructions: "Change the assignee.".into(),
    }
}
fn ready(store: &mut DocumentStore, req: NativeRequest) -> Result<NativeJob> {
    match store.begin_native(req)? {
        NativeBegin::Ready(job) => Ok(job),
        _ => bail!("expected new native edit"),
    }
}
#[test]
fn native_copy_uses_base_revision_and_idempotently_submits_after_human_save() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("confidential-internal-filename.md");
    let bytes = b"date: monday\nowner: alice\n";
    fs::write(&file, bytes)?;
    let mut store = DocumentStore::open(&dir.path().join("registry"), &file)?;
    let req = request("native-1");
    let mut job = ready(&mut store, req.clone())?;
    assert!(!job.prompt().contains("confidential"));
    assert!(!job.prompt().contains(dir.path().to_str().unwrap()));
    assert_eq!(job.filename(), "document.md");
    assert_eq!(fs::metadata(job.cwd())?.permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::read_dir(job.cwd())?.count(), 1);
    assert_eq!(fs::read(job.cwd().join(job.filename()))?, bytes);
    assert!(matches!(
        store.begin_native(req.clone())?,
        NativeBegin::Running
    ));
    let sha256 = store.stage(b"other save")?;
    let reused = store
        .submit(Command {
            author: "AI".into(),
            operation_id: req.operation_id.clone(),
            base_revision: 0,
            action: Action::Save { sha256 },
        })
        .unwrap_err();
    assert!(matches!(
        reused.downcast_ref::<Rejection>(),
        Some(Rejection::ReusedId)
    ));
    let human = store.stage(b"date: friday\nowner: alice\n")?;
    store.submit(Command {
        author: "human".into(),
        operation_id: "human".into(),
        base_revision: 0,
        action: Action::Save { sha256: human },
    })?;
    job.mark_dispatched(Some("session-a".into()))?;
    fs::write(
        job.cwd().join(job.filename()),
        b"date: monday\nowner: bob\n",
    )?;
    let scratch = job.cwd().to_owned();
    let receipt = job.finish(&mut store)?;
    assert_eq!(receipt.revision, 2);
    assert_eq!(receipt.status, Status::Applied);
    assert_eq!(fs::read(&file)?, b"date: friday\nowner: bob\n");
    assert!(!scratch.try_exists()?);
    let NativeBegin::Finished(retry) = store.begin_native(req.clone())? else {
        bail!("missing prior receipt")
    };
    assert_eq!(retry.cursor, receipt.cursor);
    let mut other = req;
    other.instructions = "different".into();
    assert!(store.begin_native(other).is_err());
    Ok(())
}

#[test]
fn interrupted_native_turn_is_never_automatically_redispatched() -> Result<()> {
    let dir = tempdir()?;
    let state = dir.path().join("state");
    let file = dir.path().join("note.txt");
    fs::write(&file, "base")?;
    let mut store = DocumentStore::open(&state, &file)?;
    let req = request("recover");
    let mut job = ready(&mut store, req.clone())?;
    let input = job.input_id();
    job.mark_dispatched(Some("session-1".into()))?;
    let copy = job.cwd().join(job.filename());
    fs::write(&copy, "complete")?;
    drop(job);
    drop(store);
    let mut reopened = DocumentStore::open(&state, &file)?;
    assert!(
        matches!(reopened.begin_native(req.clone())?,NativeBegin::Interrupted{native_session_id:Some(id)} if id=="session-1")
    );
    assert!(reopened.recover_native(&req, "other-session").is_err());
    let mut recovered = reopened.recover_native(&req, "session-1")?;
    assert_eq!(recovered.input_id(), input);
    assert_eq!(input, req.input_id(&reopened.state()?.document));
    assert!(
        recovered
            .mark_dispatched(Some("second attempt".into()))
            .is_err()
    );
    // This simulated host has the positive native terminal receipt.
    let done = recovered.finish(&mut reopened)?;
    assert_eq!(done.revision, 1);
    assert_eq!(fs::read(file)?, b"complete");
    Ok(())
}

#[test]
fn failed_or_linked_native_output_does_not_publish_a_working_file() -> Result<()> {
    let dir = tempdir()?;
    let state = dir.path().join("state");
    let file = dir.path().join("draft.txt");
    fs::write(&file, "base")?;
    let mut store = DocumentStore::open(&state, &file)?;
    let req = request("bad-output");
    let mut job = ready(&mut store, req.clone())?;
    job.mark_dispatched(Some("session".into()))?;
    let output = job.cwd().join(job.filename());
    fs::remove_file(&output)?;
    symlink(&file, &output)?;
    assert!(job.finish(&mut store).is_err());
    assert_eq!(store.state()?.revision, 0);
    let recovered = store.recover_native(&req, "session")?;
    recovered.fail("native output unavailable".into())?;
    assert!(
        matches!(store.begin_native(req)?,NativeBegin::Failed{reason} if reason=="native output unavailable")
    );
    assert_eq!(fs::read(file)?, b"base");
    Ok(())
}
