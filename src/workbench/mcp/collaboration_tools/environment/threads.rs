//! Peer sessions serve only the named chat. The local owner selects the linked environment.
use super::*;
use crate::{chat::ChatCursor, workbench::thread_find::items};
use axum::{
    body::Body,
    extract::Path,
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::{StreamExt, stream};
use std::time::Duration;

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct Window {
    before: Option<Uuid>,
    limit: Option<usize>,
    item: Option<String>,
    after: Option<ChatCursor>,
}

async fn serve(
    State(app): State<Workbench>,
    Path((p, c, op)): Path<(Uuid, Uuid, String)>,
    headers: HeaderMap,
    Query(q): Query<Window>,
) -> std::result::Result<Response, ApiError> {
    app.binding(p, c)?;
    if op == "events" {
        let cursor = match headers.get("last-event-id").and_then(|s| s.to_str().ok()) {
            Some(s) if !s.is_empty() => Some(s.parse()?),
            _ => q.after,
        };
        let store = app.0.chats.clone();
        let (initial,feed)=tokio::task::spawn_blocking(move || -> Result<_> {
            if let Some(cursor)=cursor {return Ok((None,store.follow(c,Some(cursor))?))}
            let snap=store.feed_snapshot(c)?;
            let total=snap.history.turns.len();
            let turns=&snap.history.turns[total.saturating_sub(50)..];
            let snapshot=json!({"chat":c,"turns":turns,"before":(total>50).then(||turns[0].id),"cursor":snap.cursor});
            let initial=Event::default().event("snapshot").id(snap.cursor.to_string()).data(snapshot.to_string());
            Ok((Some(initial),store.follow(c,Some(snap.cursor))?))
        }).await.map_err(|e|bad(&e.to_string()))??;
        let events = stream::try_unfold(feed, |mut feed| async move {
            let e = feed.recv().await?;
            let row = Event::default()
                .id(e.cursor.to_string())
                .event("chat")
                .data(serde_json::to_string(&e)?);
            Ok::<_, Error>(Some((row, feed)))
        });
        return Ok(
            Sse::new(stream::iter(initial.into_iter().map(Ok)).chain(events))
                .keep_alive(KeepAlive::default())
                .into_response(),
        );
    }
    let result=crate::workbench::http::blocking(move ||{
        let history=app.0.chats.history(c)?;
        match op.as_str() {
            "history"=>{
                let limit=q.limit.unwrap_or(50);
                if !(1..=100).contains(&limit){return Err(bad("history limit must be between 1 and 100"));}
                let end=match q.before{Some(id)=>history.turns.iter().position(|t|t.id==id).ok_or(Error::InvalidCursor)?,None=>history.turns.len()};
                let start=end.saturating_sub(limit);
                Ok(json!({"turns":&history.turns[start..end],"before":(start>0).then(|| history.turns[start].id)}))
            },
            "item"=>{
                let id=q.item.ok_or_else(||bad("missing item identity"))?;
                let item=history.turns.iter().flat_map(items).find(|i|i.entry_id==id);
                Ok(json!({"item":item}))
            },
            _=>Err(bad("unknown remote thread operation"))
        }
    }).await?;
    Ok(([(axum::http::header::CACHE_CONTROL, "no-store")], result).into_response())
}

async fn relay(
    State(app): State<Workbench>,
    Path((peer, p, c, op)): Path<(Uuid, Uuid, Uuid, String)>,
    headers: HeaderMap,
    Query(q): Query<Window>,
) -> std::result::Result<Response, ApiError> {
    if !matches!(op.as_str(), "history" | "events" | "item") {
        return Err(bad("unknown remote thread operation").into());
    }
    let proof = app
        .0
        .catalog
        .lock()
        .unwrap()
        .environment
        .links
        .get(&peer)
        .map(|l| l.request_id)
        .ok_or_else(|| bad("unknown environment link"))?;
    let mut request = peer_request(
        &app,
        peer,
        reqwest::Method::GET,
        &["peer", "threads", &p.to_string(), &c.to_string(), &op],
    )?
    .query(&q);
    if let Some(cursor) = headers.get("last-event-id") {
        request = request.header("last-event-id", cursor);
    }
    let remote = request
        .send()
        .await
        .map_err(|_| bad("linked environment did not answer"))?;
    let status = remote.status();
    if !status.is_success() {
        return Ok((status, "linked environment refused the chat request").into_response());
    }
    let mime = if op == "events" {
        "text/event-stream"
    } else {
        "application/json"
    };
    if !remote
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.split(';').next() == Some(mime))
    {
        return Err(bad("unexpected linked environment response").into());
    }
    let revoked = async move {
        loop {
            if !app
                .0
                .catalog
                .lock()
                .unwrap()
                .environment
                .links
                .get(&peer)
                .is_some_and(|l| l.request_id == proof)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    Ok((
        StatusCode::OK,
        [("content-type", mime), ("cache-control", "no-store")],
        Body::from_stream(remote.bytes_stream().take_until(revoked)),
    )
        .into_response())
}
pub(super) fn remote_routes() -> Router<Workbench> {
    Router::new().route("/peer/threads/{project}/{chat}/{operation}", get(serve))
}
pub(super) fn local_routes() -> Router<Workbench> {
    Router::new().route(
        "/environment/links/{id}/projects/{project}/chats/{chat}/{operation}",
        get(relay),
    )
}
