//! Monitor workflow questions and runs without attaching a provider or dispatching work.
use super::super::http::{ApiError, blocking};
use super::{Data, Schedule, Status, Step, Workbench, Workflow, missing, same_project};
use crate::Error;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use uuid::Uuid;

fn ended(s: &Status) -> bool {
    matches!(s, Status::Failed | Status::Completed | Status::Cancelled)
}
fn counts(db: &Data, p: Uuid) -> Value {
    let (mut total, mut active, mut needs_you) = (0, 0, 0);
    for run in db.runs.values().filter(|r| r.project == p) {
        total += 1;
        if !ended(&run.status) {
            active += 1;
        }
        if matches!(run.status, Status::Asking | Status::Reconcile) {
            needs_you += 1;
        }
    }
    json!({"total":total,"active":active,"needs_you":needs_you})
}
fn snapshot(db: &Data, p: Uuid, only_schedule: bool) -> Value {
    let definitions: Vec<&Workflow> = db.workflows.values().filter(|w| w.project == p).collect();
    if only_schedule {
        return json!({"revision":db.revision,"workflows":definitions});
    }
    let mut runs: Vec<_> = db.runs.values().filter(|r| r.project == p).collect();
    runs.sort_by_key(|r| (std::cmp::Reverse(r.started_ms), r.id));
    let questions:Vec<_>=runs.iter().filter(|r|r.status==Status::Asking).filter_map(|r|{
        let Step::Ask{question}=r.steps.get(r.cursor)? else{return None;};
        Some(json!({"run":r.id,"workflow":r.workflow,"attempt":r.attempt.as_ref()?.id,"question":question}))
    }).collect();
    let summaries: Vec<_> = runs
        .iter()
        .map(|r| {
            json!({"id":r.id,"workflow":r.workflow,"started_ms":r.started_ms,
            "status":r.status,"step":r.cursor,"cancel_requested":r.cancel_requested})
        })
        .collect();
    json!({"revision":db.revision,"workflows":definitions,"runs":summaries,"questions":questions,"counts":counts(db,p)})
}
impl Workbench {
    pub(in crate::workbench) fn workflow_index(&self, project: Uuid) -> crate::Result<Value> {
        self.project(project)?;
        Ok(snapshot(&self.0.workflows.db.lock().unwrap(), project, false))
    }
}
type Reply = std::result::Result<Json<Value>, ApiError>;
async fn count(State(app): State<Workbench>, Path(p): Path<Uuid>) -> Reply {
    blocking(move || {
        app.project(p)?;
        Ok(counts(&app.0.workflows.db.lock().unwrap(), p))
    })
    .await
}
async fn read(State(app): State<Workbench>, Path(p): Path<Uuid>) -> Reply {
    blocking(move || {
        app.project(p)?;
        Ok(snapshot(&app.0.workflows.db.lock().unwrap(), p, false))
    })
    .await
}
#[derive(Deserialize)]
struct Views {
    #[serde(default)]
    schedules_only: bool,
}
async fn events(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    Query(q): Query<Views>,
) -> std::result::Result<Response, ApiError> {
    app.project(p)?;
    let rx = app.0.workflows.changed.subscribe();
    let stream =
        futures_util::stream::unfold((app, rx, None), move |(app, mut rx, last)| async move {
            loop {
                if app.project(p).is_err() {
                    return None;
                }
                let event = {
                    let db = app.0.workflows.db.lock().unwrap();
                    if Some(db.revision) == last {
                        None
                    } else {
                        Some((
                            db.revision,
                            Event::default()
                                .id(db.revision.to_string())
                                .event("snapshot")
                                .json_data(snapshot(&db, p, q.schedules_only))
                                .ok()?,
                        ))
                    }
                };
                if let Some((revision, frame)) = event {
                    return Some((Ok::<_, Infallible>(frame), (app, rx, Some(revision))));
                }
                rx.changed().await.ok()?;
            }
        });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
#[derive(Deserialize)]
struct Enable {
    enabled: bool,
}
async fn enable(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Json(q): Json<Enable>,
) -> Reply {
    blocking(move || {
        app.project(p)?;
        let engine = &app.0.workflows;
        engine.update(|db| {
            super::sources::allow_direct(db, id)?;
            let w = db.workflows.get_mut(&id).ok_or_else(missing)?;
            same_project(p, w.project)?;
            if q.enabled != w.definition.enabled {
                let next = w.definition.schedule.next_after(engine.clock.now_ms())?;
                if q.enabled
                    && matches!(w.definition.schedule, Schedule::Once { .. })
                    && next.is_none()
                {
                    return Err(Error::Refused(
                        "one-time schedule must be in the future".into(),
                    ));
                }
                w.definition.enabled = q.enabled;
                w.next_ms = if q.enabled { next } else { None };
                if !q.enabled {
                    cancel_deferred(&mut db.runs, id);
                }
            }
            Ok(json!(w))
        })
    })
    .await
}
fn cancel_deferred(runs: &mut std::collections::BTreeMap<Uuid, super::Run>, workflow: Uuid) {
    for r in runs.values_mut().filter(|r| {
        r.workflow == workflow && r.status == Status::Ready && r.webhook_due_ms.is_some()
    }) {
        r.status = Status::Cancelled;
        r.cancel_requested = true;
        r.webhook_due_ms = None;
        r.webhook_templates = None;
    }
}
pub(super) fn erase_workflow(db: &mut Data, id: Uuid) {
    db.workflows.remove(&id);
    db.mailbox.remove(id);
    db.secrets.remove(&id);
    db.webhook_tokens.remove(&id);
    db.webhook_rules.remove(&id);
    db.webhook_windows.remove(&id);
    db.deliveries.remove(&id);
    cancel_deferred(&mut db.runs, id);
}

pub(super) async fn remove(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Reply {
    blocking(move || {
        app.project(p)?;
        app.0.workflows.update(|db| {
            let w = db.workflows.get(&id).ok_or_else(missing)?;
            same_project(p, w.project)?;
            super::sources::allow_direct(db, id)?;
            erase_workflow(db, id);
            Ok(json!({"id":id}))
        })
    })
    .await
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{p}/workflow-state", get(read))
        .route("/projects/{p}/workflow-state/events", get(events))
        .route("/projects/{p}/workflow-state/counts", get(count))
        .route("/projects/{p}/workflows/{id}/enabled", put(enable))
}
