mod files;
mod html;
mod output;
mod render;
use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{
    Error, Result,
    chat::{ChatCursor, ChatFeedSnapshot, ChatTurn, Item, Role, TimelineItem},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, convert::Infallible};
use uuid::Uuid;

#[derive(Deserialize)]
struct Start {
    entry_id: String,
    occurrence: usize,
}
#[derive(Deserialize)]
struct Skill {
    name: String,
    display_name: Option<String>,
}
#[derive(Deserialize)]
struct Find {
    query: String,
    index: Option<usize>,
    #[serde(default)]
    offset: i64,
    start: Option<Start>,
    #[serde(default)]
    skills: Vec<Skill>,
    #[serde(default)]
    theme: Option<String>,
}
impl Find {
    fn prepare(&mut self) -> Result<()> {
        let query = self.query.trim();
        if query.is_empty() || query.encode_utf16().count() > 200 {
            return Err(Error::Refused(
                "query must contain 1 to 200 characters".into(),
            ));
        }
        if self.skills.len() > 1000
            || self.skills.iter().any(|s| {
                s.name.encode_utf16().count() > 200
                    || s.display_name
                        .as_ref()
                        .is_some_and(|n| n.encode_utf16().count() > 200)
            })
        {
            return Err(Error::Refused("invalid search skill metadata".into()));
        }
        self.query = render::fold(query);
        Ok(())
    }
}

#[derive(Serialize)]
pub(super) struct Row<'a> {
    pub(super) entry_id: String,
    pub(super) turn: Uuid,
    pub(super) item: &'a TimelineItem,
}
pub(super) fn items(turn: &ChatTurn) -> impl Iterator<Item = Row<'_>> {
    let mut occurrences = HashMap::new();
    turn.items.iter().map(move |item| {
        let (kind, native) = match &item.item {
            Item::Message {
                role, provider_id, ..
            } => (
                match role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Reasoning => "reasoning",
                },
                provider_id.as_deref(),
            ),
            Item::Tool { id, .. } => ("tool", Some(id.as_str())),
            Item::Diff { turn_id, .. } => ("diff", Some(turn_id.as_str())),
            Item::Retry { .. } => ("retry", None),
            Item::Compaction { .. } => ("compaction", None),
            Item::RequestError { error } => ("request_error", error.request_id.as_str()),
        };
        // Independent native agents may reuse the same item ID, even in a single turn.
        let identity = serde_json::to_vec(&(turn.id, &item.source, kind, native))
            .expect("serializable item identity");
        let count = occurrences.entry(identity.clone()).or_insert(0u64);
        let mut digest = Sha256::new();
        digest.update(identity);
        digest.update(count.to_be_bytes());
        *count += 1;
        Row {
            entry_id: format!("{:x}", digest.finalize()),
            turn: turn.id,
            item,
        }
    })
}

fn has_html_render_json(v: &Value) -> bool {
    v.get("htmlRender").is_some_and(|h| !h.is_null())
        || v.pointer("/structuredContent/htmlRender")
            .is_some_and(|h| !h.is_null())
}

fn has_html_render_part(part: &crate::OutputPart) -> bool {
    match part {
        crate::OutputPart::Text { text } => {
            text.contains("htmlRender")
                && serde_json::from_str::<Value>(text).is_ok_and(|v| has_html_render_json(&v))
        }
        crate::OutputPart::Other { value } => has_html_render_json(value),
        crate::OutputPart::Image { .. } => false,
    }
}

fn turn_suppresses_empty_response(turn: &ChatTurn) -> bool {
    turn.items.iter().any(|ti| match &ti.item {
        Item::Tool { activity, .. } => {
            let succeeded = activity.finished
                && activity
                    .result
                    .as_ref()
                    .and_then(|r| r.status.as_ref())
                    .is_none_or(|s| *s == crate::ToolStatus::Succeeded);
            if !succeeded {
                return false;
            }
            activity.mcp_app_uri.is_some()
                || activity
                    .result
                    .as_ref()
                    .and_then(|r| r.content.as_ref())
                    .is_some_and(|parts| parts.iter().any(has_html_render_part))
        }
        _ => false,
    })
}

#[derive(Clone, Serialize)]
struct Matches {
    entry_id: String,
    turn: Uuid,
    start_index: usize,
    count: usize,
}

fn result(
    cursor: ChatCursor,
    rows: &[Matches],
    q: &Find,
    resolved_start: Option<usize>,
) -> Value {
    let total = rows.last().map_or(0, |r| r.start_index + r.count);
    let mut index = 0;
    if total > 0 {
        index = if let Some(i) = q.index {
            i.min(total - 1)
        } else {
            let base = resolved_start.map_or(0, |n| n % total);
            ((base as i128 + i128::from(q.offset)).rem_euclid(total as i128)) as usize
        };
    }
    let pos = rows.partition_point(|r| r.start_index + r.count <= index);
    let selected = rows
        .get(pos)
        .map(|r| json!({"entry_id":r.entry_id,"turn":r.turn,"occurrence":index-r.start_index}));
    let navigation: Vec<_> = if rows.len() <= 17 {
        rows.iter().collect()
    } else {
        (0..17)
            .map(|i| &rows[(rows.len() + pos + i - 8) % rows.len()])
            .collect()
    };
    json!({"cursor":cursor,"complete":true,"total_matches":total,"active_index":index,"match":selected,"navigation":navigation})
}

#[derive(Clone, Copy)]
enum DocKind<'a> {
    Message {
        user: bool,
        text: &'a str,
        complete: bool,
        suppress_empty: bool,
    },
    RequestError {
        reason: &'a str,
    },
    Other,
}

struct Doc<'a> {
    entry_id: String,
    turn: Uuid,
    kind: DocKind<'a>,
}

