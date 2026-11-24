//! The workbench directory stream carries project and chat summaries. A reconnect can
//! resume the same stream after a process restart without reopening any model session.
mod log;
mod scan;
use super::{Workbench, http::ApiError};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::HeaderMap,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    path::Path,
    sync::{Arc, Mutex, Once},
    time::Duration,
};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Project,
    Chat,
}
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
struct Key {
    kind: Kind,
    id: Uuid,
}
#[derive(Clone, Serialize, Deserialize)]
struct Change {
    #[serde(flatten)]
    key: Key,
    row: Option<Value>,
}
pub(super) struct Service {
    journal: Mutex<log::Journal>,
    sources: Mutex<scan::Sources>,
    updates: watch::Sender<u64>,
    wake: Arc<Notify>,
    started: Once,
}
impl Service {
    pub fn open(state: &Path, roots: &[super::Root]) -> Result<Self> {
        let mut dirs: Vec<_> = roots.iter().map(|r| &r.path).collect();
        dirs.sort();
        dirs.dedup();
        let mut scope = Sha256::new();
        for dir in dirs {
            let bytes = dir.as_os_str().as_encoded_bytes();
            scope.update((bytes.len() as u64).to_be_bytes());
            scope.update(bytes);
        }
        let journal = log::Journal::open(state, &format!("{:x}", scope.finalize()))?;
        Ok(Self {
            updates: watch::channel(journal.sequence()).0,
            journal: Mutex::new(journal),
            sources: Default::default(),
            wake: Default::default(),
            started: Once::new(),
        })
    }
    fn refresh(&self, app: &Workbench) -> Result<()> {
        let mut sources = self.sources.lock().unwrap();
        let rows = sources.scan(app)?;
        let mut journal = self.journal.lock().unwrap();
        if journal.commit(rows)? {
            self.updates.send_replace(journal.sequence());
        }
        Ok(())
    }
    pub fn changed(&self) {
        self.wake.notify_one();
    }
    pub fn start(&self, app: &Workbench) {
        self.started.call_once(|| {
            let weak = Arc::downgrade(&app.0);
            let wake = self.wake.clone();
            tokio::spawn(async move {
                loop {
                    let Some(inner) = weak.upgrade() else { return };
                    let delay = if inner.shell.updates.receiver_count() > 0 {
                        250
                    } else {
                        1_500
                    };
                    let app = Workbench(inner);
                    match tokio::task::spawn_blocking(move || app.0.shell.refresh(&app)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => eprintln!("Kar shell directory: {e}"),
                        Err(e) => eprintln!("Kar shell worker: {e}"),
                    }
                    tokio::select! {
                        _ = wake.notified() => {},
                        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    }
                }
            });
        });
    }
}
pub(super) fn snapshot(app: &Workbench) -> Result<(Uuid, u64, Value)> {
    app.0.shell.refresh(app)?;
    let journal = app.0.shell.journal.lock().unwrap();
    let rows = journal
        .rows
        .iter()
        .map(|(key, v)| json!({"kind":key.kind,"id":key.id,"row":v}))
        .collect::<Vec<_>>();
    let stream = journal.stream;
    let sequence = journal.sequence();
    Ok((
        stream,
        sequence,
        json!({"kind":"snapshot","cursor":format!("{stream}:{sequence}"),"rows":rows}),
    ))
}
async fn get_snapshot(State(app): State<Workbench>) -> std::result::Result<Json<Value>, ApiError> {
    super::http::blocking(move || snapshot(&app).map(|(_, _, value)| value)).await
}
#[derive(Deserialize)]
struct Resume {
    after: Option<String>,
}
async fn events(
    State(app): State<Workbench>,
    Query(q): Query<Resume>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let rx = app.0.shell.updates.subscribe();
    let after = headers
        .get("last-event-id")
        .map(|v| {
            v.to_str()
                .map(str::to_owned)
                .map_err(|_| Error::InvalidCursor)
        })
        .transpose()?
        .or(q.after);
    let state = app.clone();
    let (stream, after, initial) = tokio::task::spawn_blocking(move || -> Result<_> {
        if let Some(cursor) = after {
            state.0.shell.refresh(&state)?;
            let log = state.0.shell.journal.lock().unwrap();
            let (id, n) = cursor.split_once(':').ok_or(Error::InvalidCursor)?;
            let n: u64 = n.parse().map_err(|_| Error::InvalidCursor)?;
            if id != log.stream.to_string() || n > log.sequence() {
                return Err(Error::InvalidCursor);
            }
            Ok((log.stream, n, None))
        } else {
            let (id, n, snap) = snapshot(&state)?;
            Ok((id, n, Some(snap)))
        }
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    let frames = futures_util::stream::unfold(
        (
            app,
            rx,
            after,
            initial,
            VecDeque::<log::Batch>::new(),
            false,
        ),
        move |(app, mut rx, mut after, mut initial, mut pending, closed)| async move {
            if closed {
                return None;
            }
            loop {
                let (frame, failed) = if let Some(first) = initial.take() {
                    (first, false)
                } else if let Some(batch) = pending.pop_front() {
                    after = batch.sequence;
                    (
                        json!({"kind":"changes","cursor":format!("{stream}:{after}"),"changes":batch.changes}),
                        false,
                    )
                } else {
                    let state = app.clone();
                    match tokio::task::spawn_blocking(move || {
                        state.0.shell.journal.lock().unwrap().page(after)
                    })
                    .await
                    {
                        Ok(Ok(page)) => {
                            pending = page;
                            if pending.is_empty() {
                                rx.changed().await.ok()?;
                            }
                            continue;
                        }
                        Ok(Err(e)) => (json!({"kind":"error","error":e.to_string()}), true),
                        Err(e) => (json!({"kind":"error","error":e.to_string()}), true),
                    }
                };
                let mut evt = Event::default().event("shell");
                if !failed {
                    evt = evt.id(format!("{stream}:{after}"));
                }
                return Some((
                    Ok::<_, Infallible>(evt.json_data(frame).ok()?),
                    (app, rx, after, initial, pending, failed),
                ));
            }
        },
    );
    Ok(Sse::new(frames)
        .keep_alive(KeepAlive::default())
        .into_response())
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/shell", get(get_snapshot))
        .route("/shell/events", get(events))
}
