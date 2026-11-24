//! Host-local device discovery, controls, and per-chat claims.
mod account;
mod actions;
mod agent;
mod native;
mod remote;
use remote::SshHost;
mod screen;
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
    ssh_hosts: Vec<SshHost>,
    agent_access_enabled: bool,
    onboarding_completed: bool,
}
#[derive(Clone, Serialize)]
struct Device {
    host_id: String,
    #[serde(skip)]
    ssh: Option<SshHost>,
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
            host_id: "local".into(),
            ssh: None,
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
    view_id: Uuid,
    #[serde(skip)]
    grant: Option<agent::Grant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_config: Option<PathBuf>,
    project: Uuid,
    chat: Uuid,
    host_id: String,
    device_id: String,
    platform: &'static str,
    opened_at: String,
}
#[derive(Clone, Default)]
struct Live {
    settings: Settings,
    booting: Vec<Value>,
    revision: u64,
    devices: Vec<Device>,
    sessions: Vec<Session>,
    errors: BTreeMap<String, String>,
    inspected: bool,
}
impl Live {
    fn snapshot(&self) -> Value {
        let hosts:Vec<_>=std::iter::once(("local","local","This machine")).chain(self.settings.ssh_hosts.iter().map(|h|(h.id.as_str(),"ssh",h.label.as_str()))).map(|(id,kind,label)|{
            let platforms:Vec<_>=["ios","android"].into_iter().map(|p|{
                let k=format!("{id}/{p}");let mut v=json!({"platform":p,"available":self.inspected && !self.errors.contains_key(&k)});
                if let Some(e)=self.errors.get(&k){v["reason"]=json!(e);}v
            }).collect();
            let status=if !self.settings.enabled{"disabled"}else if !self.inspected{"idle"}else if platforms.iter().all(|p|p["available"]==false){"failed"}else{"ready"};
            json!({"id":id,"kind":kind,"label":label,"platforms":platforms,"status":status})
        }).collect();
        json!({"settings":self.settings,"revision":self.revision,"host_status":hosts[0]["status"],"hosts":hosts,"incomplete_platforms":self.errors.keys().collect::<Vec<_>>(),"devices":self.devices,"sessions":self.sessions,"booting_devices":self.booting})
    }
    fn remote(&self, id: &str) -> Result<Option<&SshHost>> {
        if id == "local" {
            return Ok(None);
        }
        self.settings
            .ssh_hosts
            .iter()
            .find(|v| v.id == id)
            .map(Some)
            .ok_or_else(|| Error::Refused("unknown device host".into()))
    }
    fn device(&self, host: &str, id: &str) -> Result<Device> {
        if !self.settings.enabled {
            return Err(Error::Refused("device support is disabled".into()));
        }
        self.devices
            .iter()
            .find(|d| d.id == id && d.host_id == host)
            .cloned()
            .ok_or_else(|| Error::Refused("device unavailable on the selected host".into()))
    }
}
pub(super) struct Service {
    lifecycle: Mutex<()>,
    screens: screen::Screens,
    agents: agent::AgentServer,
    file: PathBuf,
    live: Mutex<Live>,
    events: watch::Sender<Value>,
    polling: std::sync::atomic::AtomicBool,
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
            lifecycle: Mutex::new(()),
            screens: Default::default(),
            agents: agent::AgentServer::new(path)?,
            file,
            live: Mutex::new(live),
            events,
            polling: false.into(),
        })
    }
    fn publish(&self, s: &mut Live) -> Value {
        s.revision = s.revision.saturating_add(1);
        let v = s.snapshot();
        self.events.send_replace(v.clone());
        v
    }
    async fn refresh(&self, s: &mut Live) -> Value {
        let mut hosts = vec![None];
        hosts.extend(s.settings.ssh_hosts.iter().cloned().map(Some));
        self.refresh_hosts(s, hosts).await
    }
    async fn refresh_hosts(&self, s: &mut Live, hosts: Vec<Option<SshHost>>) -> Value {
        use futures_util::StreamExt;
        let before = s.snapshot();
        let mut work = futures_util::stream::iter(hosts)
            .map(|host| async move {
                let timeout = std::time::Duration::from_secs(20);
                let (ios, android) = remote::at(host.as_ref(), async {
                    tokio::join!(
                        tokio::time::timeout(timeout, native::ios_list()),
                        tokio::time::timeout(timeout, native::android_list())
                    )
                })
                .await;
                (
                    host,
                    ios.unwrap_or_else(|_| Err(Error::Refused("Xcode discovery timed out".into()))),
                    android.unwrap_or_else(|_| {
                        Err(Error::Refused("Android discovery timed out".into()))
                    }),
                )
            })
            .buffer_unordered(4);
        while let Some((host, ios, android)) = work.next().await {
            let id = host.as_ref().map_or("local", |s| s.id.as_str());
            for (platform, result) in [("ios", ios), ("android", android)] {
                let key = format!("{id}/{platform}");
                match result {
                    Ok(mut rows) => {
                        s.errors.remove(&key);
                        s.devices
                            .retain(|d| d.host_id != id || d.platform != platform);
                        for row in &mut rows {
                            row.host_id = id.into();
                            row.ssh = host.clone();
                        }
                        s.devices.extend(rows);
                    }
                    Err(e) => {
                        s.errors.insert(key, e.to_string());
                    }
                }
            }
        }
        s.devices.sort_by(|a, b| {
            (&a.host_id, a.platform, &a.name, &a.id).cmp(&(&b.host_id, b.platform, &b.name, &b.id))
        });
        s.inspected = true;
        if before == s.snapshot() {
            before
        } else {
            self.publish(s)
        }
    }
    async fn resolve(&self, s: &mut Live, host: &str, id: &str) -> Result<Device> {
        if !s.settings.enabled {
            return Err(Error::Refused("device support is disabled".into()));
        }
        let selected = s.remote(host)?.cloned();
        self.refresh_hosts(s, vec![selected]).await;
        let d = s.device(host, id)?;
        if let Some(cause) = s.errors.get(&format!("{host}/{}", d.platform)) {
            return Err(Error::Refused(cause.clone()));
        }
        Ok(d)
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    ssh_hosts: Option<Vec<SshHost>>,
    enabled: Option<bool>,
    agent_access_enabled: Option<bool>,
    onboarding_completed: Option<bool>,
}
async fn configure(State(app): State<Workbench>, Json(q): Json<Configure>) -> Reply<Json<Value>> {
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let mut next = s.settings.clone();
    if let Some(hosts) = q.ssh_hosts {
        let mut ids = std::collections::BTreeSet::new();
        for h in &hosts {
            h.validate()?;
            if !ids.insert(&h.id) {
                return Err(Error::Refused("duplicate SSH device host".into()).into());
            }
        }
        next.ssh_hosts = hosts;
    }
    if let Some(b) = q.enabled {
        next.enabled = b;
    }
    if let Some(b) = q.onboarding_completed {
        next.onboarding_completed = b;
    }
    if let Some(b) = q.agent_access_enabled {
        next.agent_access_enabled = b;
    }
    let mut grants = vec![];
    if next.enabled && next.agent_access_enabled {
        for row in &s.sessions {
            if row.grant.is_none() {
                grants.push((row.view_id, svc.agents.grant(&app).await?));
            }
        }
    }
    svc.save(&next)?;
    for row in &mut s.sessions {
        if !next.agent_access_enabled || !next.enabled {
            row.revoke();
        } else if let Some(i) = grants.iter().position(|(view, _)| *view == row.view_id) {
            let grant = grants.swap_remove(i).1;
            row.agent_config = Some(grant.file.clone());
            row.grant = Some(grant);
        }
    }
    let changed: Vec<_> = s
        .settings
        .ssh_hosts
        .iter()
        .filter(|h| !next.ssh_hosts.contains(h))
        .map(|h| h.id.clone())
        .collect();
    revoke_matching(&mut s, |v| changed.contains(&v.host_id));
    s.devices.retain(|d| !changed.contains(&d.host_id));
    s.errors
        .retain(|k, _| !changed.iter().any(|h| k.starts_with(&format!("{h}/"))));
    s.booting
        .retain(|v| !changed.iter().any(|h| v["host_id"] == *h));
    s.settings = next;
    if !s.settings.enabled {
        s.sessions.clear();
        s.booting.clear();
        s.devices.clear();
        s.inspected = false;
        s.errors.clear();
    }
    let state = svc.publish(&mut s);
    Ok(Json(if s.settings.enabled {
        svc.refresh(&mut s).await
    } else {
        state
    }))
}
#[derive(Deserialize)]
struct List {
    inspect_only: Option<bool>,
    retry_host_id: Option<String>,
    update_tool: Option<String>,
}
async fn list(State(app): State<Workbench>, Query(q): Query<List>) -> Reply<Json<Value>> {
    if q.update_tool.is_some() {
        return Err(Error::Unsupported(
            "device tools are managed by Xcode and the Android SDK".into(),
        )
        .into());
    }
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let host_id = q.retry_host_id.as_deref().unwrap_or("local");
    let selected = s.remote(host_id)?.cloned();
    if q.inspect_only == Some(true) {
        let (xcode, adb, emulator) = remote::at(selected.as_ref(), async {
            tokio::join!(
                native::text("xcrun", &["--find", "simctl"]),
                native::text("adb", &["version"]),
                native::text("emulator", &["-version"])
            )
        })
        .await;
        let mut v = s.snapshot();
        if let Some(h) = v["hosts"]
            .as_array_mut()
            .and_then(|hs| hs.iter_mut().find(|h| h["id"] == host_id))
        {
            h["tool_inspection"] = json!({"simctl":xcode.ok(),"adb":adb.ok().and_then(|s|s.lines().next().map(str::to_string)),"emulator":emulator.ok().and_then(|s|s.lines().next().map(str::to_string))});
        }
        return Ok(Json(v));
    }
    Ok(Json(if !s.settings.enabled {
        s.snapshot()
    } else if q.retry_host_id.is_some() {
        svc.refresh_hosts(&mut s, vec![selected]).await
    } else {
        svc.refresh(&mut s).await
    }))
}
async fn probe(Json(host): Json<SshHost>) -> Reply<Json<Value>> {
    Ok(Json(host.probe().await?))
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
    tokio::spawn(open_device(app, q))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
}
async fn open_device(app: Workbench, q: Open) -> Reply<Json<Session>> {
    let host = q.target.host_id.as_deref().unwrap_or("local");
    app.binding(q.project, q.chat)?;
    let svc = &app.0.devices;
    let _operation = svc.lifecycle.lock().await;
    let mut s = svc.live.lock().await;
    let mut device = svc.resolve(&mut s, host, &q.target.device_id).await?;
    if device.platform != q.platform {
        return Err(Error::Refused("device platform differs".into()).into());
    }
    if !device.booted && q.boot.unwrap_or(true) {
        let attempt = Uuid::new_v4();
        s.booting.push(json!({"id":device.id,"platform":device.platform,"host_id":host,"chat":q.chat,"attempt":attempt}));
        svc.publish(&mut s);
        drop(s);
        let boot = remote::at(device.ssh.as_ref(), native::boot(&device)).await;
        s = svc.live.lock().await;
        let valid = s
            .booting
            .iter()
            .any(|v| v["attempt"] == attempt.to_string());
        s.booting.retain(|v| v["attempt"] != attempt.to_string());
        svc.publish(&mut s);
        if !valid || !s.settings.enabled {
            return Err(Error::Refused("device configuration changed during boot".into()).into());
        }
        let id = boot?;
        device = svc.resolve(&mut s, host, &id).await?;
        if !device.booted {
            return Err(Error::Refused("device did not finish booting".into()).into());
        }
    }
    app.binding(q.project, q.chat)?;
    let grant = if s.settings.agent_access_enabled {
        Some(svc.agents.grant(&app).await?)
    } else {
        None
    };
    let session = Session {
        view_id: Uuid::new_v4(),
        agent_config: grant.as_ref().map(|g| g.file.clone()),
        grant,
        project: q.project,
        chat: q.chat,
        host_id: host.into(),
        device_id: device.id,
        platform: device.platform,
        opened_at: jiff::Timestamp::now().to_string(),
    };
    revoke_matching(&mut s, |v| {
        v.host_id == host && v.chat == q.chat && v.device_id == session.device_id
    });
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
fn revoke_matching(s: &mut Live, mut matches: impl FnMut(&Session) -> bool) {
    s.sessions.retain_mut(|v| {
        if matches(v) {
            v.revoke();
            false
        } else {
            true
        }
    });
}
async fn close(State(app): State<Workbench>, Json(q): Json<Close>) -> Reply<Json<()>> {
    let svc = &app.0.devices;
    let _operation = svc.lifecycle.lock().await;
    let mut s = svc.live.lock().await;
    if let Some(host) = q.host_id.as_deref() {
        s.remote(host)?;
    }
    let ids: Vec<_> = s
        .sessions
        .iter()
        .filter(|v| {
            v.chat == q.chat
                && q.device_id.as_ref().is_none_or(|id| *id == v.device_id)
                && q.host_id.as_ref().is_none_or(|h| *h == v.host_id)
        })
        .map(|v| (v.host_id.clone(), v.device_id.clone()))
        .collect();
    for (host, id) in ids {
        let host = host.as_str();
        if q.shutdown.unwrap_or(false) {
            let d = svc.resolve(&mut s, host, &id).await?;
            remote::at(d.ssh.as_ref(), native::shutdown(&d)).await?;
            revoke_matching(&mut s, |v| v.host_id == host && v.device_id == id);
            s.devices
                .iter_mut()
                .filter(|d| d.id == id && d.host_id == host)
                .for_each(|d| d.booted = false);
        } else {
            revoke_matching(&mut s, |v| {
                v.host_id == host && v.device_id == id && v.chat == q.chat
            });
        }
        svc.publish(&mut s);
    }
    Ok(Json(()))
}
async fn shutdown(State(app): State<Workbench>, Json(q): Json<Target>) -> Reply<Json<()>> {
    let host = q.host_id.as_deref().unwrap_or("local");
    let svc = &app.0.devices;
    let _operation = svc.lifecycle.lock().await;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, host, &q.device_id).await?;
    remote::at(d.ssh.as_ref(), native::shutdown(&d)).await?;
    revoke_matching(&mut s, |v| v.host_id == host && v.device_id == d.id);
    svc.refresh(&mut s).await;
    Ok(Json(()))
}
async fn detail(State(app): State<Workbench>, Query(q): Query<Target>) -> Reply<Json<Value>> {
    let host = q.host_id.as_deref().unwrap_or("local");
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, host, &q.device_id).await?;
    Ok(Json(remote::at(d.ssh.as_ref(), actions::detail(&d)).await?))
}
async fn action(State(app): State<Workbench>, Json(q): Json<Value>) -> Reply<Json<Value>> {
    let target: Target = serde_json::from_value(q.clone())
        .map_err(|_| Error::Refused("invalid device target".into()))?;
    let host = target.host_id.as_deref().unwrap_or("local");
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, host, &target.device_id).await?;
    remote::at(d.ssh.as_ref(), actions::apply(&d, &q)).await?;
    Ok(Json(remote::at(d.ssh.as_ref(), actions::detail(&d)).await?))
}
async fn screenshot(State(app): State<Workbench>, Query(q): Query<Target>) -> Reply<Response> {
    let host = q.host_id.as_deref().unwrap_or("local");
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, host, &q.device_id).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        remote::at(d.ssh.as_ref(), native::screenshot(&d)).await?,
    )
        .into_response())
}
async fn events(State(app): State<Workbench>) -> Response {
    let rx = app.0.devices.events.subscribe();
    use std::sync::atomic::Ordering;
    if !app.0.devices.polling.swap(true, Ordering::AcqRel) {
        let weak = std::sync::Arc::downgrade(&app.0);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let service = &inner.devices;
                if service.events.receiver_count() == 0 {
                    service.polling.store(false, Ordering::Release);
                    if service.events.receiver_count() == 0
                        || service.polling.swap(true, Ordering::AcqRel)
                    {
                        return;
                    }
                }
                let mut state = service.live.lock().await;
                if state.settings.enabled {
                    service.refresh(&mut state).await;
                }
            }
        });
    }
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
        .route("/devices/stream", get(screen::stream))
        .route("/devices/eas-account", post(account::eas))
        .route("/devices/simlock-identity", post(account::simlock))
}
