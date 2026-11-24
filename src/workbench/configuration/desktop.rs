//! Owner-only control channel to the desktop updater. The desktop host owns installation and restart.
use super::*;
use axum::{extract::Path, routing::post};
use std::{path::PathBuf, sync::Mutex, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Preparing,
    Checking,
    Downloading,
    Ready,
    Current,
    Committing,
    Installing,
    Installed,
    Failed,
    Unknown,
}

impl Stage {
    fn busy(self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::Checking
                | Self::Downloading
                | Self::Ready
                | Self::Committing
                | Self::Installing
                | Self::Unknown
        )
    }
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Ready | Self::Current | Self::Installed | Self::Failed
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(in crate::workbench) struct Job {
    request_id: Uuid,
    track: Option<String>,
    stage: Stage,
    #[serde(default)]
    commit_started: bool,
    version: Option<String>,
    message: Option<String>,
    revision: u64,
}

#[derive(Default)]
pub(in crate::workbench) struct Service {
    running: Mutex<BTreeMap<Uuid, (Uuid, tokio_util::sync::CancellationToken)>>,
    changed: watch::Sender<u64>,
}
fn socket() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let path = PathBuf::from(std::env::var_os("KAR_DESKTOP_UPDATE_SOCKET")?);
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if path.is_absolute()
            && meta.file_type().is_socket()
            && meta.uid() == nix::unistd::geteuid().as_raw()
        {
            return Some(path);
        }
    }
    None
}
pub(super) fn available() -> bool {
    socket().is_some()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    request_id: Uuid,
    track: Option<String>,
}
#[derive(Deserialize)]
struct Report {
    request_id: Uuid,
    stage: Stage,
    version: Option<String>,
    message: Option<String>,
}

fn get_job(app: &Workbench, id: Uuid) -> Result<Job> {
    app.0
        .catalog
        .lock()
        .unwrap()
        .desktop_jobs
        .get(&id)
        .cloned()
        .ok_or_else(crate::workbench::missing)
}
fn report(app: &Workbench, id: Uuid, owner: Uuid, r: Report) -> Result<Stage> {
    let guard = app.0.desktop_updates.running.lock().unwrap();
    if !guard.get(&id).is_some_and(|(who, _)| *who == owner) {
        return Err(bad("desktop status request was replaced"));
    }
    if r.request_id != id
        || r.message.as_deref().is_some_and(|s| s.len() > 2048)
        || r.version.as_deref().is_some_and(|s| s.len() > 128)
    {
        return Err(bad("invalid desktop updater report"));
    }
    let stage = app.update(|db| {
        let job = db
            .desktop_jobs
            .get_mut(&id)
            .ok_or_else(crate::workbench::missing)?;
        let allowed = if job.commit_started {
            matches!(
                r.stage,
                Stage::Committing
                    | Stage::Installing
                    | Stage::Installed
                    | Stage::Failed
                    | Stage::Unknown
            )
        } else {
            matches!(
                r.stage,
                Stage::Checking
                    | Stage::Downloading
                    | Stage::Ready
                    | Stage::Current
                    | Stage::Failed
                    | Stage::Unknown
            )
        };
        if !allowed {
            return Err(bad(
                "desktop updater reported a stage for a different operation",
            ));
        }
        job.stage = r.stage;
        if let Some(version) = r.version {
            job.version = Some(version);
        }
        job.message = r.message;
        job.revision += 1;
        Ok(job.stage)
    })?;
    app.0
        .desktop_updates
        .changed
        .send_modify(|n| *n = n.wrapping_add(1));
    Ok(stage)
}
#[cfg(unix)]
async fn contact(app: &Workbench, job: &Job, op: &str, owner: Uuid) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let path = socket().ok_or_else(|| bad("Kar desktop update host is unavailable"))?;
    let mut connection = tokio::net::UnixStream::connect(path).await?;
    if connection.peer_cred()?.uid() != nix::unistd::geteuid().as_raw() {
        return Err(bad("unexpected desktop peer identity"));
    }
    let mut wire = serde_json::to_vec(
        &json!({"protocol":"kar-desktop-update/1","operation":op,"request_id":job.request_id,"track":job.track}),
    )?;
    wire.push(b'\n');
    connection.write_all(&wire).await?;
    let mut reader = BufReader::new(connection);
    loop {
        let mut line = Vec::new();
        let size = (&mut reader)
            .take(8193)
            .read_until(b'\n', &mut line)
            .await?;
        if size == 0 || size > 8192 || line.last() != Some(&b'\n') {
            return Err(bad("desktop update host stopped reporting"));
        }
        let stage = report(app, job.request_id, owner, serde_json::from_slice(&line)?)?;
        if stage.terminal() || stage == Stage::Unknown {
            return Ok(());
        }
    }
}
#[cfg(not(unix))]
async fn contact(_: &Workbench, _: &Job, _: &str, _: Uuid) -> Result<()> {
    Err(Error::Unsupported(
        "Kar desktop update host on this platform".into(),
    ))
}

