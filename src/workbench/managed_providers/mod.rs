//! Managed built-in releases, shared by all instances of the same driver.
use super::{Workbench, http::ApiError};
use crate::{Error, Result, Runtime, chat::ChatStore};
use axum::{
    Json, Router,
    extract::{Path as RoutePath, State},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
mod releases;
type Reply = std::result::Result<Json<Value>, ApiError>;
fn bad(s: &str) -> Error {
    Error::Refused(s.into())
}
fn driver(id: &str) -> Result<String> {
    if matches!(id, "codex" | "antigravity") {
        return Ok(id.into());
    }
    let instance = crate::provider_instance::for_launch("codex", id)?;
    if instance.program.is_some() {
        return Err(bad(
            "Custom provider executables are managed by their owner",
        ));
    }
    Ok("codex".into())
}
fn dir(store: &ChatStore, driver: &str) -> PathBuf {
    store
        .directory()
        .join("workbench/managed-providers")
        .join(driver)
}
fn read<T: serde::de::DeserializeOwned>(p: &Path) -> Result<Option<T>> {
    match File::open(p) {
        Ok(f) => Ok(Some(serde_json::from_reader(f.take(131072))?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn write(path: &Path, data: &impl Serialize) -> Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let mut f = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut f, data)?;
    f.flush()?;
    f.as_file().sync_all()?;
    f.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn lock(root: &Path, name: &str, exclusive: bool) -> Result<File> {
    fs::create_dir_all(root)?;
    let f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(name))?;
    let result = if exclusive {
        f.try_lock()
    } else {
        f.try_lock_shared()
    };
    result.map_err(|e| match e {
        fs::TryLockError::WouldBlock => Error::Busy,
        fs::TryLockError::Error(e) => e.into(),
    })?;
    Ok(f)
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    driver: String,
    version: String,
    target: String,
    digest: String,
}
impl Record {
    fn executable(&self) -> &'static str {
        if self.driver == "codex" {
            "bin/codex"
        } else {
            "antigravity"
        }
    }
    fn path(&self, root: &Path) -> Result<PathBuf> {
        if !matches!(self.driver.as_str(), "codex" | "antigravity")
            || !matches!(self.digest.len(), 64 | 128)
            || !self.digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(bad("Invalid managed provider receipt"));
        }
        let base = root.join("releases").join(&self.digest).join("payload");
        let exe = base.join(self.executable());
        let resolved = exe.canonicalize()?;
        if !resolved.starts_with(root.canonicalize()?.join("releases")) || !resolved.is_file() {
            return Err(bad("Invalid managed provider executable"));
        }
        Ok(resolved)
    }
}
fn managed(root: &Path, driver: &str) -> Result<bool> {
    let opt = read::<String>(&root.join("selection.json"))?;
    Ok(opt
        .as_deref()
        .map_or(driver == "antigravity", |s| s == "managed"))
}
fn external(driver: &str) -> bool {
    let name = if driver == "codex" { "codex" } else { "agy" };
    std::env::var_os(format!("KAR_{}_BIN", driver.to_ascii_uppercase()))
        .is_some_and(|s| !s.is_empty() && s != name)
}
pub(crate) fn resolve(store: &ChatStore, mut rt: Runtime) -> Result<Runtime> {
    if !matches!(rt.id.as_str(), "codex" | "antigravity") || external(&rt.id) {
        return Ok(rt);
    }
    let root = dir(store, &rt.id);
    if managed(&root, &rt.id)?
        && let Some(record) = read::<Record>(&root.join("active.json"))?
    {
        rt.program = record.path(&root)?
    }
    Ok(rt)
}
pub(crate) fn pin(runtime: &Runtime) -> Result<Option<File>> {
    if !matches!(runtime.id.as_str(), "codex" | "antigravity") {
        return Ok(None);
    }
    let Some(root) = runtime
        .program
        .ancestors()
        .find(|p| p.ends_with(Path::new("workbench/managed-providers").join(&runtime.id)))
    else {
        return Ok(None);
    };
    if !runtime.program.starts_with(root.join("releases")) {
        return Ok(None);
    }
    let f = lock(root, "in-use.lock", false)?;
    if !runtime.program.is_file() {
        return Err(bad("Managed provider was removed before launch"));
    }
    Ok(Some(f))
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    driver: String,
    operation_id: Option<Uuid>,
    phase: String,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    version: Option<String>,
    installed_version: Option<String>,
    executable_path: Option<PathBuf>,
    source: Option<String>,
    can_remove: bool,
    message: Option<String>,
}
impl View {
    fn idle(driver: &str) -> Self {
        Self {
            driver: driver.into(),
            operation_id: None,
            phase: "idle".into(),
            downloaded_bytes: 0,
            total_bytes: None,
            version: None,
            installed_version: None,
            executable_path: None,
            source: None,
            can_remove: false,
            message: None,
        }
    }
    fn active(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "downloading" | "extracting" | "verifying"
        )
    }
    fn installed(&mut self, root: &Path) -> Result<bool> {
        if let Some(r) = read::<Record>(&root.join("active.json"))? {
            self.executable_path = Some(r.path(root)?);
            self.installed_version = Some(r.version);
            self.source = Some("managed".into());
            self.can_remove = true;
            return Ok(true);
        }
        Ok(false)
    }
}
struct Running {
    id: Uuid,
    cancel: CancellationToken,
}
struct Slot {
    view: watch::Sender<View>,
    job: Mutex<Option<Running>>,
}
#[derive(Default)]
pub(super) struct Manager(Mutex<BTreeMap<String, Arc<Slot>>>);
fn slot(app: &Workbench, driver: &str) -> Result<Arc<Slot>> {
    let mut map = app.0.managed_providers.0.lock().unwrap();
    if let Some(s) = map.get(driver) {
        return Ok(s.clone());
    }
    let root = dir(&app.0.chats, driver);
    let mut v = read::<View>(&root.join("operation.json"))?.unwrap_or_else(|| View::idle(driver));
    if v.active() {
        v.phase = "failed".into();
        v.message =
            Some("Kar restarted during the installation; retry to verify and finish".into());
    }
    v.installed(&root)?;
    let s = Arc::new(Slot {
        view: watch::channel(v).0,
        job: Mutex::new(None),
    });
    map.insert(driver.into(), s.clone());
    Ok(s)
}
#[derive(Clone)]
struct Local {
    forced: bool,
    path: Option<PathBuf>,
    version: Option<String>,
}
impl Local {
    fn project(&self, mut v: View) -> View {
        if self.forced || v.source.as_deref() != Some("managed") {
            v.executable_path = self.path.clone();
            v.installed_version = self.version.clone();
            v.source = self.path.as_ref().map(|_| "local".into());
            v.can_remove = false;
        }
        v
    }
}
async fn observe(app: &Workbench, id: &str) -> Result<(Local, Arc<Slot>)> {
    let (d, custom) = if matches!(id, "codex" | "antigravity") {
        (id, None)
    } else {
        (
            "codex",
            crate::provider_instance::for_launch("codex", id)?.program,
        )
    };
    let root = dir(&app.0.chats, d);
    let s = slot(app, d)?;
    let forced = external(d) || custom.is_some() || !managed(&root, d)?;
    let mut local = Local {
        forced,
        path: None,
        version: None,
    };
    if forced || (!s.view.borrow().active() && !root.join("active.json").is_file()) {
        let mut rt = crate::runtime(d)?;
        if let Some(p) = custom {
            rt.program = p
        }
        let probe = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            rt.probe_installation(&Default::default()),
        )
        .await;
        if let Ok(Ok(snapshot)) = probe {
            local.path = snapshot.command().map(Path::to_path_buf);
            local.version = match snapshot {
                crate::InstallationSnapshot::Available { version, .. } => version,
                crate::InstallationSnapshot::Failed {
                    installed_version, ..
                } => installed_version,
                _ => None,
            };
        }
    }
    Ok((local, s))
}
async fn view(State(app): State<Workbench>, RoutePath(id): RoutePath<String>) -> Reply {
    let (local, s) = observe(&app, &id).await?;
    let state = local.project(s.view.borrow().clone());
    Ok(Json(json!(state)))
}
async fn events(
    State(app): State<Workbench>,
    RoutePath(id): RoutePath<String>,
) -> std::result::Result<
    Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>,
    ApiError,
