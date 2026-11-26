//! Registered projects with a cancellable, isolated clone attempt and resumable status.
use super::repository::{CloneInput, Prepared};
use super::*;
use crate::workbench::Project;
use axum::{
    Router,
    extract::{Path, State},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use futures_util::stream;
use std::{collections::HashMap, convert::Infallible, sync::Mutex, time::Duration};
use tokio::sync::watch;
mod worker;
pub(in crate::workbench::forge) use worker::download;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Job {
    attempt: Uuid,
    project: Uuid,
    input: Prepared,
    #[serde(default)]
    actual_url: Option<String>,
    phase: String,
    stage: String,
    percent: Option<u8>,
    detail: Option<String>,
    stalled: bool,
    error: Option<String>,
    started: String,
    ended: Option<String>,
    sequence: u64,
}
impl Job {
    fn snapshot(&self) -> Value {
        json!({"projectId":self.project,"remoteUrl":self.actual_url.as_ref().unwrap_or(&self.input.source),"destinationPath":self.input.dest,"repository":self.input.repo,"phase":self.phase,"stage":self.stage,"percent":self.percent,"detail":self.detail,"stalled":self.stalled,"error":self.error,"startedAt":self.started,"endedAt":self.ended,"sequence":self.sequence})
    }
}
#[derive(Clone)]
struct Control {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}
pub(in crate::workbench) struct Tracker {
    tasks: Mutex<HashMap<Uuid, Control>>,
    events: watch::Sender<u64>,
}
impl Default for Tracker {
    fn default() -> Self {
        Self {
            tasks: Default::default(),
            events: watch::channel(0).0,
        }
    }
}
fn now() -> String {
    jiff::Timestamp::now().to_string()
}
fn next(db: &mut ForgeState) -> crate::Result<u64> {
    db.clone_seq = db
        .clone_seq
        .checked_add(1)
        .ok_or_else(|| Error::Protocol("clone sequence exhausted".into()))?;
    Ok(db.clone_seq)
}
fn notify(app: &Workbench) {
    app.0.clones.events.send_modify(|n| *n = n.wrapping_add(1));
}
// Invalidate the worker's publication guard in the same transaction as the project tombstone.
pub(in crate::workbench) fn mark_discarded(db: &mut ForgeState, id: Uuid) {
    if let Some(job) = db.clones.get_mut(&id) {
        job.phase = "discarding".into();
    }
}
pub(in crate::workbench) async fn finish_discard(app: &Workbench, id: Uuid) -> crate::Result<()> {
    let job = app
        .0
        .catalog
        .lock()
        .unwrap()
        .forge
        .clones
        .get(&id)
        .filter(|j| j.phase == "discarding")
        .cloned();
    let Some(job) = job else {
        return Ok(());
    };
    notify(app);
    let control = app.0.clones.tasks.lock().unwrap().get(&id).cloned();
    if let Some(mut control) = control {
        control.cancel.send_replace(true);
        let _ = control.done.wait_for(|v| *v).await;
    }
    worker::reclaim_stage(app, &job);
    app.update(|db| {
        if db
            .forge
            .clones
            .get(&id)
            .is_some_and(|j| j.phase == "discarding" && j.attempt == job.attempt)
        {
            db.forge.clones.remove(&id);
        }
        Ok(())
    })
}
pub(in crate::workbench) fn ready(app: &Workbench, id: Uuid) -> crate::Result<()> {
    if app
        .0
        .catalog
        .lock()
        .unwrap()
        .forge
        .clones
        .get(&id)
        .is_some_and(|j| j.phase != "done")
    {
        return Err(Error::Refused("project clone is incomplete".into()));
    }
    Ok(())
}
pub(in crate::workbench) fn recover(app: &Workbench) -> crate::Result<()> {
    let jobs = app.0.catalog.lock().unwrap().forge.clones.clone();
    if jobs.is_empty() {
        return Ok(());
    }
    app.update(|db| {
        for (id, prior) in &jobs {
            if prior.phase == "discarding" {
                worker::reclaim_stage(app, prior);
                db.forge.clones.remove(id);
                continue;
            }
            if prior.phase == "done" {
                db.forge.clones.remove(id);
                continue;
            }
            if prior.phase != "running" {
                continue;
            }
            worker::reclaim_stage(app, prior);
            let committed = app.allowed(&prior.input.dest).is_ok()
                && prior
                    .input
                    .dest
                    .canonicalize()
                    .is_ok_and(|p| p == prior.input.dest)
                && worker::has_receipt(prior);
            let seq = next(&mut db.forge)?;
            let job = db.forge.clones.get_mut(id).unwrap();
            job.phase = if committed { "done" } else { "failed" }.into();
            job.ended = Some(now());
            job.sequence = seq;
            job.stalled = false;
            if committed {
                job.input.credential_url = None;
                job.percent = Some(100)
            } else {
                job.error = Some(
                    "Kar stopped before this clone was published; retry the project clone".into(),
                )
            }
        }
        Ok(())
    })
}

#[derive(Deserialize)]
struct Start {
    id: Uuid,
    title: String,
    #[serde(flatten)]
    input: CloneInput,
}

type CloneReceipt<'a> = dyn Fn(&mut crate::workbench::Catalog, &Value) -> crate::Result<()> + 'a;

fn begin(
    app: &Workbench,
    id: Uuid,
    title: String,
    input: Prepared,
    retry: bool,
    record: Option<&CloneReceipt<'_>>,
) -> Result<Job, ForgeError> {
    // Another registered project may already own this empty checkout after a failed attempt.
    let id = if retry || record.is_some() {
        id
    } else {
        let db = app.0.catalog.lock().unwrap();
        db.projects
            .values()
            .find(|p| p.root == input.dest)
            .map(|p| p.id)
            .unwrap_or(id)
    };
    let mut tasks = app.0.clones.tasks.lock().unwrap();
    if tasks.contains_key(&id) {
        return Err(ForgeError::remote(409, "project clone is already running"));
    }
    let job = app.update(|db| {
        if db.lifecycle.is_deleted(id) {
            return Err(Error::Refused("project ID was deleted".into()));
        }
        if let Some(old) = db.forge.clones.get(&id) {
            if record.is_some()
                || old.phase == "running"
                || old.phase == "done"
                || old.input.dest != input.dest
            {
                return Err(Error::Busy);
            }
        } else if retry {
            return Err(Error::Refused("unknown clone retry".into()));
        }
        if db
            .forge
            .clones
            .values()
            .any(|j| j.project != id && j.input.dest == input.dest && j.phase == "running")
        {
            return Err(Error::Busy);
        }
        if db
            .projects
            .values()
            .any(|p| p.id != id && p.root == input.dest)
            || db.projects.get(&id).is_some_and(|p| p.root != input.dest)
        {
            return Err(Error::Busy);
        }
        if let Some(record) = record {
            record(db, &started(id, &input))?;
        }
        match std::fs::create_dir(&input.dest) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    && input.dest.is_dir()
                    && std::fs::read_dir(&input.dest)?.next().is_none() => {}
            Err(e) => return Err(e.into()),
        }
        let job = Job {
            attempt: Uuid::new_v4(),
            project: id,
            input: input.clone(),
            actual_url: None,
            phase: "running".into(),
            stage: "connecting".into(),
            percent: None,
            detail: None,
            stalled: false,
            error: None,
            started: now(),
            ended: None,
            sequence: next(&mut db.forge)?,
        };
        db.projects.entry(id).or_insert_with(|| Project {
            id,
            name: title,
            root: input.dest,
            metadata: Default::default(),
        });
        db.forge.clones.insert(id, job.clone());
        Ok(job)
    })?;
    let (cancel, rx) = watch::channel(false);
    let (done, finished) = watch::channel(false);
    tasks.insert(
        id,
        Control {
            cancel,
            done: finished,
        },
    );
    drop(tasks);
    notify(app);
    let app = app.clone();
    let running = job.clone();
    tokio::spawn(async move {
        let result = worker::run(&app, &running, rx).await;
        if let Err(reason) = result {
            let _ = app.update(|db| {
                let seq = next(&mut db.forge)?;
                let Some(live) = db
                    .forge
                    .clones
                    .get_mut(&id)
                    .filter(|j| j.attempt == running.attempt && j.phase == "running")
                else {
                    return Ok(());
                };
                if worker::has_receipt(live) {
                    live.phase = "done".into();
                    live.percent = Some(100);
                    live.error = None;
                    live.ended = Some(now());
                    live.sequence = seq;
                    return Ok(());
                }
                live.phase = if reason.kind == "cancelled" {
                    "cancelled"
                } else {
                    "failed"
                }
                .into();
                live.stalled = false;
                live.error = (reason.kind != "cancelled").then_some(reason.detail);
                live.ended = Some(now());
                live.sequence = seq;
                Ok(())
            });
        }
        app.0.clones.tasks.lock().unwrap().remove(&id);
        done.send_replace(true);
        notify(&app);
        tokio::time::sleep(Duration::from_secs(30)).await;
        if app
            .0
            .catalog
            .lock()
            .unwrap()
            .forge
            .clones
            .get(&id)
            .is_some_and(|j| j.attempt == running.attempt && j.phase == "done")
        {
            let _ = app.update(|db| {
                if db
                    .forge
                    .clones
                    .get(&id)
                    .is_some_and(|j| j.attempt == running.attempt && j.phase == "done")
                {
                    db.forge.clones.remove(&id);
                }
                Ok(())
            });
            notify(&app);
        }
    });
    Ok(job)
}
fn started(id: Uuid, input: &Prepared) -> Value {
    json!({"projectId":id,"cwd":input.dest,"repository":input.repo,"remoteUrl":input.source})
}

