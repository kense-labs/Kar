//! Host-local device discovery, controls, and per-chat claims.
mod account;
mod actions;
mod agent;
mod cloud;
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
    cloud_hosts: Vec<cloud::Host>,
    agent_access_enabled: bool,
    onboarding_completed: bool,
}
#[derive(Clone, Serialize)]
struct Device {
    host_id: String,
    #[serde(skip)]
    ssh: Option<SshHost>,
    #[serde(skip)]
    cloud: bool,
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
            cloud: false,
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
#[derive(Clone)]
enum Backend {
    Native(Option<SshHost>),
    Cloud(cloud::Host),
}
impl Backend {
    fn id(&self) -> &str {
        match self {
            Self::Native(h) => h.as_ref().map_or("local", |h| h.id.as_str()),
            Self::Cloud(h) => &h.id,
        }
    }
}
impl Live {
    fn snapshot(&self) -> Value {
        let hosts:Vec<_>=std::iter::once(("local","local","This machine")).chain(self.settings.ssh_hosts.iter().map(|h|(h.id.as_str(),"ssh",h.label.as_str()))).chain(self.settings.cloud_hosts.iter().map(|h|(h.id.as_str(),h.kind_name(),h.label.as_str()))).map(|(id,kind,label)|{
            let platforms:Vec<_>=["ios","android"].into_iter().map(|p|{
                let k=format!("{id}/{p}");let mut v=json!({"platform":p,"available":self.inspected && !self.errors.contains_key(&k)});
                if let Some(e)=self.errors.get(&k){v["reason"]=json!(e);}v
            }).collect();
            let status=if !self.settings.enabled{"disabled"}else if !self.inspected{"idle"}else if platforms.iter().all(|p|p["available"]==false){"failed"}else{"ready"};
            json!({"id":id,"kind":kind,"label":label,"platforms":platforms,"status":status})
        }).collect();
        let mut settings = serde_json::to_value(&self.settings).expect("device settings");
        for host in settings["cloud_hosts"].as_array_mut().into_iter().flatten() {
            if let Some(row) = host.as_object_mut() {
                let configured = row
                    .remove("token")
                    .is_some_and(|t| t.as_str().is_some_and(|t| !t.trim().is_empty()));
                row.insert("has_credential".into(), json!(configured));
            }
        }
        json!({"settings":settings,"revision":self.revision,"host_status":hosts[0]["status"],"hosts":hosts,"incomplete_platforms":self.errors.keys().collect::<Vec<_>>(),"devices":self.devices,"sessions":self.sessions,"booting_devices":self.booting})
    }
    fn backend(&self, id: &str) -> Result<Backend> {
        if let Some(host) = self.settings.cloud_hosts.iter().find(|h| h.id == id) {
            return Ok(Backend::Cloud(host.clone()));
        }
        if id == "local" {
            return Ok(Backend::Native(None));
        }
        self.settings
            .ssh_hosts
            .iter()
            .find(|v| v.id == id)
            .map(|h| Backend::Native(Some(h.clone())))
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
    cloud: cloud::Cloud,
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
            cloud: cloud::Cloud::open(path)?,
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
        let mut hosts = vec![Backend::Native(None)];
        hosts.extend(
            s.settings
                .ssh_hosts
                .iter()
                .cloned()
                .map(|h| Backend::Native(Some(h))),
        );
        hosts.extend(s.settings.cloud_hosts.iter().cloned().map(Backend::Cloud));
        self.refresh_hosts(s, hosts).await
    }
    async fn refresh_hosts(&self, s: &mut Live, hosts: Vec<Backend>) -> Value {
        use futures_util::StreamExt;
        let before = s.snapshot();
        let mut work = futures_util::stream::iter(hosts)
            .map(|host| async move {
                let timeout = std::time::Duration::from_secs(20);
                let Backend::Native(ssh) = &host else {
                    let Backend::Cloud(config) = &host else {
                        unreachable!()
                    };
                    return match self.cloud.list(config).await {
                        Ok(rows) => {
                            let (ios, android) =
                                rows.into_iter().partition(|d| d.platform == "ios");
                            (host, Ok(ios), Ok(android))
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            (
                                host,
                                Err(Error::Refused(msg.clone())),
                                Err(Error::Refused(msg)),
                            )
                        }
                    };
                };
                let (ios, android) = remote::at(ssh.as_ref(), async {
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
            let id = host.id();
            for (platform, result) in [("ios", ios), ("android", android)] {
                let key = format!("{id}/{platform}");
                match result {
                    Ok(mut rows) => {
                        s.errors.remove(&key);
                        s.devices
                            .retain(|d| d.host_id != id || d.platform != platform);
                        for row in &mut rows {
                            row.host_id = id.into();
                            if let Backend::Native(ssh) = &host {
                                row.ssh = ssh.clone();
                            }
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
        let selected = s.backend(host)?;
        if let Backend::Cloud(h) = &selected
            && let Some(d) = self.cloud.owned(h, id)
        {
            if !s
                .devices
                .iter()
                .any(|row| row.host_id == host && row.id == id && row.booted)
            {
                s.devices.retain(|row| row.host_id != host || row.id != id);
                s.devices.push(d.clone());
                self.publish(s);
            }
            return Ok(d);
        }
        self.refresh_hosts(s, vec![selected]).await;
        let d = s.device(host, id)?;
        if let Some(cause) = s.errors.get(&format!("{host}/{}", d.platform)) {
            return Err(Error::Refused(cause.clone()));
        }
        Ok(d)
    }
    pub(super) fn maintain(&self, app: &Workbench) {
        self.cloud.maintain(app);
    }
    async fn capture(&self, d: &Device) -> Result<Vec<u8>> {
        if d.cloud {
            self.cloud.screenshot(d).await
        } else {
            remote::at(d.ssh.as_ref(), native::screenshot(d)).await
        }
    }
    async fn shutdown_device(&self, d: &Device) -> Result<()> {
        if d.cloud {
            self.cloud.shutdown(&d.host_id, &d.id).await
        } else {
            remote::at(d.ssh.as_ref(), native::shutdown(d)).await
        }
    }
    async fn detail(&self, d: &Device) -> Result<Value> {
        if d.cloud {
            Ok(
                json!({"host_id":d.host_id,"device_id":d.id,"settings":{},"foreground_app":null,"read_at":jiff::Timestamp::now().to_string()}),
            )
        } else {
            remote::at(d.ssh.as_ref(), actions::detail(d)).await
        }
    }
    async fn apply(&self, d: &Device, q: &Value) -> Result<()> {
        if d.cloud {
            return Err(Error::Unsupported(
                "this cloud host does not expose simulator shell controls".into(),
            ));
        }
        remote::at(d.ssh.as_ref(), actions::apply(d, q)).await
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
    cloud_hosts: Option<Vec<cloud::Host>>,
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
    if let Some(mut hosts) = q.cloud_hosts {
        for h in &mut hosts {
            h.validate()?;
            if h.token.is_none()
                && let Some(old) = s.settings.cloud_hosts.iter().find(|old| {
                    old.id == h.id && old.kind == h.kind && old.gateway_url == h.gateway_url
                })
            {
                h.token = old.token.clone();
            }
        }
        next.cloud_hosts = hosts;
    }
    let mut ids = std::collections::BTreeSet::new();
    if next
        .ssh_hosts
        .iter()
        .map(|h| &h.id)
        .chain(next.cloud_hosts.iter().map(|h| &h.id))
        .any(|id| !ids.insert(id))
    {
        return Err(Error::Refused("duplicate device host id".into()).into());
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
    let retired = s
        .settings
        .cloud_hosts
        .iter()
        .filter(|h| !next.enabled || !next.cloud_hosts.contains(h))
        .map(|h| h.id.clone())
        .collect::<Vec<_>>();
    let retired_resources = svc.cloud.keys_for_hosts(&retired);
    let mut changed: Vec<_> = s
        .settings
        .ssh_hosts
        .iter()
        .filter(|h| !next.ssh_hosts.contains(h))
        .map(|h| h.id.clone())
        .collect();
    changed.extend(retired.iter().cloned());
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
    let state = if s.settings.enabled {
        svc.refresh(&mut s).await
    } else {
        state
    };
    drop(s);
    if !retired.is_empty() {
        svc.cloud.release_keys(retired_resources).await?;
    }
    Ok(Json(state))
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
    let selected = s.backend(host_id)?;
    if q.inspect_only == Some(true) {
        let Backend::Native(ssh) = &selected else {
            return Ok(Json(s.snapshot()));
        };
        let (xcode, adb, emulator) = remote::at(ssh.as_ref(), async {
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
    let cwd = app.binding(q.project, q.chat)?.workspace;
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
        let backend = s.backend(host)?;
        drop(s);
        let boot = match &backend {
            Backend::Native(ssh) => remote::at(ssh.as_ref(), native::boot(&device)).await,
            Backend::Cloud(h) => svc.cloud.boot(h, &device, q.chat, &cwd).await,
        };
        s = svc.live.lock().await;
        let valid = s
            .booting
            .iter()
            .any(|v| v["attempt"] == attempt.to_string());
        s.booting.retain(|v| v["attempt"] != attempt.to_string());
        svc.publish(&mut s);
        if !valid || !s.settings.enabled {
            drop(s);
            if matches!(backend, Backend::Cloud(_))
                && let Ok(id) = boot
            {
                svc.cloud.shutdown(host, &id).await?;
            }
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
        s.backend(host)?;
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
            svc.shutdown_device(&d).await?;
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
    svc.shutdown_device(&d).await?;
    revoke_matching(&mut s, |v| v.host_id == host && v.device_id == d.id);
    svc.refresh(&mut s).await;
    Ok(Json(()))
}
async fn detail(State(app): State<Workbench>, Query(q): Query<Target>) -> Reply<Json<Value>> {
    let host = q.host_id.as_deref().unwrap_or("local");
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, host, &q.device_id).await?;
    Ok(Json(svc.detail(&d).await?))
}
async fn action(State(app): State<Workbench>, Json(q): Json<Value>) -> Reply<Json<Value>> {
    let target: Target = serde_json::from_value(q.clone())
        .map_err(|_| Error::Refused("invalid device target".into()))?;
    let host = target.host_id.as_deref().unwrap_or("local");
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, host, &target.device_id).await?;
    svc.apply(&d, &q).await?;
    Ok(Json(svc.detail(&d).await?))
}
async fn screenshot(State(app): State<Workbench>, Query(q): Query<Target>) -> Reply<Response> {
    let host = q.host_id.as_deref().unwrap_or("local");
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let d = svc.resolve(&mut s, host, &q.device_id).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        svc.capture(&d).await?,
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
        .route("/devices/video", get(cloud::media::video))
        .route("/devices/input", get(cloud::media::input))
        .route("/devices/eas-account", post(account::eas))
        .route("/devices/simlock-identity", post(account::simlock))
}
