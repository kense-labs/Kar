//! Recover against the exact native thread requested by this controller.
use super::{ChatTurn, Direction, Frame};
use crate::{
    Record, Session,
    chat::{TurnStatus, timeline::CodexHistory},
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

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
                && r.native.get("method").is_none()
                && r.native.get("id") == Some(id)
                && r.native
                    .pointer("/result/thread/id")
                    .and_then(|v| v.as_str())
                    == Some(root)
        })
        .map(Arc::as_ref)
}

pub(super) async fn recover(session: &Session, turns: &[ChatTurn]) -> Vec<(Uuid, Frame)> {
    let root = session.id();
    let belongs = |t: &ChatTurn| {
        t.items.first().is_some_and(|item| {
            item.source.path.is_empty() && item.source.session_id.as_deref() == Some(root)
        })
    };
    let mut pending: Vec<_> = turns
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            matches!(t.status, TurnStatus::Running | TurnStatus::Interrupted) && belongs(t)
        })
        .collect();
    if pending.is_empty() {
        return Vec::new();
    }
    let mut claims = HashMap::new();
    for turn in turns.iter().filter(|t| belongs(t)) {
        if let Some(id) = &turn.native_turn_id {
            claims
                .entry(id.clone())
                .and_modify(|owner| *owner = None)
                .or_insert(Some(turn.id));
        }
    }
    let mut frames = Vec::new();
    for method in ["thread/resume", "thread/read"] {
        if pending.is_empty() {
            break;
        }
        if method == "thread/read"
            && session
                .rpc(method, json!({"threadId":root,"includeTurns":true}))
                .await
                .is_err()
        {
            break;
        }
        let records = session.records();
        let Some(snapshot) =
            response(&records, method, root).and_then(|r| CodexHistory::new(root, r))
        else {
            continue;
        };
        pending.retain(|(position, turn)| {
            let Some(frame) = snapshot.frame(turn) else {
                return true;
            };
            let Some(native) = frame.native_turn.as_deref() else {
                return true;
            };
            if claims.entry(native.to_owned()).or_insert(Some(turn.id)) == &Some(turn.id) {
                frames.push((*position, turn.id, frame));
            }
            false
        });
    }
    // Several turns can be restored by one response, even if only some needed a second read.
    frames.sort_by_key(|(pos, _, _)| *pos);
    frames
        .into_iter()
        .map(|(_, id, frame)| (id, frame))
        .collect()
}