pub(in crate::workbench) async fn model_start(
    app: Workbench,
    id: Uuid,
    title: String,
    payload: Value,
    record: impl Fn(&mut crate::workbench::Catalog, &Value) -> crate::Result<()> + Send + Sync,
) -> crate::Result<Value> {
    let request = serde_json::from_value(payload)?;
    let prepared = repository::prepare(&app, request)
        .await
        .map_err(|e| Error::Refused(e.detail))?;
    let job = begin(&app, id, title, prepared, false, Some(&record))
        .map_err(|e| Error::Refused(e.detail))?;
    Ok(started(job.project, &job.input))
}
async fn start(State(app): State<Workbench>, Json(s): Json<Start>) -> Reply<Value> {
    if s.title.trim().is_empty() {
        return Err(ForgeError::bad("project title is required"));
    }
    let input = repository::prepare(&app, s.input).await?;
    let job = begin(&app, s.id, s.title, input, false, None)?;
    Ok(Json(started(job.project, &job.input)))
}
async fn cancel(State(app): State<Workbench>, Path(id): Path<Uuid>) -> Reply<Value> {
    let active = app.0.clones.tasks.lock().unwrap().get(&id).cloned();
    if let Some(mut task) = active {
        task.cancel.send_replace(true);
        let _ = task.done.wait_for(|v| *v).await;
        return Ok(Json(json!({"applied":true})));
    }
    Ok(Json(json!({"applied":false})))
}
async fn retry(State(app): State<Workbench>, Path(id): Path<Uuid>) -> Reply<Value> {
    let old = app.0.catalog.lock().unwrap().forge.clones.get(&id).cloned();
    let Some(job) = old.filter(|j| matches!(j.phase.as_str(), "cancelled" | "failed")) else {
        return Ok(Json(json!({"applied":false})));
    };
    let project = app.project(id)?;
    app.destination(&job.input.dest)?;
    match begin(&app, id, project.name, job.input, true, None) {
        Ok(_) => Ok(Json(json!({"applied":true}))),
        Err(e) if e.status == 409 => Ok(Json(json!({"applied":false}))),
        Err(e) => Err(e),
    }
}
fn snapshots(app: &Workbench) -> Vec<Value> {
    let db = app.0.catalog.lock().unwrap();
    let mut jobs: Vec<_> = db
        .forge
        .clones
        .values()
        .filter(|j| j.phase != "discarding" && app.allowed(&j.input.dest).is_ok())
        .collect();
    jobs.sort_by_key(|j| j.sequence);
    jobs.into_iter().map(Job::snapshot).collect()
}
async fn list(State(app): State<Workbench>) -> Json<Vec<Value>> {
    Json(snapshots(&app))
}
async fn subscribe(State(app): State<Workbench>) -> Response {
    let rx = app.0.clones.events.subscribe();
    let stream = stream::unfold((app, rx, true), |(app, mut rx, initial)| async move {
        if !initial {
            rx.changed().await.ok()?;
        }
        let event = Event::default()
            .event("clones")
            .json_data(snapshots(&app))
            .ok()?;
        Some((Ok::<_, Infallible>(event), (app, rx, false)))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/forge/project-clones", post(start).get(list))
        .route("/forge/project-clones/stream", get(subscribe))
        .route("/forge/project-clones/{id}/cancel", post(cancel))
        .route("/forge/project-clones/{id}/retry", post(retry))
}
