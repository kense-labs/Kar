use super::*;
use crate::workbench::http::ApiError;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
type Reply = std::result::Result<Json<Value>, ApiError>;
type Ids = (Uuid, Uuid, Uuid);
pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{p}/prepared-runs", get(list))
        .route(
            "/projects/{p}/chats/{c}/prepared-runs/{r}",
            get(status).put(prepare),
        )
        .route(
            "/projects/{p}/chats/{c}/prepared-runs/{r}/events",
            get(events),
        )
        .route(
            "/projects/{p}/chats/{c}/prepared-runs/{r}/retry",
            post(retry),
        )
        .route(
            "/projects/{p}/chats/{c}/prepared-runs/{r}/release",
            post(release),
        )
}
async fn list(State(app): State<Workbench>, Path(p): Path<Uuid>) -> Reply {
    app.project(p)?;
    let db = app.0.catalog.lock().unwrap();
    Ok(Json(
        json!({"runs":db.prepared_runs.values().filter(|j|j.project==p).map(Job::view).collect::<Vec<_>>()}),
    ))
}
async fn status(State(app): State<Workbench>, Path((p, c, r)): Path<Ids>) -> Reply {
    Ok(Json(get_job(&app, p, c, r)?.view()))
}
use super::get as get_job;
async fn prepare(
    State(app): State<Workbench>,
    Path((p, c, r)): Path<Ids>,
    Json(q): Json<Input>,
) -> Reply {
    let gate = app.controller(c);
    let _owner = gate.owner.lock().await;
    Ok(Json(submit(&app, p, c, r, q, None)?.view()))
}
#[derive(Deserialize)]
struct Action {
    command_id: Uuid,
}
fn act(app: &Workbench, (p, c, r): Ids, cmd: Uuid, retry: bool) -> Result<Value> {
    let row = get_job(app, p, c, r)?;
    let action = if retry { "retry" } else { "release" };
    if !row.commands.contains_key(&cmd)
        && retry
        && let Some(s) = row.script.as_ref()
        && row.stage == Stage::Failed
        && app.setup_still_running(p, s.terminal)?
    {
        return Err(Error::Busy);
    }
    let job = change(app, r, |j| {
        if let Some(previous) = j.commands.get(&cmd) {
            if previous != action {
                return Err(Error::Refused(
                    "command ID used by another preparation action".into(),
                ));
            }
            return Ok(j.clone());
        }
        if retry {
            if j.stage != Stage::Failed {
                return Err(Error::Busy);
            }
            let failed_at = j.events.iter().rev().nth(1).map(|e| e.stage);
            j.attempt = j
                .attempt
                .checked_add(1)
                .ok_or_else(|| Error::Protocol("too many setup attempts".into()))?;
            let next = if failed_at == Some(Stage::Releasing) {
                Stage::Releasing
            } else {
                Stage::Worktree
            };
            j.advance(next, Some("retry".into()));
        } else {
            match j.stage {
                Stage::Ready => j.advance(Stage::Releasing, None),
                Stage::Releasing | Stage::Released => {}
                _ => return Err(Error::Busy),
            }
        }
        j.commands.insert(cmd, action.into());
        Ok(j.clone())
    })?;
    worker::schedule(app, &job);
    Ok(job.view())
}
async fn retry(State(app): State<Workbench>, Path(ids): Path<Ids>, Json(q): Json<Action>) -> Reply {
    Ok(Json(act(&app, ids, q.command_id, true)?))
}
async fn release(
    State(app): State<Workbench>,
    Path(ids): Path<Ids>,
    Json(q): Json<Action>,
) -> Reply {
    Ok(Json(act(&app, ids, q.command_id, false)?))
}
#[derive(Deserialize)]
struct Cursor {
    after: Option<u64>,
}
async fn events(
    State(app): State<Workbench>,
    Path((p, c, r)): Path<Ids>,
    Query(q): Query<Cursor>,
    h: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let rx = app.0.run_events.subscribe();
    let h = h
        .get("Last-Event-ID")
        .map(|s| {
            s.to_str()
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(Error::InvalidCursor)
        })
        .transpose()?;
    let after = h.or(q.after).unwrap_or(0);
    if after > get_job(&app, p, c, r)?.events.len() as u64 {
        return Err(Error::InvalidCursor.into());
    }
    let stream = futures_util::stream::unfold(
        (app, rx, after),
        move |(app, mut rx, mut after)| async move {
            loop {
                let row = get_job(&app, p, c, r).ok()?;
                if let Some(e) = row.events.get(after as usize) {
                    after = e.sequence;
                    let event = SseEvent::default()
                        .id(after.to_string())
                        .event("preparation")
                        .json_data(e)
                        .ok()?;
                    return Some((Ok::<_, std::convert::Infallible>(event), (app, rx, after)));
                }
                if row.stage == Stage::Released {
                    return None;
                }
                rx.changed().await.ok()?;
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
