//! Milestones are written inside the authoritative chat journal row.
use super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub(super) struct Before {
    turn: Option<Uuid>,
    requests: BTreeMap<String, (String, String)>,
}
fn requests(state: &State) -> BTreeMap<String, (String, String)> {
    state
        .pending_requests
        .iter()
        .filter(|r| r.agent_path.is_empty())
        .filter_map(|r| {
            let kind = match r.kind {
                crate::RequestKind::Question => "user_input",
                crate::RequestKind::Permission => "approval",
                _ => return None,
            };
            Some((
                serde_json::to_string(&r.cursor).ok()?,
                (
                    r.id.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| r.id.to_string()),
                    kind.into(),
                ),
            ))
        })
        .collect()
}
impl Before {
    pub(super) fn capture(state: &State) -> Self {
        Self {
            turn: state.active(),
            requests: requests(state),
        }
    }
    fn after(self, state: &State, resolution: &'static str) -> Vec<Value> {
        let mut data = vec![];
        if let Some(turn) = self.turn
            && let Some(t) = state.find(turn)
            && t.status != TurnStatus::Running
        {
            let outcome = match t.status {
                TurnStatus::Completed => "completed",
                TurnStatus::Failed => "failed",
                TurnStatus::Interrupted => "interrupted",
                TurnStatus::Aborted => "cancelled",
                TurnStatus::Running => unreachable!(),
            };
            data.push(json!({"type":"thread.run_settled","runId":turn,"outcome":outcome}));
        }
        let current = requests(state);
        for (key, (id, kind)) in &self.requests {
            if !current.contains_key(key) {
                data.push(json!({"type":"thread.input_resolved","requestId":id,"kind":kind,"resolution":resolution}));
            }
        }
        for (key, (id, kind)) in current {
            if !self.requests.contains_key(&key) {
                data.push(json!({"type":"thread.needs_input","requestId":id,"kind":kind}));
            }
        }
        data
    }
}

fn encoded_transition(state: &mut State, row: ChatUpdate, expired: bool) -> Result<Vec<u8>> {
    let before = Before::capture(state);
    let resolution = if matches!(
        &row,
        ChatUpdate::End {
            status: TurnStatus::Aborted,
            ..
        }
    ) {
        "cancelled"
    } else if expired
        || matches!(
            row,
            ChatUpdate::Opened { .. }
                | ChatUpdate::ContextRewound { .. }
                | ChatUpdate::HandoffReady { .. }
                | ChatUpdate::RequestDeliveryFailed { .. }
        )
        || matches!(&row, ChatUpdate::End { status, .. } if *status != TurnStatus::Completed)
    {
        "expired"
    } else {
        "resolved"
    };
    let mut bytes = serde_json::to_vec(&row)?;
    state.apply(row)?;
    let events = before.after(state, resolution);
    if !events.is_empty() {
        bytes.pop();
        bytes.extend_from_slice(b",\"_kar_milestones\":");
        serde_json::to_writer(
            &mut bytes,
            &json!({"at":jiff::Timestamp::now().as_millisecond(),"events":events}),
        )?;
        bytes.push(b'}');
    }
    bytes.push(b'\n');
    Ok(bytes)
}
fn write(file: &mut File, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)?;
    file.sync_data()?;
    Ok(())
}
pub(super) fn commit(
    file: &mut File,
    state: &mut State,
    row: ChatUpdate,
    expired: bool,
) -> Result<()> {
    let bytes = encoded_transition(state, row, expired)?;
    with_lock(file, true, |f| write(f, &bytes))
}
pub(in crate::chat) fn orphan(file: &mut File, state: &mut State, row: ChatUpdate) -> Result<()> {
    write(file, &encoded_transition(state, row, true)?)
}
