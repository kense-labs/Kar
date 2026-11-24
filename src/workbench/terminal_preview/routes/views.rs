use super::super::{
    Terminal,
    viewers::{Lease, Viewer},
};
use super::*;
use axum::http::HeaderMap;
use axum::response::{
    IntoResponse,
    sse::{Event, KeepAlive, Sse},
};
use serde_json::{Value, json};
use std::{collections::VecDeque, convert::Infallible, sync::Arc};

pub(super) fn viewer(device: Device, id: Option<Uuid>) -> Option<Viewer> {
    id.map(|id| Viewer {
        device: device.map(|d| d.id),
        id,
    })
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Position {
    after: Option<u64>,
    max_history_bytes: Option<u64>,
}
#[derive(Deserialize)]
pub(super) struct Attach {
    viewer_id: Option<Uuid>,
    cols: Option<u16>,
    rows: Option<u16>,
    worktree: Option<std::path::PathBuf>,
    #[serde(flatten)]
    position: Position,
}
type StreamReply = std::result::Result<Response, ApiError>;

pub(super) async fn attach(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
    headers: HeaderMap,
    Json(q): Json<Attach>,
) -> StreamReply {
    let grid = match (q.cols, q.rows) {
        (Some(c), Some(r)) => {
            super::super::size(c, r)?;
            Some((c, r))
        }
        (None, None) => None,
        _ => return Err(Error::Refused("terminal grid needs both cols and rows".into()).into()),
    };
    let key = Viewer {
        device: device.map(|d| d.id),
        id: q.viewer_id.unwrap_or_else(Uuid::new_v4),
    };
    let (terminal, lease) = tokio::task::spawn_blocking(move || -> Result<_> {
        app.project(p)?;
        let state = &app.0.terminal_preview;
        if q.worktree.is_some() || state.find(p, id).is_err() {
            let (cols, rows) = grid.unwrap_or((120, 30));
            app.new_terminal(p, id, q.worktree, cols, rows)?;
        }
        let term = state.find(p, id)?;
        let lease = term.attach(key, grid)?;
        Ok((term, lease))
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    output_stream(terminal, Some(lease), Some(key.id), q.position, &headers)
}
pub(super) async fn observe(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Query(position): Query<Position>,
) -> StreamReply {
    app.project(p)?;
    output_stream(
        app.0.terminal_preview.find(p, id)?,
        None,
        None,
        position,
        &headers,
    )
}
struct Output {
    term: Arc<Terminal>,
    _lease: Option<Lease>,
    viewer: Option<Uuid>,
    changes: tokio::sync::watch::Receiver<u64>,
    after: u64,
    last: Option<Meta>,
    finished: bool,
}

fn output_stream(
    term: Arc<Terminal>,
    lease: Option<Lease>,
    viewer: Option<Uuid>,
    pos: Position,
    headers: &HeaderMap,
) -> StreamReply {
    let changes = term.changed.subscribe();
    let after = pos
        .after
        .or_else(|| headers.get("last-event-id")?.to_str().ok()?.parse().ok());
    let (after, end) = {
        let h = term.history.lock().unwrap();
        (
            after.unwrap_or_else(|| {
                pos.max_history_bytes
                    .map_or(0, |cap| h.end.saturating_sub(cap))
            }),
            h.end,
        )
    };
    if after > end {
        return Err(Error::InvalidCursor.into());
    }
    let initial = Output {
        term,
        _lease: lease,
        viewer,
        changes,
        after,
        last: None,
        finished: false,
    };
    let body = futures_util::stream::unfold(initial, |mut s| async move {
        while !s.finished {
            // Subscribe before reading. A concurrent write is either in this page or wakes the next one.
            s.changes.borrow_and_update();
            let t = s.term.clone();
            let after = s.after;
            let read = tokio::task::spawn_blocking(move || t.page(after)).await;
            let page = match read {
                Ok(Ok(page)) => page,
                _ => {
                    s.finished = true;
                    let event = Event::default()
                        .event("terminal")
                        .json_data(
                            json!({"type":"error","message":"unable to read terminal history"}),
                        )
                        .ok()?;
                    return Some((Ok::<_, Infallible>(event), s));
                }
            };
            let first = s.last.is_none();
            let changed = s.last.as_ref() != Some(&page.terminal);
            let output = page.next != after;
            if first || changed || output {
                let kind = if first || page.missed {
                    "snapshot"
                } else if !page.events.is_empty() {
                    "cleared"
                } else if output {
                    "output"
                } else if page.terminal.closed {
                    "closed"
                } else if matches!(page.terminal.status, Status::Exited { .. }) {
                    "exited"
                } else {
                    "activity"
                };
                s.finished = page.terminal.closed && page.next >= page.end;
                s.after = page.next;
                s.last = Some(page.terminal.clone());
                let e = Event::default()
                    .event("terminal")
                    .id(s.after.to_string())
                    .json_data(json!({"type":kind,"viewer_id":s.viewer,"page":page}))
                    .ok()?;
                return Some((Ok::<_, Infallible>(e), s));
            }
            if s.changes.changed().await.is_err() {
                break;
            }
        }
        None
    });
    Ok(Sse::new(body)
        .keep_alive(KeepAlive::default())
        .into_response())
}

#[derive(Deserialize)]
pub(super) struct MetaCursor {
    after: Option<String>,
}
pub(super) async fn directory(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    headers: HeaderMap,
    Query(q): Query<MetaCursor>,
) -> StreamReply {
    app.project(p)?;
    let dir = app.0.terminal_preview.directory.clone();
    let rx = dir.changes.changed.subscribe();
    let cursor = q
        .after
        .as_deref()
        .or_else(|| headers.get("last-event-id")?.to_str().ok())
        .and_then(|s| s.split_once('.'))
        .and_then(|(epoch, n)| {
            (epoch.parse().ok() == Some(dir.epoch))
                .then(|| n.parse::<u64>().ok())
                .flatten()
        });
    let mut pending = VecDeque::<(u64, Value)>::new();
    let after = cursor.unwrap_or_else(|| {
        let (end, snapshot) = dir.snapshot(p);
        pending.push_back((end, snapshot));
        end
    });
    let stream = futures_util::stream::unfold(
        (dir, rx, after, pending),
        move |(dir, mut rx, mut after, mut pending)| async move {
            loop {
                if let Some((n, item)) = pending.pop_front() {
                    let e = Event::default()
                        .event("terminal-metadata")
                        .id(format!("{}.{n}", dir.epoch))
                        .json_data(item)
                        .ok()?;
                    return Some((Ok::<_, Infallible>(e), (dir, rx, after, pending)));
                }
                let (next, rows) = dir.changes.page(p, None, after);
                after = next;
                if rows.iter().any(|(_, r)| r["type"] == "resync") {
                    let (end, snapshot) = dir.snapshot(p);
                    after = end;
                    pending.push_back((end, snapshot));
                } else {
                    pending.extend(rows);
                }
                if pending.is_empty() && rx.changed().await.is_err() {
                    return None;
                }
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
