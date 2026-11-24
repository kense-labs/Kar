//! Recover against the exact native thread requested by this controller.
use super::{ChatTurn, Direction, Frame};
use crate::{Record, Session};
use serde_json::json;
use std::sync::Arc;

fn response<'a>(records: &'a [Arc<Record>], method: &str, root: &str) -> Option<&'a Record> {
    let request = records.iter().rev().find(|r| {
        r.direction == Direction::ToRuntime
            && r.native["method"] == method
            && r.native["params"]["threadId"] == root
    })?;
    let id = request.native.get("id")?;
    records
        .iter()
        .rev()
        .find(|r| {
            r.seq > request.seq
                && r.direction == Direction::FromRuntime
                && r.native.get("id") == Some(id)
                && r.native
                    .pointer("/result/thread/id")
                    .and_then(|v| v.as_str())
                    == Some(root)
        })
        .map(Arc::as_ref)
}

pub(super) async fn recover(session: &Session, local: &ChatTurn) -> Option<Frame> {
    let records = session.records();
    let root = session.id();
    if let Some(frame) = response(&records, "thread/resume", root)
        .and_then(|r| Frame::codex_recovery(local, root, r))
    {
        return Some(frame);
    }
    // A resume reply may omit some or all items. Ask the same thread for its
    // full history once. An absent or ambiguous native witness stays unconfirmed.
    session
        .rpc(
            "thread/read",
            json!({"threadId": root, "includeTurns": true}),
        )
        .await
        .ok()?;
    Frame::codex_recovery(
        local,
        root,
        response(&session.records(), "thread/read", root)?,
    )
}