fn scan(
    snapshot: &ChatFeedSnapshot,
    q: &Find,
    early: Option<&tokio::sync::mpsc::Sender<Result<Value>>>,
) -> Result<Value> {
    let cwd = (!snapshot.history.chat.workspace.as_os_str().is_empty())
        .then_some(snapshot.history.chat.workspace.as_path());
    let theme = q.theme.as_deref();
    let mut docs = Vec::new();
    for turn in &snapshot.history.turns {
        let suppress_empty = turn_suppresses_empty_response(turn);
        for row in items(turn) {
            let kind = match &row.item.item {
                Item::Message {
                    role: role @ (Role::User | Role::Assistant),
                    text,
                    complete,
                    ..
                } => DocKind::Message {
                    user: *role == Role::User,
                    text,
                    complete: *complete,
                    suppress_empty,
                },
                Item::RequestError { error } if !error.reason.is_empty() => DocKind::RequestError {
                    reason: &error.reason,
                },
                _ => DocKind::Other,
            };
            docs.push(Doc {
                entry_id: row.entry_id,
                turn: row.turn,
                kind,
            });
        }
    }
    let eval = |doc: &Doc<'_>| match doc.kind {
        DocKind::Message {
            user,
            text,
            complete,
            suppress_empty,
        } => render::occurrences(
            text,
            user,
            complete,
            suppress_empty,
            &q.query,
            &q.skills,
            cwd,
            theme,
        ),
        DocKind::RequestError { reason } => render::plain_occurrences(reason, &q.query),
        DocKind::Other => 0,
    };
    let mut counts = vec![None; docs.len()];
    if let Some(tx) = early.filter(|_| q.index.is_none() && q.offset == 0) {
        let start = q
            .start
            .as_ref()
            .and_then(|s| docs.iter().position(|d| d.entry_id == s.entry_id))
            .unwrap_or(0);
        for offset in 0..docs.len().saturating_add(1) {
            if docs.is_empty() {
                break;
            }
            let i = (start + offset) % docs.len();
            let doc = &docs[i];
            let count = *counts[i].get_or_insert_with(|| eval(doc));
            let occurrence = if offset == 0 {
                q.start
                    .as_ref()
                    .filter(|s| s.entry_id == doc.entry_id)
                    .map_or(0, |s| s.occurrence)
            } else {
                0
            };
            if occurrence < count {
                tx.blocking_send(Ok(json!({"cursor":snapshot.cursor,"complete":false,"total_matches":1,"active_index":0,"match":{"entry_id":doc.entry_id,"turn":doc.turn,"occurrence":occurrence}}))).map_err(|_|Error::Closed("search client disconnected".into()))?;
                break;
            }
        }
    }
    let mut rows = Vec::new();
    let mut start_index = 0;
    let mut resolved_start = None;
    for (doc, cached) in docs.into_iter().zip(counts) {
        if early.is_some_and(|tx| tx.is_closed()) {
            return Err(Error::Closed("search client disconnected".into()));
        }
        let count = cached.unwrap_or_else(|| eval(&doc));
        if resolved_start.is_none()
            && let Some(s) = q.start.as_ref().filter(|s| s.entry_id == doc.entry_id)
        {
            resolved_start = Some(start_index + s.occurrence.min(count));
        }
        if count > 0 {
            rows.push(Matches {
                entry_id: doc.entry_id,
                turn: doc.turn,
                start_index,
                count,
            });
            start_index += count;
        }
    }
    Ok(result(snapshot.cursor, &rows, q, resolved_start))
}

async fn find(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
    Json(mut q): Json<Find>,
) -> std::result::Result<Json<Value>, ApiError> {
    q.prepare()?;
    blocking(move || {
        app.binding(project, chat)?;
        scan(&app.0.chats.feed_snapshot(chat)?, &q, None)
    })
    .await
}
async fn stream(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
    Json(mut q): Json<Find>,
) -> std::result::Result<Response, ApiError> {
    q.prepare()?;
    let Json(snapshot) = blocking(move || {
        app.binding(project, chat)?;
        app.0.chats.feed_snapshot(chat)
    })
    .await?;
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tokio::task::spawn_blocking(move || {
        let final_result = scan(&snapshot, &q, Some(&tx));
        let _ = tx.blocking_send(final_result);
    });
    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        let frame = match rx.recv().await? {
            Ok(data) => Event::default().event("find").json_data(data).ok()?,
            Err(e) => Event::default().event("error").data(e.to_string()),
        };
        Some((Ok::<_, Infallible>(frame), rx))
    });
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}
async fn turn_items(
    State(app): State<Workbench>,
    Path((p, c, t)): Path<(Uuid, Uuid, Uuid)>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        let h = app.0.chats.history(c)?;
        let turn = h
            .turns
            .iter()
            .find(|turn| turn.id == t)
            .ok_or_else(super::missing)?;
        Ok(json!(items(turn).collect::<Vec<_>>()))
    })
    .await
}
async fn detail(
    State(app): State<Workbench>,
    Path((p, c, id)): Path<(Uuid, Uuid, String)>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || {
        app.binding(p, c)?;
        let h = app.0.chats.history(c)?;
        Ok(json!({"item":h.turns.iter().flat_map(items).find(|r|r.entry_id==id)}))
    })
    .await
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{p}/chats/{c}/find", post(find))
        .route("/projects/{p}/chats/{c}/find/events", post(stream))
        .route(
            "/projects/{p}/chats/{c}/turns/{turn}/items",
            get(turn_items),
        )
        .route("/projects/{p}/chats/{c}/items/{item}", get(detail))
        .route(
            "/projects/{p}/chats/{c}/items/{item}/output",
            get(output::events),
        )
}
