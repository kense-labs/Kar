//! Bounded replay for authorized preview observers. Cursors identify the current backend lifetime.
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};
use tokio::sync::watch;
use uuid::Uuid;

struct Event {
    project: Uuid,
    owner: Option<Uuid>,
    seq: u64,
    data: Value,
}
pub(super) struct Events {
    rows: Mutex<(u64, VecDeque<Event>)>,
    pub changed: watch::Sender<u64>,
}
impl Default for Events {
    fn default() -> Self {
        Self {
            rows: Mutex::new((0, VecDeque::new())),
            changed: watch::channel(0).0,
        }
    }
}
impl Events {
    pub fn push(&self, project: Uuid, owner: Option<Uuid>, data: Value) {
        let mut rows = self.rows.lock().unwrap();
        rows.0 += 1;
        let seq = rows.0;
        rows.1.push_back(Event {
            project,
            owner,
            seq,
            data,
        });
        if rows.1.len() > 1024 {
            rows.1.pop_front();
        }
        self.changed.send_replace(seq);
    }
    pub fn page(&self, project: Uuid, owner: Option<Uuid>, after: u64) -> (u64, Vec<(u64, Value)>) {
        let rows = self.rows.lock().unwrap();
        let mut out = vec![];
        if after > rows.0
            || rows
                .1
                .front()
                .is_some_and(|row| after.saturating_add(1) < row.seq)
        {
            out.push((rows.0, json!({"type":"resync"})));
        } else {
            for row in rows.1.iter().filter(|r| {
                r.seq > after && r.project == project && owner.is_none_or(|o| r.owner == Some(o))
            }) {
                out.push((row.seq, row.data.clone()));
            }
        }
        (rows.0, out)
    }
}
