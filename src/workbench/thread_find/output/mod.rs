//! A viewer starts from a durable tool item and follows the same exclusive cursor as chat history.
mod terminal;
use super::{Workbench, items};
use crate::{
    Error, Result,
    chat::{ChatFeed, ChatReplica, ChatTurn, Item, TurnStatus},
    observe::{Tool, ToolActionKind},
};
use axum::{
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::Serialize;
use std::{convert::Infallible, time::Duration};
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Serialize)]
struct Frame {
    kind: &'static str,
    text: String,
    truncated: bool,
    running: bool,
}
struct Viewer {
    feed: ChatFeed,
    replica: ChatReplica,
    turn: Uuid,
    entry: String,
    previous: Option<(String, bool)>,
    interval: Duration,
    closed: bool,
}
fn row<'a>(turn: &'a ChatTurn, entry: &str) -> Option<&'a Tool> {
    let item = items(turn).find(|row| row.entry_id == entry)?.item;
    match &item.item {
        Item::Tool { activity, .. } => Some(activity),
        _ => None,
    }
}
impl Viewer {
    fn open(app: &Workbench, project: Uuid, chat: Uuid, entry: String) -> Result<Self> {
        app.binding(project, chat)?;
        let snap = app.0.chats.feed_snapshot(chat)?;
        let (turn, tool) = snap
            .history
            .turns
            .iter()
            .find_map(|t| row(t, &entry).map(|tool| (t, tool)))
            .ok_or_else(super::super::missing)?;
        let native = turn
            .items
            .iter()
            .find(|i| i.source.path.is_empty())
            .and_then(|i| i.source.session_id.as_ref());
        let rt = snap
            .history
            .handoffs
            .iter()
            .flat_map(|h| std::iter::once(&h.from).chain(h.target.iter()))
            .find(|provider| Some(&provider.native_id) == native)
            .map(|p| &p.runtime)
            .unwrap_or(&snap.history.chat.runtime);
        if ![tool.name.as_deref(), tool.native_type.as_deref()]
            .into_iter()
            .flatten()
            .any(|name| ToolActionKind::classify(rt, name) == ToolActionKind::RunCommand)
        {
            return Err(Error::Refused("timeline item is not a command".into()));
        }
        let turn = turn.id;
        let feed = app.0.chats.follow(chat, Some(snap.cursor))?;
        Ok(Self {
            feed,
            replica: ChatReplica::from_snapshot(snap),
            turn,
            entry,
            previous: None,
            closed: false,
            interval: Duration::from_millis(100),
        })
    }
    fn project(&mut self) -> Result<Option<Frame>> {
        let turn = self
            .replica
            .turns()
            .iter()
            .find(|t| t.id == self.turn)
            .ok_or_else(super::super::missing)?;
        let tool = row(turn, &self.entry).ok_or_else(super::super::missing)?;
        let running = !tool.finished && turn.status == TurnStatus::Running;
        let final_text = tool
            .result
            .as_ref()
            .and_then(|r| r.content.as_ref().map(|_| r.text().unwrap_or_default()));
        let (text, cut) = terminal::normalize(final_text.as_deref().unwrap_or(&tool.preview));
        let truncated = tool.elided || cut;
        let mut frame = Frame {
            kind: "replace",
            text: String::new(),
            truncated,
            running,
        };
        self.interval = Duration::from_millis(100);
        if let Some((last, was_cut)) = &self.previous
            && running
        {
            if &text == last && truncated == *was_cut {
                return Ok(None);
            }
            if truncated == *was_cut
                && let Some(delta) = text.strip_prefix(last)
            {
                if delta.encode_utf16().count() <= 16 * 1024 {
                    frame.kind = "append";
                    frame.text = delta.to_owned();
                } else {
                    self.interval = Duration::from_millis(500);
                }
            }
        }
        if frame.kind == "replace" {
            frame.text.clone_from(&text);
        }
        self.closed = !running;
        self.previous = Some((text, truncated));
        Ok(Some(frame))
    }
    async fn next(&mut self) -> Result<Option<Frame>> {
        if self.closed {
            return Ok(None);
        }
        if self.previous.is_none() {
            return self.project();
        }
        loop {
            let first = self.feed.recv().await?;
            self.replica.apply(first)?;
            let due = Instant::now() + self.interval;
            while Instant::now() < due {
                match tokio::time::timeout_at(due, self.feed.recv()).await {
                    Ok(result) => {
                        self.replica.apply(result?)?;
                    }
                    Err(_) => break,
                }
            }
            if let Some(frame) = self.project()? {
                return Ok(Some(frame));
            }
        }
    }
}
pub(super) async fn events(
    State(app): State<Workbench>,
    Path((project, chat, entry)): Path<(Uuid, Uuid, String)>,
) -> std::result::Result<Response, super::super::http::ApiError> {
    let viewer = tokio::task::spawn_blocking(move || Viewer::open(&app, project, chat, entry))
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
    let stream = futures_util::stream::unfold(viewer, |mut viewer| async move {
        let event = match viewer.next().await {
            Ok(Some(frame)) => Event::default()
                .event("output")
                .id(viewer.replica.cursor().to_string())
                .json_data(frame)
                .ok()?,
            Ok(None) => return None,
            Err(e) => {
                viewer.closed = true;
                Event::default()
                    .event("error")
                    .json_data(serde_json::json!({"error":e.to_string()}))
                    .ok()?
            }
        };
        Some((Ok::<_, Infallible>(event), viewer))
    });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
