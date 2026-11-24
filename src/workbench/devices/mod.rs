//! Host-local device discovery, controls, and per-chat claims.
mod account;
mod actions;
mod native;
use super::{Workbench, http::ApiError};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs::File,
    path::{Path, PathBuf},
};
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct Settings {
    enabled: bool,
    agent_access_enabled: bool,
    onboarding_completed: bool,
}
#[derive(Clone, Serialize)]
struct Device {
    host_id: &'static str,
    id: String,
    platform: &'static str,
    name: String,
    version: String,
    booted: bool,
    physical: bool,
}
impl Device {
    fn new(
        id: &str,
        platform: &'static str,
        name: &str,
        version: String,
        booted: bool,
        physical: bool,
    ) -> Self {
        Self {
            host_id: "local",
            id: id.into(),
            platform,
            name: name.into(),
            version,
            booted,
            physical,
        }
    }
}
#[derive(Clone, Serialize)]
struct Session {
    chat: Uuid,
    host_id: &'static str,
    device_id: String,
    platform: &'static str,
    opened_at: String,
}
#[derive(Clone, Default)]
struct Live {
    settings: Settings,
    revision: u64,
    devices: Vec<Device>,
    sessions: Vec<Session>,
    errors: BTreeMap<String, String>,
    inspected: bool,
}
impl Live {
    fn snapshot(&self) -> Value {
        let platforms:Vec<_>=["ios","android"].into_iter().map(|p|{
            let mut v=json!({"platform":p,"available":self.inspected && !self.errors.contains_key(p)});
            if let Some(e)=self.errors.get(p){v["reason"]=json!(e);}v
        }).collect();
        let status = if !self.settings.enabled {
            "disabled"
        } else if !self.inspected {
            "idle"
        } else if self.errors.len() == 2 {
            "failed"
        } else {
            "ready"
        };
        json!({"settings":self.settings,"revision":self.revision,"host_status":status,"hosts":[{"id":"local","kind":"local","label":"This machine","platforms":platforms}],"incomplete_platforms":self.errors.keys().collect::<Vec<_>>(),"devices":self.devices,"sessions":self.sessions})
    }
    fn device(&self, id: &str) -> Result<Device> {
        if !self.settings.enabled {
            return Err(Error::Refused("device support is disabled".into()));
        }
        self.devices
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or_else(|| Error::Refused("device unavailable on the selected host".into()))
    }
}
pub(super) struct Service {
    file: PathBuf,
    live: Mutex<Live>,
    events: watch::Sender<Value>,
}
impl Service {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let file = path.join("devices.json");
        let settings = match File::open(&file) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e.into()),
        };
        let live = Live {
            settings,
            ..Default::default()
        };
        let events = watch::channel(live.snapshot()).0;
        Ok(Self {
            file,
            live: Mutex::new(live),
            events,
        })
    }
    fn publish(&self, s: &mut Live) -> Value {
        s.revision = s.revision.saturating_add(1);
        let v = s.snapshot();
        self.events.send_replace(v.clone());
        v
    }
    async fn refresh(&self, s: &mut Live) -> Value {
        let (ios, android) = tokio::join!(native::ios_list(), native::android_list());
        for (platform, result) in [("ios", ios), ("android", android)] {
            match result {
                Ok(rows) => {
                    s.errors.remove(platform);
                    s.devices.retain(|d| d.platform != platform);
                    s.devices.extend(rows);
                }
                Err(e) => {
                    s.errors.insert(platform.into(), e.to_string());
                }
            }
        }
        s.devices
            .sort_by(|a, b| (a.platform, &a.name, &a.id).cmp(&(b.platform, &b.name, &b.id)));
        s.inspected = true;
        self.publish(s)
    }
    async fn resolve(&self, s: &mut Live, id: &str) -> Result<Device> {
        if !s.settings.enabled {
            return Err(Error::Refused("device support is disabled".into()));
        }
        self.refresh(s).await;
        let device = s.device(id)?;
        if let Some(cause) = s.errors.get(device.platform) {
            return Err(Error::Refused(cause.clone()));
        }
        Ok(device)
    }
    fn save(&self, settings: &Settings) -> Result<()> {
        let parent = self.file.parent().expect("workbench state");
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut tmp, settings)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.file).map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}
type Reply<T> = std::result::Result<T, ApiError>;
fn host(id: Option<&str>) -> Result<()> {
    if id.is_none_or(|v| v == "local") {
        Ok(())
    } else {
        Err(Error::Unsupported(
            "host is not configured for local device operations".into(),
        ))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    enabled: Option<bool>,
    agent_access_enabled: Option<bool>,
    onboarding_completed: Option<bool>,
}
async fn configure(State(app): State<Workbench>, Json(q): Json<Configure>) -> Reply<Json<Value>> {
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let mut next = s.settings.clone();
    if let Some(b) = q.enabled {
        next.enabled = b;
    }
    if let Some(b) = q.onboarding_completed {
        next.onboarding_completed = b;
    }
    if let Some(b) = q.agent_access_enabled {
        next.agent_access_enabled = b;
    }
    svc.save(&next)?;
    s.settings = next;
    if !s.settings.enabled {
        s.sessions.clear();
        s.devices.clear();
        s.inspected = false;
        s.errors.clear();
    }
    Ok(Json(svc.publish(&mut s)))
}
#[derive(Deserialize)]
struct List {
    inspect_only: Option<bool>,
    retry_host_id: Option<String>,
    update_tool: Option<String>,
}
async fn list(State(app): State<Workbench>, Query(q): Query<List>) -> Reply<Json<Value>> {
    host(q.retry_host_id.as_deref())?;
    if q.update_tool.is_some() {
        return Err(Error::Unsupported(
            "device tools are managed by Xcode and the Android SDK".into(),
        )
        .into());
    }
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    Ok(Json(
        if s.settings.enabled || q.inspect_only == Some(true) {
            svc.refresh(&mut s).await
        } else {
            s.snapshot()
        },
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SshProbe {
    id: String,
    label: String,
    target: String,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
    kind: Option<String>,
}
async fn probe(Json(q): Json<SshProbe>) -> Reply<Json<Value>> {
    if q.kind.as_deref().is_some_and(|s| s != "ssh")
        || q.id == "local"
        || q.id.is_empty()
        || q.label.trim().is_empty()
        || q.port == Some(0)
        || q.target.is_empty()
        || q.target.starts_with('-')
        || q.target.len() > 512
        || !q
            .target
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"@._:[]%-".contains(&c))
    {
        return Err(Error::Refused("invalid SSH device host".into()).into());
    }
    let mut args = vec![
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ClearAllForwardings=yes",
    ];
    let port = q.port.map(|n| n.to_string());
    if let Some(p) = &port {
        args.extend(["-p", p]);
    }
    if let Some(path) = &q.identity_file {
        let path = path
            .to_str()
            .filter(|_| path.is_absolute() && path.is_file())
            .ok_or_else(|| Error::Refused("invalid SSH identity file".into()))?;
        args.extend(["-i", path]);
    }
    // Only a fixed capability probe runs remotely. The destination and key path remain separate local argv.
    args.extend(["--",&q.target,"uname -s; command -v xcrun >/dev/null && printf 'ios\n'; command -v adb >/dev/null && printf 'android\n'; true"]);
    let result = native::text("ssh", &args)
        .await
        .map_err(|_| Error::Refused("could not complete the SSH device host probe".into()))?;
    Ok(Json(
        json!({"id":q.id,"label":q.label,"kind":"ssh","platforms":[
        {"platform":"ios","available":result.lines().any(|s|s=="Darwin") && result.lines().any(|s|s=="ios")},
        {"platform":"android","available":result.lines().any(|s|s=="android")}]}),
    ))
}

#[derive(Deserialize)]
struct Target {
    host_id: Option<String>,
    device_id: String,
}
#[derive(Deserialize)]
struct Open {
    project: Uuid,
    chat: Uuid,
    platform: String,
    boot: Option<bool>,
    #[serde(flatten)]
    target: Target,
}
async fn open(State(app): State<Workbench>, Json(q): Json<Open>) -> Reply<Json<Session>> {
    host(q.target.host_id.as_deref())?;
    app.binding(q.project, q.chat)?;
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let mut device = svc.resolve(&mut s, &q.target.device_id).await?;
    if device.platform != q.platform {
        return Err(Error::Refused("device platform differs".into()).into());
    }
    if !device.booted && q.boot.unwrap_or(true) {
        let id = native::boot(&device).await?;
        device = svc.resolve(&mut s, &id).await?;
        if !device.booted {
            return Err(Error::Refused("device did not finish booting".into()).into());
        }
    }
    app.binding(q.project, q.chat)?;
    let session = Session {
        chat: q.chat,
        host_id: "local",
        device_id: device.id,
        platform: device.platform,
        opened_at: jiff::Timestamp::now().to_string(),
    };
    s.sessions
        .retain(|a| a.chat != q.chat || a.device_id != session.device_id);
    s.sessions.push(session.clone());
    svc.publish(&mut s);
    Ok(Json(session))
}
#[derive(Deserialize)]
struct Close {
    chat: Uuid,
    host_id: Option<String>,
    device_id: Option<String>,
    shutdown: Option<bool>,
}
async fn close(State(app): State<Workbench>, Json(q): Json<Close>) -> Reply<Json<()>> {
    host(q.host_id.as_deref())?;
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let ids: Vec<_> = s
        .sessions
        .iter()
        .filter(|v| v.chat == q.chat && q.device_id.as_ref().is_none_or(|id| *id == v.device_id))
        .map(|v| v.device_id.clone())
        .collect();
    for id in ids {
        if q.shutdown.unwrap_or(false) {
            let d = svc.resolve(&mut s, &id).await?;
            native::shutdown(&d).await?;
            s.sessions.retain(|v| v.device_id != id);
            s.devices
                .iter_mut()
                .filter(|d| d.id == id)
                .for_each(|d| d.booted = false);
        } else {
            s.sessions.retain(|v| v.device_id != id || v.chat != q.chat);
        }
        svc.publish(&mut s);
    }
    Ok(Json(()))
}
async fn shutdown(State(app): State<Workbench>, Json(q): Json<Target>) -> Reply<Json<()>> {
    host(q.host_id.as_deref())?;
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, &q.device_id).await?;
    native::shutdown(&d).await?;
    s.sessions.retain(|v| v.device_id != d.id);
    svc.refresh(&mut s).await;
    Ok(Json(()))
}
async fn detail(State(app): State<Workbench>, Query(q): Query<Target>) -> Reply<Json<Value>> {
    host(q.host_id.as_deref())?;
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, &q.device_id).await?;
    Ok(Json(actions::detail(&d).await?))
}
async fn action(State(app): State<Workbench>, Json(q): Json<Value>) -> Reply<Json<Value>> {
    let target: Target = serde_json::from_value(q.clone())
        .map_err(|_| Error::Refused("invalid device target".into()))?;
    host(target.host_id.as_deref())?;
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, &target.device_id).await?;
    actions::apply(&d, &q).await?;
    Ok(Json(actions::detail(&d).await?))
}
async fn screenshot(State(app): State<Workbench>, Query(q): Query<Target>) -> Reply<Response> {
    host(q.host_id.as_deref())?;
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, &q.device_id).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        native::screenshot(&d).await?,
    )
        .into_response())
}
async fn events(State(app): State<Workbench>) -> Response {
    let rx = app.0.devices.events.subscribe();
    let feed = futures_util::stream::unfold((rx, true), |(mut rx, initial)| async move {
        if !initial && rx.changed().await.is_err() {
            return None;
        }
        let v = rx.borrow_and_update().clone();
        let event = Event::default()
            .id(v["revision"].to_string())
            .event("devices")
            .json_data(v)
            .ok()?;
        Some((Ok::<_, Infallible>(event), (rx, false)))
    });
    Sse::new(feed)
        .keep_alive(KeepAlive::default())
        .into_response()
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/devices", get(list))
        .route("/devices/events", get(events))
        .route("/devices/configure", post(configure))
        .route("/devices/test-host", post(probe))
        .route("/devices/open", post(open))
        .route("/devices/close", post(close))
        .route("/devices/shutdown", post(shutdown))
        .route("/devices/detail", get(detail))
        .route("/devices/action", post(action))
        .route("/devices/screenshot", get(screenshot))
        .route("/devices/eas-account", post(account::eas))
        .route("/devices/simlock-identity", post(account::simlock))
}
