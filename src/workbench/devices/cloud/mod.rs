//! Bounded cloud device leases with private durable ownership before any billable creation.
mod expo;
pub(super) mod media;
mod simlock;
use super::*;
use reqwest::{Client, RequestBuilder, Response};
use std::{
    collections::BTreeSet,
    sync::{Mutex as StdMutex, Once},
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Provider {
    Simlock,
    #[serde(rename = "eas")]
    Expo,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Host {
    pub id: String,
    pub label: String,
    pub kind: Provider,
    pub gateway_url: Option<String>,
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub max_sessions: Option<u8>,
    pub max_minutes: Option<u16>,
    pub idle_minutes: Option<u16>,
}
impl Host {
    pub fn validate(&self) -> Result<()> {
        if let Some(token) = &self.token
            && !token.trim().is_empty()
            && reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim())).is_err()
        {
            return Err(bad("invalid device host credential"));
        }
        if self.id.is_empty()
            || self.id == "local"
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            || self.label.trim().is_empty()
            || !(1..=20).contains(&self.limit())
        {
            return Err(bad("invalid cloud device host or concurrency limit"));
        }
        match self.kind {
            Provider::Simlock => {
                let gateway = self
                    .gateway_url
                    .as_deref()
                    .ok_or_else(|| bad("missing Simlock gateway"))?;
                safe_url(gateway)?;
                if url::Url::parse(gateway).unwrap().path() != "/"
                    || !(5..=240).contains(&self.age())
                {
                    return Err(bad("invalid Simlock gateway or lease duration"));
                }
            }
            Provider::Expo => {
                if self
                    .app_id
                    .as_deref()
                    .is_some_and(|s| Uuid::parse_str(s).is_err())
                    || !(1..=115).contains(&self.age())
                    || !(1..=60).contains(&self.idle())
                    || self.idle() >= self.age()
                {
                    return Err(bad("invalid Expo app or session duration"));
                }
            }
        }
        Ok(())
    }
    fn limit(&self) -> u8 {
        self.max_sessions.unwrap_or(2)
    }
    fn age(&self) -> u16 {
        self.max_minutes
            .unwrap_or(if self.kind == Provider::Expo { 30 } else { 60 })
    }
    fn idle(&self) -> u16 {
        self.idle_minutes.unwrap_or(10)
    }
    fn gateway(&self) -> Result<&str> {
        self.gateway_url
            .as_deref()
            .map(|s| s.trim_end_matches('/'))
            .ok_or_else(|| bad("missing Simlock gateway"))
    }
    fn auth(&self) -> Result<Auth> {
        if let Some(v) = &self.token
            && !v.trim().is_empty()
        {
            return Ok(Auth {
                header: "authorization".into(),
                value: format!("Bearer {}", v.trim()),
            });
        }
        if self.kind == Provider::Expo {
            if let Ok(v) = std::env::var("EXPO_TOKEN")
                && !v.trim().is_empty()
            {
                return Ok(Auth {
                    header: "authorization".into(),
                    value: format!("Bearer {}", v.trim()),
                });
            }
            if let Some(s) = account::login() {
                return Ok(Auth {
                    header: "expo-session".into(),
                    value: s,
                });
            }
        } else if std::env::var("AGENT_DEVICE_DAEMON_BASE_URL")
            .ok()
            .is_some_and(|v| {
                v.trim_end_matches('/')
                    == format!("{}/v1/agent-device", self.gateway().unwrap_or_default())
            })
            && let Ok(v) = std::env::var("AGENT_DEVICE_DAEMON_AUTH_TOKEN")
            && !v.trim().is_empty()
        {
            return Ok(Auth {
                header: "authorization".into(),
                value: format!("Bearer {}", v.trim()),
            });
        }
        Err(bad("cloud device host has no matching credential"))
    }
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            Provider::Expo => "eas",
            Provider::Simlock => "simlock",
        }
    }
}
fn bad(msg: &str) -> Error {
    Error::Refused(msg.into())
}
fn safe_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).map_err(|_| bad("invalid cloud endpoint"))?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || (url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))))
    {
        return Err(bad("cloud endpoints require HTTPS or a loopback peer"));
    }
    Ok(url)
}
fn required(v: &Value, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| bad("cloud response missing required field"))
}
fn now() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}
fn segment(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}
#[derive(Clone, Serialize, Deserialize)]
struct Auth {
    header: String,
    value: String,
}
impl Auth {
    fn apply(&self, b: RequestBuilder) -> RequestBuilder {
        b.header(self.header.as_str(), &self.value)
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    key: Uuid,
    host: Host,
    auth: Auth,
    chat: Uuid,
    catalog: String,
    platform: String,
    name: String,
    version: String,
    created: i64,
    renew_at: i64,
    app: Option<String>,
    resource: Option<String>,
    device_key: Option<String>,
    agent_url: Option<String>,
    agent_token: Option<String>,
    preview: Option<String>,
    preview_token: Option<String>,
    remote_device: Option<String>,
    ready: bool,
    stopping: bool,
}
impl Lease {
    fn id(&self) -> &str {
        self.resource.as_deref().unwrap_or("")
    }
    fn session(&self) -> String {
        format!("kar-{}", self.key.simple())
    }
    fn device(&self) -> Device {
        Device::new(
            self.id(),
            if self.platform == "ios" {
                "ios"
            } else {
                "android"
            },
            &self.name,
            self.version.clone(),
            true,
            false,
        )
    }
}
pub(super) struct Cloud {
    media: Client,
    graph: String,
    path: PathBuf,
    db: StdMutex<BTreeMap<Uuid, Lease>>,
    startup: BTreeSet<Uuid>,
    started: Once,
    client: Client,
    // Admission spans a count and a durable reservation. Remote work is serialized per lease.
    gates: StdMutex<BTreeMap<Uuid, std::sync::Weak<Mutex<()>>>>,
    operations: Mutex<()>,
}
impl Cloud {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("device-cloud-leases.json");
        let db: BTreeMap<Uuid, Lease> = match File::open(&path) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => return Err(e.into()),
        };
        let startup = db.keys().copied().collect();
        Ok(Self {
            media: Client::builder()
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            graph: "https://api.expo.dev/graphql".into(),
            path,
            db: StdMutex::new(db),
            startup,
            started: Once::new(),
            gates: Default::default(),
            operations: Default::default(),
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(35))
                .build()?,
        })
    }
    fn gate(&self, id: Uuid) -> std::sync::Arc<Mutex<()>> {
        let mut gates = self.gates.lock().unwrap();
        if let Some(g) = gates.get(&id).and_then(|g| g.upgrade()) {
            return g;
        }
        gates.retain(|_, w| w.strong_count() > 0);
        let g = std::sync::Arc::new(Mutex::new(()));
        gates.insert(id, std::sync::Arc::downgrade(&g));
        g
    }
    fn record(&self, id: Uuid) -> Option<Lease> {
        self.db.lock().unwrap().get(&id).cloned()
    }
    fn save(&self, record: &Lease) -> Result<()> {
        self.update(|db| {
            db.insert(record.key, record.clone());
        })
    }
    fn forget(&self, key: Uuid) -> Result<()> {
        self.update(|db| {
            db.remove(&key);
        })
    }
    fn update(&self, op: impl FnOnce(&mut BTreeMap<Uuid, Lease>)) -> Result<()> {
        let mut owner = self.db.lock().unwrap();
        let mut db = owner.clone();
        op(&mut db);
        let dir = self.path.parent().expect("cloud state");
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut f, &db)?;
        f.as_file().sync_all()?;
        f.persist(&self.path).map_err(|e| e.error)?;
        *owner = db;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    }
    pub async fn list(&self, host: &Host) -> Result<Vec<Device>> {
        let mut list = match host.kind {
            Provider::Simlock => simlock::catalog(&self.client, host).await?,
            Provider::Expo => expo::catalog(),
        };
        list.extend(
            self.db
                .lock()
                .unwrap()
                .values()
                .filter(|l| l.host == *host && l.ready && !l.stopping)
                .map(Lease::device),
        );
        for d in &mut list {
            d.host_id = host.id.clone();
            d.cloud = true;
        }
        Ok(list)
    }
    pub async fn boot(&self, host: &Host, d: &Device, chat: Uuid, cwd: &Path) -> Result<String> {
        let _lock = self.operations.lock().await;
        if d.booted {
            return Ok(d.id.clone());
        }
        let auth = host.auth()?;
        let app = if host.kind == Provider::Expo {
            Some(expo::project(cwd, host)?)
        } else {
            None
        };
        let mut lease = {
            let ledger = self.db.lock().unwrap();
            if let Some(old) = ledger
                .values()
                .find(|l| l.host.id == host.id && l.chat == chat && l.catalog == d.id)
            {
                if old.ready && !old.stopping && old.host == *host {
                    return Ok(old.id().into());
                }
                return Err(bad("a previous cloud opening is being reconciled"));
            }
            if ledger.values().filter(|l| l.host.id == host.id).count() >= usize::from(host.limit())
            {
                return Err(bad("cloud host session limit reached"));
            }
            Lease {
                key: Uuid::new_v4(),
                host: host.clone(),
                auth,
                chat,
                catalog: d.id.clone(),
                platform: d.platform.into(),
                name: d.name.clone(),
                version: d.version.clone(),
                created: now(),
                renew_at: now() + 120_000,
                app,
                resource: None,
                device_key: None,
                agent_url: None,
                agent_token: None,
                preview: None,
                preview_token: None,
                remote_device: None,
                ready: false,
                stopping: false,
            }
        };
        let gate = self.gate(lease.key);
        let _opening = gate.lock().await;
        self.save(&lease)?;
        drop(_lock);
        let result = match host.kind {
            Provider::Simlock => simlock::start(self, &mut lease).await,
            Provider::Expo => expo::start(self, &mut lease).await,
        };
        if let Err(e) = result {
            lease.stopping = true;
            self.save(&lease)?;
            let _ = self.release(&mut lease).await;
            return Err(e);
        }
        lease.ready = true;
        self.save(&lease)?;
        Ok(lease.id().into())
    }
    pub fn owned(&self, host: &Host, id: &str) -> Option<Device> {
        let l = self.find(&host.id, id).ok()?;
        if l.host != *host || !l.ready || l.stopping {
            return None;
        }
        let mut d = l.device();
        d.cloud = true;
        d.host_id = l.host.id;
        Some(d)
    }
    fn find(&self, host: &str, id: &str) -> Result<Lease> {
        self.db
            .lock()
            .unwrap()
            .values()
            .find(|l| l.host.id == host && l.resource.as_deref() == Some(id))
            .cloned()
            .ok_or_else(|| bad("cloud lease is not owned by this host"))
    }
    async fn release(&self, l: &mut Lease) -> Result<()> {
        l.stopping = true;
        self.save(l)?;
        let done = match l.host.kind {
            Provider::Expo => expo::stop(self, l).await?,
            Provider::Simlock => simlock::stop(self, l).await?,
        };
        if done {
            self.forget(l.key)?;
            Ok(())
        } else {
            Err(bad("cloud release is awaiting confirmation"))
        }
    }
    async fn release_key(&self, key: Uuid) -> Result<()> {
        let gate = self.gate(key);
        let _lock = gate.lock().await;
        if let Some(mut lease) = self.record(key) {
            self.release(&mut lease).await
        } else {
            Ok(())
        }
    }
    pub async fn shutdown(&self, host: &str, id: &str) -> Result<()> {
        self.release_key(self.find(host, id)?.key).await
    }
    pub fn keys_for_hosts(&self, hosts: &[String]) -> Vec<Uuid> {
        self.db
            .lock()
            .unwrap()
            .values()
            .filter(|l| hosts.contains(&l.host.id))
            .map(|l| l.key)
            .collect()
    }
    pub async fn release_keys(&self, list: Vec<Uuid>) -> Result<()> {
        let mut failure = None;
        for id in list {
            if let Err(e) = self.release_key(id).await {
                failure = Some(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub async fn screenshot(&self, d: &Device) -> Result<Vec<u8>> {
        let l = self.find(&d.host_id, &d.id)?;
        if !l.ready || l.stopping {
            return Err(bad("cloud device is inactive"));
        }
        match l.host.kind {
            Provider::Expo => expo::screen(&self.client, &l).await,
            Provider::Simlock => simlock::screen(&self.client, &l).await,
        }
    }
    pub fn maintain(&self, app: &Workbench) {
        self.started.call_once(|| {
            let weak = std::sync::Arc::downgrade(&app.0);
            tokio::spawn(async move {
                loop {
                    use futures_util::StreamExt;
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    let cloud = &inner.devices.cloud;
                    let keys = cloud.db.lock().unwrap().keys().copied().collect::<Vec<_>>();
                    let removed = futures_util::stream::iter(keys)
                        .map(|key| async move {
                            let gate = cloud.gate(key);
                            let _guard = gate.try_lock().ok()?;
                            let mut l = cloud.record(key)?;
                            if cloud.startup.contains(&key)
                                || l.stopping
                                || !l.ready
                                || now() >= l.created + i64::from(l.host.age()) * 60_000
                            {
                                cloud.release(&mut l).await.ok()?;
                            } else if now() >= l.renew_at {
                                let alive = match l.host.kind {
                                    Provider::Simlock => simlock::renew(&cloud.client, &l).await,
                                    Provider::Expo => expo::alive(cloud, &l).await,
                                }
                                .ok()?;
                                if alive {
                                    l.renew_at = now()
                                        + if l.host.kind == Provider::Expo {
                                            60_000
                                        } else {
                                            120_000
                                        };
                                    let _ = cloud.save(&l);
                                    return None;
                                }
                                cloud.forget(key).ok()?;
                            } else {
                                return None;
                            }
                            Some((l.host.id.clone(), l.id().to_owned()))
                        })
                        .buffer_unordered(8)
                        .filter_map(std::future::ready)
                        .collect::<Vec<_>>()
                        .await;
                    if !removed.is_empty() {
                        let mut live = inner.devices.live.lock().await;
                        revoke_matching(&mut live, |v| {
                            removed
                                .iter()
                                .any(|(h, id)| *h == v.host_id && *id == v.device_id)
                        });
                        live.devices.retain(|d| {
                            !removed.iter().any(|(h, id)| *h == d.host_id && *id == d.id)
                        });
                        inner.devices.publish(&mut live);
                    }
                    drop(inner);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            });
        });
    }
}
async fn bounded(mut response: Response, max: usize) -> Result<Vec<u8>> {
    let mut result = vec![];
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| bad("incomplete cloud response"))?
    {
        if result.len() + chunk.len() > max {
            return Err(bad("cloud response exceeds size limit"));
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}
async fn json_response(request: RequestBuilder) -> Result<Value> {
    let response = request
        .send()
        .await
        .map_err(|_| bad("cloud request did not return a confirmed response"))?;
    if !response.status().is_success() {
        return Err(bad(&format!(
            "cloud host returned HTTP {}",
            response.status().as_u16()
        )));
    }
    serde_json::from_slice(&bounded(response, 2 * 1024 * 1024).await?)
        .map_err(|_| bad("invalid cloud response"))
}
async fn png(request: RequestBuilder) -> Result<Vec<u8>> {
    let response = request
        .send()
        .await
        .map_err(|_| bad("cloud screenshot unavailable"))?;
    if !response.status().is_success() {
        return Err(bad("cloud host rejected the screenshot"));
    }
    let data = bounded(response, 32 * 1024 * 1024).await?;
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") || !data.ends_with(b"IEND\xaeB\x60\x82") {
        return Err(bad("cloud host did not return a complete PNG"));
    }
    Ok(data)
}