fn follow(app: &Workbench, job: Job, op: &'static str) {
    let service = &app.0.desktop_updates;
    let owner = Uuid::new_v4();
    let stop = tokio_util::sync::CancellationToken::new();
    {
        let mut workers = service.running.lock().unwrap();
        if op == "status" && workers.contains_key(&job.request_id) {
            return;
        }
        if let Some((_, old)) = workers.insert(job.request_id, (owner, stop.clone())) {
            old.cancel();
        }
    }
    service.changed.send_modify(|n| *n = n.wrapping_add(1));
    let host = app.clone();
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            _=stop.cancelled()=>return,
            r=tokio::time::timeout(Duration::from_secs(600),contact(&host,&job,op,owner))=>r,
        };
        let detail = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some("desktop update host timed out".to_owned()),
        };
        if let Some(detail) = detail {
            let _ = report(
                &host,
                job.request_id,
                owner,
                Report {
                    request_id: job.request_id,
                    stage: Stage::Unknown,
                    version: None,
                    message: Some(detail),
                },
            );
        }
        let mut workers = host.0.desktop_updates.running.lock().unwrap();
        if workers
            .get(&job.request_id)
            .is_some_and(|(who, _)| *who == owner)
        {
            workers.remove(&job.request_id);
        }
    });
}
async fn prepare(State(app): State<Workbench>, Json(input): Json<Prepare>) -> Reply {
    if !available() {
        return Err(Error::Unsupported("Kar desktop update host is unavailable".into()).into());
    }
    if input
        .track
        .as_deref()
        .is_some_and(|v| !matches!(v, "stable" | "beta" | "nightly"))
    {
        return Err(bad("unknown Kar release track").into());
    }
    let (job, fresh) = app.update(|db| {
        if let Some(job) = db.desktop_jobs.get(&input.request_id) {
            if job.track != input.track {
                return Err(bad("request ID already belongs to a different track"));
            }
            return Ok((job.clone(), false));
        }
        if db.desktop_jobs.values().any(|j| j.stage.busy()) {
            return Err(Error::Busy);
        }
        let job = Job {
            request_id: input.request_id,
            stage: Stage::Preparing,
            commit_started: false,
            track: input.track,
            version: None,
            message: None,
            revision: 1,
        };
        db.desktop_jobs.insert(job.request_id, job.clone());
        Ok((job, true))
    })?;
    if !job.stage.terminal() {
        follow(&app, job.clone(), if fresh { "prepare" } else { "status" });
    }
    Ok(Json(json!(job)))
}
async fn commit(State(app): State<Workbench>, Path(id): Path<Uuid>) -> Reply {
    if !available() {
        return Err(bad("Kar desktop update host is unavailable").into());
    }
    let (job, fresh) = app.update(|db| {
        let job = db
            .desktop_jobs
            .get_mut(&id)
            .ok_or_else(crate::workbench::missing)?;
        if job.commit_started {
            return Ok((job.clone(), false));
        }
        if job.stage != Stage::Ready {
            return Err(bad("update has not been prepared"));
        }
        job.stage = Stage::Committing;
        job.commit_started = true;
        job.revision += 1;
        Ok((job.clone(), true))
    })?;
    if !job.stage.terminal() {
        follow(&app, job.clone(), if fresh { "commit" } else { "status" });
    }
    Ok(Json(json!(job)))
}
async fn read(State(app): State<Workbench>, Path(id): Path<Uuid>) -> Reply {
    let job = get_job(&app, id)?;
    if !job.stage.terminal() && available() {
        follow(&app, job.clone(), "status");
    }
    Ok(Json(json!(job)))
}
async fn list(State(app): State<Workbench>) -> Reply {
    let db = app.0.catalog.lock().unwrap();
    Ok(Json(
        json!({"available":available(),"requests":db.desktop_jobs.values().collect::<Vec<_>>()}),
    ))
}
async fn events(
    State(app): State<Workbench>,
    Path(id): Path<Uuid>,
) -> std::result::Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>, ApiError>
{
    let rx = app.0.desktop_updates.changed.subscribe();
    let job = get_job(&app, id)?;
    if !job.stage.terminal() && available() {
        follow(&app, job, "status");
    }
    let stream =
        futures_util::stream::unfold((app, rx, None), move |(app, mut rx, last)| async move {
            loop {
                let job = get_job(&app, id).ok()?;
                if last != Some(job.revision) {
                    let e = Event::default()
                        .event("update")
                        .id(job.revision.to_string())
                        .json_data(&job)
                        .ok()?;
                    return Some((Ok(e), (app, rx, Some(job.revision))));
                }
                rx.changed().await.ok()?;
            }
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/server/desktop-update", get(list).post(prepare))
        .route("/server/desktop-update/{id}", get(read))
        .route("/server/desktop-update/{id}/commit", post(commit))
        .route("/server/desktop-update/{id}/events", get(events))
}
