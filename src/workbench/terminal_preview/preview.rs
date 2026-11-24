use super::events::Events;
use super::{Port, ProcessId, Status, Terminal, browser::Display, process};
use crate::{Error, Result, workbench::access::DeviceLease};
use axum::{
    Router,
    body::Body,
    extract::{
        Request, State,
        ws::{Message, WebSocket},
    },
    http::{HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    time::{Instant, timeout},
};
use uuid::Uuid;

#[derive(Clone, Serialize)]
pub(super) struct Preview {
    pub runtime: &'static str,
    pub id: Uuid,
    pub project: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Port>,
    pub url: String,
    pub profile_id: String,
    #[serde(flatten)]
    pub display: Display,
    #[serde(flatten)]
    navigation: Navigation,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub(super) struct Navigation {
    pub nav_status: Value,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}
impl Default for Navigation {
    fn default() -> Self {
        Self {
            nav_status: json!({"_tag":"Idle"}),
            can_go_back: false,
            can_go_forward: false,
        }
    }
}
impl Navigation {
    pub fn validate(&self) -> Result<()> {
        let v = &self.nav_status;
        let valid = match v["_tag"].as_str() {
            Some("Idle") => true,
            Some(tag @ ("Loading" | "Success" | "LoadFailed")) => {
                v["url"].as_str().is_some_and(|s| {
                    !s.trim().is_empty() && s == s.trim() && s.chars().count() <= 2048
                }) && v["title"]
                    .as_str()
                    .is_some_and(|s| s.chars().count() <= 512)
                    && (tag != "LoadFailed" || (v["code"].is_i64() && v["description"].is_string()))
            }
            _ => false,
        };
        if !valid {
            return Err(Error::Refused("invalid preview navigation status".into()));
        }
        Ok(())
    }
}
struct Public {
    source: reqwest::Url,
    origins: std::collections::BTreeSet<String>,
    authorized: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}
pub(super) struct Grant {
    pub info: Preview,
    display: Mutex<Display>,
    navigation: Mutex<(Navigation, bool)>,
    events: Arc<Events>,
    publishing: Mutex<()>,
    device: Option<DeviceLease>,
    terminal: Weak<Terminal>,
    root: Option<ProcessId>,
    public: Option<Public>,
    authority: String,
    stop: watch::Sender<bool>,
}
async fn stopped(mut rx: watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    let _ = rx.wait_for(|v| *v).await;
}

impl Grant {
    fn revoke(&self) {
        let _publishing = self.publishing.lock().unwrap();
        if !self.stop.send_replace(true) {
            self.events.push(
                self.info.project,
                self.owner(),
                json!({"type":"closed","id":self.info.id}),
            );
        }
    }
    fn active(&self) -> bool {
        !*self.stop.borrow()
            && self.device.as_ref().is_none_or(DeviceLease::active)
            && self
                .public
                .as_ref()
                .is_none_or(|p| (p.authorized)().is_ok())
    }
    fn belongs_to(&self, project: Uuid, device: Option<Uuid>) -> bool {
        self.info.project == project
            && device.is_none_or(|id| self.device.as_ref().is_some_and(|d| d.id == id))
    }
    pub fn snapshot(&self) -> Preview {
        Preview {
            display: self.display(),
            navigation: self.navigation.lock().unwrap().0.clone(),
            ..self.info.clone()
        }
    }
    pub fn display(&self) -> Display {
        self.display.lock().unwrap().clone()
    }
    pub fn set_display(&self, value: Display, request: Value) {
        let _publishing = self.publishing.lock().unwrap();
        if !self.active() {
            return;
        }
        *self.display.lock().unwrap() = value;
        self.events.push(
            self.info.project,
            self.owner(),
            json!({"type":"resized","snapshot":self.snapshot(),"request":request}),
        );
    }
    pub fn mark_managed(&self) {
        self.navigation.lock().unwrap().1 = true;
    }
    fn report(&self, nav: Navigation, external: bool) -> Result<()> {
        let _publishing = self.publishing.lock().unwrap();
        if !self.active() {
            return Err(super::missing());
        }
        let mut current = self.navigation.lock().unwrap();
        if external && current.1 {
            return Err(crate::workbench::denied());
        }
        if current.0 == nav {
            return Ok(());
        }
        let failed = nav.nav_status["_tag"] == "LoadFailed";
        current.0 = nav;
        drop(current);
        self.events.push(
            self.info.project,
            self.owner(),
            json!({"type":if failed{"failed"}else{"navigated"},"snapshot":self.snapshot()}),
        );
        Ok(())
    }
    pub fn report_status(&self, nav: Navigation) -> Result<()> {
        nav.validate()?;
        self.report(nav, true)
    }
    pub fn native_status(&self, status: Value, back: bool, forward: bool) {
        let _ = self.report(
            Navigation {
                nav_status: status,
                can_go_back: back,
                can_go_forward: forward,
            },
            false,
        );
    }
    pub fn authority(&self) -> &str {
        &self.authority
    }
    pub fn owner(&self) -> Option<Uuid> {
        self.device.as_ref().map(|d| d.id)
    }
    pub fn revocation(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }
    pub async fn visible(&self) -> Result<bool> {
        if self.public.is_some() {
            return Ok(self.active());
        }
        if !self.active()
            || !self
                .terminal
                .upgrade()
                .is_some_and(|t| matches!(t.snapshot().status, Status::Running))
        {
            return Ok(false);
        }
        let root = self.root.ok_or_else(super::missing)?;
        let target = self.info.target.ok_or_else(super::missing)?;
        tokio::task::spawn_blocking(move || {
            process::ports(root).map(|ports| ports.contains(&target))
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
    pub async fn connect(&self) -> Result<TcpStream> {
        let make = async {
            if !self.visible().await? {
                return Err(Error::Closed("preview listener stopped".into()));
            }
            let root = self.root.ok_or_else(super::missing)?;
            let target = self.info.target.ok_or_else(super::missing)?;
            let s = TcpStream::connect(SocketAddr::new(target.address, target.port)).await?;
            let client = s.local_addr()?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let confirmed =
                    tokio::task::spawn_blocking(move || process::owns_peer(root, target, client))
                        .await
                        .map_err(|e| Error::Closed(e.to_string()))??;
                if confirmed {
                    return Ok(s);
                }
                if Instant::now() >= deadline {
                    return Err(Error::Refused(
                        "unable to confirm the preview connection belongs to the terminal".into(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        };
        tokio::select! {_ = stopped(self.stop.subscribe())=>Err(Error::Closed("preview revoked".into())),r=timeout(Duration::from_secs(5),make)=>r.map_err(|_|Error::Timeout("preview connection".into()))?}
    }
    pub async fn websocket(self: Arc<Self>, socket: WebSocket) {
        let Ok(stream) = self.connect().await else {
            return;
        };
        let (mut sends, mut receives) = socket.split();
        let (mut read, mut write) = stream.into_split();
        let from_client = async {
            while let Some(Ok(frame)) = receives.next().await {
                match frame {
                    Message::Binary(bytes) => write.write_all(&bytes).await?,
                    Message::Ping(_) | Message::Pong(_) => continue,
                    _ => break,
                }
            }
            std::io::Result::Ok(())
        };
        let to_client = async {
            let mut buf = [0u8; 16384];
            loop {
                let n = read.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                sends
                    .send(Message::Binary(buf[..n].to_vec().into()))
                    .await
                    .map_err(std::io::Error::other)?;
            }
            std::io::Result::Ok(())
        };
        tokio::select! {_=stopped(self.stop.subscribe())=>{},_=from_client=>{},_=to_client=>{}}
    }
}

#[derive(Default)]
pub(super) struct Grants(
    Mutex<HashMap<Uuid, Arc<Grant>>>,
    pub Arc<Events>,
    Mutex<HashMap<(Uuid, Uuid), Instant>>,
);
impl Grants {
    pub async fn open(
        &self,
        project: Uuid,
        chat: Option<Uuid>,
        term: Arc<Terminal>,
        target: Port,
        device: Option<DeviceLease>,
        profile_id: String,
        display: Display,
    ) -> Result<Preview> {
        let m = term.snapshot();
        let root = m
            .process
            .ok_or_else(|| Error::Closed("terminal process unavailable".into()))?;
        let permitted = tokio::task::spawn_blocking(move || process::ports(root))
            .await
            .map_err(|e| Error::Closed(e.to_string()))??;
        if m.project != project
            || !matches!(m.status, Status::Running)
            || !permitted.contains(&target)
        {
            return Err(Error::Refused("not a project terminal listener".into()));
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let id = Uuid::new_v4();
        // Keep the developer page on a separate, unguessable loopback origin.
        // The grant authority is independent of the workbench bearer credential.
        let authority = format!(
            "{}.localhost:{}",
            Uuid::new_v4().simple(),
            listener.local_addr()?.port()
        );
        let info = Preview {
            runtime: "server",
            id,
            project,
            terminal: Some(m.id),
            public_origin: None,
            chat,
            target: Some(target),
            url: format!("http://{authority}/"),
            profile_id,
            display: display.clone(),
            navigation: Navigation::default(),
        };
        let grant = Arc::new(Grant {
            info: info.clone(),
            display: Mutex::new(display),
            navigation: Mutex::new((Navigation::default(), false)),
            events: self.1.clone(),
            publishing: Mutex::new(()),
            device: device.clone(),
            terminal: Arc::downgrade(&term),
            root: Some(root),
            public: None,
            authority,
            stop: watch::channel(false).0,
        });
        self.0.lock().unwrap().retain(|_, g| g.active());
        self.0.lock().unwrap().insert(id, grant.clone());
        self.1.push(
            project,
            grant.owner(),
            json!({"type":"opened","snapshot":info}),
        );
        let app = Router::new().fallback(proxy).with_state(grant.clone());
        let stop = grant.stop.subscribe();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(stopped(stop))
                .await;
        });
        let stop = grant.stop.subscribe();
        tokio::spawn(async move {
            let polling = async {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if !grant.visible().await.unwrap_or(false) {
                        grant.revoke();
                        break;
                    }
                }
            };
            let revoked = async {
                match device {
                    Some(device) => device.revoked().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {_=polling=>{},_=stopped(stop)=>{},_=revoked=>grant.revoke()}
        });
        Ok(info)
    }
    pub async fn open_public(
        &self,
        project: Uuid,
        chat: Uuid,
        source: reqwest::Url,
        origins: std::collections::BTreeSet<String>,
        authorized: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> Result<Arc<Grant>> {
        use crate::workbench::public_web;
        public_web::authorize(&origins, &source)?;
        public_web::addresses(&source).await?;
        authorized()?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let id = Uuid::new_v4();
        let authority = format!(
            "{}.localhost:{}",
            Uuid::new_v4().simple(),
            listener.local_addr()?.port()
        );
        let mut local = reqwest::Url::parse(&format!("http://{authority}")).expect("local URL");
        local.set_path(source.path());
        local.set_query(source.query());
        local.set_fragment(source.fragment());
        let display = Display::default();
        let info = Preview {
            runtime: "server",
            id,
            project,
            terminal: None,
            chat: Some(chat),
            target: None,
            public_origin: Some(source.origin().ascii_serialization()),
            url: local.to_string(),
            profile_id: "incognito".into(),
            display: display.clone(),
            navigation: Navigation::default(),
        };
        let grant = Arc::new(Grant {
            info: info.clone(),
            display: Mutex::new(display),
            navigation: Mutex::new((Navigation::default(), false)),
            events: self.1.clone(),
            publishing: Mutex::new(()),
            device: None,
            terminal: Weak::new(),
            root: None,
            public: Some(Public {
                source,
                origins,
                authorized,
            }),
            authority,
            stop: watch::channel(false).0,
        });
        self.0.lock().unwrap().insert(id, grant.clone());
        self.1
            .push(project, None, json!({"type":"opened","snapshot":info}));
        let app = Router::new().fallback(proxy).with_state(grant.clone());
        let stop = grant.stop.subscribe();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(stopped(stop))
                .await;
        });
        let weak = Arc::downgrade(&grant);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(grant) = weak.upgrade() else {
                    return;
                };
                if !grant.active() {
                    grant.revoke();
                    return;
                }
            }
        });
        Ok(grant)
    }
    pub fn report_desktop(
        &self,
        project: Uuid,
        chat: Uuid,
        device: DeviceLease,
        id: Uuid,
        nav: Navigation,
        display: Display,
    ) -> Result<Preview> {
        nav.validate()?;
        display.validate()?;
        let mut all = self.0.lock().unwrap();
        let mut closed = self.2.lock().unwrap();
        closed.retain(|_, at| at.elapsed() < Duration::from_secs(3600));
        if closed.contains_key(&(device.id, id)) || !device.active() {
            return Err(super::missing());
        }
        if let Some(g) = all.get(&id) {
            if g.info.runtime != "desktop"
                || g.info.project != project
                || g.info.chat != Some(chat)
                || g.owner() != Some(device.id)
            {
                return Err(super::missing());
            }
            g.report_status(nav)?;
            if g.display() != display {
                g.set_display(display, json!({"reported":true}));
            }
            return Ok(g.snapshot());
        }
        let url = nav.nav_status["url"]
            .as_str()
            .unwrap_or("about:blank")
            .to_owned();
        let info = Preview {
            runtime: "desktop",
            id,
            project,
            terminal: None,
            public_origin: None,
            chat: Some(chat),
            target: None,
            url,
            profile_id: "desktop".into(),
            display: display.clone(),
            navigation: nav.clone(),
        };
        let grant = Arc::new(Grant {
            info: info.clone(),
            display: Mutex::new(display),
            navigation: Mutex::new((nav, false)),
            events: self.1.clone(),
            publishing: Mutex::new(()),
            device: Some(device.clone()),
            terminal: Weak::new(),
            root: None,
            public: None,
            authority: String::new(),
            stop: watch::channel(false).0,
        });
        all.insert(id, grant.clone());
        self.1.push(
            project,
            Some(device.id),
            json!({"type":"opened","snapshot":info}),
        );
        tokio::spawn(async move {
            let mut stop = grant.revocation();
            tokio::select! {_=device.revoked()=>grant.revoke(),_=stop.changed()=>{}}
        });
        Ok(info)
    }
    pub fn get(&self, project: Uuid, id: Uuid, device: Option<Uuid>) -> Result<Arc<Grant>> {
        self.0
            .lock()
            .unwrap()
            .get(&id)
            .filter(|g| g.belongs_to(project, device) && g.active())
            .cloned()
            .ok_or_else(super::missing)
    }
    pub fn list(&self, project: Uuid, device: Option<Uuid>) -> Vec<Preview> {
        self.0
            .lock()
            .unwrap()
            .values()
            .filter(|g| g.belongs_to(project, device) && g.active())
            .map(|g| g.snapshot())
            .collect()
    }
    pub fn close(&self, project: Uuid, id: Uuid, device: Option<Uuid>) -> Result<()> {
        let mut all = self.0.lock().unwrap();
        if all.get(&id).is_none_or(|g| !g.belongs_to(project, device)) {
            return Err(super::missing());
        }
        let grant = all.remove(&id).unwrap();
        if grant.info.runtime == "desktop"
            && let Some(owner) = grant.owner()
        {
            self.2.lock().unwrap().insert((owner, id), Instant::now());
        }
        grant.revoke();
        Ok(())
    }
}
impl Drop for Grants {
    fn drop(&mut self) {
        for grant in self.0.get_mut().unwrap().values() {
            grant.revoke();
        }
    }
}

pub(super) async fn proxy(State(grant): State<Arc<Grant>>, mut req: Request) -> Response {
    if req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        != Some(&grant.authority)
        || req.uri().authority().is_some()
        || req.method() == Method::CONNECT
        || req.method() == Method::TRACE
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(public) = &grant.public {
        if !grant.active()
            || !matches!(*req.method(), Method::GET | Method::HEAD)
            || req.headers().contains_key(header::UPGRADE)
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        let mut upstream = public.source.clone();
        upstream.set_path(req.uri().path());
        upstream.set_query(req.uri().query());
        upstream.set_fragment(None);
        let reply =
            crate::workbench::public_web::get(&public.origins, upstream, 25 * 1024 * 1024).await;
        let Ok(mut resource) = reply else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        // A redirect must not make relative links point to a different remote origin under this tab's grant.
        if resource.url.origin() != public.source.origin() {
            return StatusCode::FORBIDDEN.into_response();
        }
        if resource.mime.starts_with("text/html") {
            let html = String::from_utf8_lossy(&resource.bytes).into_owned();
            match crate::workbench::public_web::bundle(&public.origins, html).await {
                Ok(page) => resource.bytes = page.into_bytes(),
                Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        if !grant.active() {
            return StatusCode::FORBIDDEN.into_response();
        }
        let bytes = if req.method() == Method::HEAD {
            Vec::new()
        } else {
            resource.bytes
        };
        return (resource.status,[(header::CONTENT_TYPE,resource.mime.as_str()),(header::REFERRER_POLICY,"no-referrer"),(header::X_CONTENT_TYPE_OPTIONS,"nosniff"),(header::CACHE_CONTROL,"private, no-store"),(header::CONTENT_SECURITY_POLICY,"default-src 'self' data:; script-src 'self' 'unsafe-inline' data:; style-src 'self' 'unsafe-inline' data:; form-action 'none'; frame-ancestors 'none'; base-uri 'self'")],bytes).into_response();
    }
    let Ok(socket) = grant.connect().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok((mut sender, connection)) =
        hyper::client::conn::http1::handshake(TokioIo::new(socket)).await
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let stop = grant.stop.subscribe();
    tokio::spawn(async move {
        tokio::select! {_=connection.with_upgrades()=>{},_=stopped(stop)=>{}}
    });
    let websocket = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let front = websocket.then(|| hyper::upgrade::on(&mut req));
    let Some(target) = grant.info.target else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let upstream = SocketAddr::new(target.address, target.port).to_string();
    let path = req
        .uri()
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .to_owned();
    let Ok(uri) = path.parse::<Uri>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *req.uri_mut() = uri;
    let headers = req.headers_mut();
    headers.remove(header::AUTHORIZATION);
    headers.remove(header::PROXY_AUTHORIZATION);
    headers.remove(header::REFERER);
    headers.remove("forwarded");
    headers.remove("x-forwarded-host");
    headers.remove("x-forwarded-for");
    if !websocket {
        headers.remove(header::UPGRADE);
        headers.remove(header::CONNECTION);
    }
    headers.insert(header::HOST, HeaderValue::from_str(&upstream).unwrap());
    if headers.contains_key(header::ORIGIN) {
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{upstream}")).unwrap(),
        );
    }
    let Ok(mut resp) = sender.send_request(req).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(front) = front
    {
        let back = hyper::upgrade::on(&mut resp);
        let stop = grant.stop.subscribe();
        tokio::spawn(async move {
            let bridge = async {
                let (Ok(front), Ok(back)) = tokio::join!(front, back) else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(
                    &mut TokioIo::new(front),
                    &mut TokioIo::new(back),
                )
                .await;
            };
            tokio::select! {_=bridge=>{},_=stopped(stop)=>{}}
        });
    }
    resp.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    resp.map(Body::new)
}
