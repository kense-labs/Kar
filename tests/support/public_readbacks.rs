//! Fixed public case session.query-readbacks-carry-stream-seq.
use kar::{Outcome, Protocol, Session};
use uuid::Uuid;

#[path = "public_stream.rs"]
pub(crate) mod stream;

pub(crate) async fn check(session: &Session) -> kar::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(45), check_turns(session))
        .await
        .expect("two public readback turns timed out")
}
async fn check_turns(session: &Session) -> kar::Result<()> {
    let explicit = Uuid::new_v4();
    let start = session.records().len();
    let supplied = session.prompt_with_id(explicit, "hello").await?;
    assert_eq!(supplied.input_id(), explicit);
    assert_eq!(supplied.wait().await?, Outcome::Completed);
    let ordinary = session.prompt("without inputId").await?;
    let generated = ordinary.input_id();
    assert_ne!(explicit, generated);
    assert_eq!(generated.get_version_num(), 4);
    assert_eq!(ordinary.wait().await?, Outcome::Completed);
    let records = session.records();
    let ids: Vec<_> = records[start..]
        .iter()
        .filter_map(|r| {
            let prompt = r.prompt.as_ref()?;
            assert_eq!(r.direction, kar::Direction::ToRuntime);
            assert_eq!(r.kind, kar::RecordKind::Request);
            assert_eq!(r.session_id.as_deref(), Some(session.id()));
            assert_eq!(prompt.native_admission, session.protocol() == Protocol::Pi);
            if session.protocol() == Protocol::Qwen {
                assert_eq!(r.native["uuid"].as_str(), Some(prompt.input_id.as_str()));
            }
            Some(prompt.input_id.as_str())
        })
        .collect();
    assert_eq!(ids, [explicit.to_string(), generated.to_string()]);
    let model = session.model_readback();
    let usage = session.usage_readback();
    let context = session.context_usage_readback();
    // Native notifications may arrive between queries. Replay each exact prefix,
    // so even a read concurrent with a new frame is checked against the correct state.
    let recorded = session.records();
    let mut reducer = kar::observe::SessionReducer::new(session.id());
    let mut matched = [false; 3];
    for record in recorded {
        reducer.apply(&record)?;
        let state = reducer.snapshot();
        if model.seq == Some(record.seq) {
            assert_eq!(state.model, model.value);
            matched[0] = true;
        }
        if usage.seq == Some(record.seq) {
            assert_eq!(state.usage, usage.value);
            matched[1] = true;
        }
        if context.seq == Some(record.seq) {
            assert_eq!(state.context_usage, context.value);
            matched[2] = true;
        }
    }
    assert_eq!(matched, [true; 3]);
    for seq in [model.seq, usage.seq, context.seq] {
        assert!(seq.unwrap() >= records.last().unwrap().seq);
    }
    Ok(())
}