> {
    let (local, s) = observe(&app, &id).await?;
    let rx = s.view.subscribe();
    let stream =
        futures_util::stream::unfold((rx, true, local), |(mut rx, first, local)| async move {
            if !first {
                rx.changed().await.ok()?
            }
            let v = local.project(rx.borrow_and_update().clone());
            let e = Event::default().event("installation").json_data(v).ok()?;
            Some((Ok(e), (rx, false, local)))
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selection {
    setup_mode: String,
}
async fn select(
    State(app): State<Workbench>,
    RoutePath(id): RoutePath<String>,
    Json(s): Json<Selection>,
) -> Reply {
    let d = driver(&id)?;
    let root = dir(&app.0.chats, &d);
    let _guard = lock(&root, "operation.lock", true)?;
    if !matches!(s.setup_mode.as_str(), "managed" | "existing") {
        return Err(bad("Choose managed or existing installation").into());
    }
    if external(&d) && s.setup_mode == "managed" {
        return Err(bad("A custom executable was configured on the host").into());
    }
    write(&root.join("selection.json"), &s.setup_mode)?;
    Ok(Json(
        json!({"instanceId":id,"driver":d,"setupMode":s.setup_mode}),
    ))
}
async fn start(State(app): State<Workbench>, RoutePath(id): RoutePath<String>) -> Reply {
    let d = driver(&id)?;
    let root = dir(&app.0.chats, &d);
    if external(&d) || !managed(&root, &d)? {
        return Err(
            bad("Select managed setup and clear any custom executable before installing").into(),
        );
    }
    let slot = slot(&app, &d)?;
    let mut job = slot.job.lock().unwrap();
    if job.is_some() {
        return Ok(Json(json!(*slot.view.borrow())));
    }
    let file_lock = lock(&root, "operation.lock", true)?;
    let op = Uuid::new_v4();
    let cancel = CancellationToken::new();
    let mut v = slot.view.borrow().clone();
    v.phase = "downloading".into();
    v.operation_id = Some(op);
    v.message = None;
    v.downloaded_bytes = 0;
    v.total_bytes = None;
    write(&root.join("operation.json"), &v)?;
    *job = Some(Running {
        id: op,
        cancel: cancel.clone(),
    });
    slot.view.send_replace(v.clone());
    drop(job);
    let updates = super::mcp::collaboration_tools::environment::prefs(
        &app.0.catalog.lock().unwrap(),
    )["enable_provider_update_checks"]
        .as_bool()
        .unwrap_or(true);
    let worker = slot.clone();
    tokio::spawn(async move {
        let _guard = file_lock;
        let prepared = async {
            let stage = tempfile::Builder::new()
                .prefix(".kar-stage-")
                .tempdir_in(&root)?;
            let record =
                releases::prepare(&d, updates, stage.path(), &worker.view, &cancel).await?;
            let download = stage.path().join("download");
            if download.try_exists()? {
                fs::remove_file(download)?;
            }
            write(&stage.path().join("record.json"), &record)?;
            Ok::<_, Error>((stage, record))
        }
        .await;
        let mut job = worker.job.lock().unwrap();
        let mut next = worker.view.borrow().clone();
        let result = prepared.and_then(|(stage, r)| {
            if cancel.is_cancelled() {
                return Err(bad("Cancelled"));
            }
            let releases = root.join("releases");
            fs::create_dir_all(&releases)?;
            let dest = releases.join(&r.digest);
            if !dest.exists() {
                fs::rename(stage.path(), &dest)?;
            } else if read::<Record>(&dest.join("record.json"))?
                .is_none_or(|v| v.digest != r.digest || v.version != r.version)
            {
                return Err(bad("Existing installation does not match this release"));
            }
            r.path(&root)?;
            write(&root.join("active.json"), &r)?;
            next.installed(&root)?;
            crate::clear_installation_cache();
            Ok(())
        });
        next.phase = match result {
            Ok(()) => "succeeded".into(),
            Err(_) if cancel.is_cancelled() => "cancelled".into(),
            Err(e) => {
                next.message = Some(e.to_string());
                "failed".into()
            }
        };
        if write(&root.join("operation.json"), &next).is_err() {
            next.message = Some("Could not persist installation progress".into());
        }
        *job = None;
        worker.view.send_replace(next);
    });
    Ok(Json(json!(v)))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cancel {
    operation_id: Uuid,
}
async fn cancel(
    State(app): State<Workbench>,
    RoutePath(id): RoutePath<String>,
    Json(p): Json<Cancel>,
) -> Reply {
    let s = slot(&app, &driver(&id)?)?;
    let job = s.job.lock().unwrap();
    if let Some(j) = job.as_ref().filter(|j| j.id == p.operation_id) {
        j.cancel.cancel()
    } else if s.view.borrow().operation_id != Some(p.operation_id) {
        return Err(bad("Unknown installer operation").into());
    }
    let v = s.view.borrow().clone();
    Ok(Json(json!(v)))
}
async fn remove(State(app): State<Workbench>, RoutePath(id): RoutePath<String>) -> Reply {
    Ok(Json(remove_managed(&app, &id)?))
}
fn remove_managed(app: &Workbench, id: &str) -> Result<Value> {
    let d = driver(&id)?;
    let root = dir(&app.0.chats, &d);
    let s = slot(app, &d)?;
    let _g = lock(&root, "operation.lock", true)?;
    let _use = lock(&root, "in-use.lock", true)?;
    if external(&d) || !managed(&root, &d)? {
        return Err(bad("External provider installations are owned by their installer").into());
    }
    let active = root.join("active.json");
    let releases = root.join("releases");
    if !releases.try_exists()? {
        if active.try_exists()? {
            return Err(bad("Managed provider receipt has no release directory"));
        }
        return Ok(json!(*s.view.borrow()));
    }
    if releases.canonicalize()? != root.canonicalize()?.join("releases") {
        return Err(bad("Managed release directory is redirected"));
    }
    let mut owned = Vec::new();
    for entry in fs::read_dir(&releases)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let key = entry.file_name();
        let Some(key) = key.to_str() else { continue };
        if !matches!(key.len(), 64 | 128) || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Some(record) = read::<Record>(&entry.path().join("record.json"))? else {
            continue;
        };
        if record.driver == d && record.digest == key {
            owned.push(entry.path())
        }
    }
    if let Some(record) = read::<Record>(&active)? {
        if !owned.contains(&releases.join(record.digest)) {
            return Err(bad("Active release does not have a matching owned receipt"));
        }
    }
    for instance in crate::provider_instance::configured()? {
        if let Some(custom) = instance.program {
            let mut paths = vec![custom.clone()];
            if custom.components().count() == 1 {
                paths.extend(
                    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                        .map(|p| p.join(&custom)),
                );
            }
            if paths
                .into_iter()
                .filter_map(|p| p.canonicalize().ok())
                .any(|p| owned.iter().any(|r| p.starts_with(r)))
            {
                return Err(bad(
                    "A custom provider instance still references a managed executable",
                ));
            }
        }
    }
    let trash = tempfile::Builder::new()
        .prefix(".kar-remove-")
        .tempdir_in(&root)?;
    if active.try_exists()? {
        owned.insert(0, active)
    }
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for source in owned {
        let dest = trash.path().join(source.file_name().unwrap());
        if let Err(e) = fs::rename(&source, &dest) {
            for (from, to) in moved.into_iter().rev() {
                fs::rename(to, from)?
            }
            return Err(e.into());
        }
        moved.push((source, dest));
    }
    let v = View::idle(&d);
    write(&root.join("operation.json"), &v)?;
    s.view.send_replace(v.clone());
    crate::clear_installation_cache();
    Ok(json!(v))
}
pub(super) fn owner_routes() -> Router<Workbench> {
    Router::new()
        .route("/providers/{id}/setup", put(select))
        .route("/providers/{id}/install/start", post(start))
        .route("/providers/{id}/install/cancel", post(cancel))
        .route("/providers/{id}/install/remove", post(remove))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/providers/{id}/install", get(view))
        .route("/providers/{id}/install/events", get(events))
}
