use super::{Port, ProcessId, Status, Terminal, process};
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
use serde::Serialize;
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
    pub id: Uuid,
    pub project: Uuid,
    pub terminal: Uuid,
    target: Port,
    pub url: String,
}

pub(super) struct Grant {
    pub info: Preview,
    device: Option<DeviceLease>,
    terminal: Weak<Terminal>,
    root: ProcessId,
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
        self.stop.send_replace(true);
    }
    fn active(&self) -> bool {
        !*self.stop.borrow() && self.device.as_ref().is_none_or(DeviceLease::active)
    }
    fn belongs_to(&self, project: Uuid, device: Option<Uuid>) -> bool {
        self.info.project == project
            && device.is_none_or(|id| self.device.as_ref().is_some_and(|d| d.id == id))
    }
    async fn visible(&self) -> Result<bool> {
        if !self.active()
            || !self
                .terminal
                .upgrade()
                .is_some_and(|t| matches!(t.snapshot().status, Status::Running))
        {
            return Ok(false);
        }
        let root = self.root;
        let target = self.info.target;
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
            let root = self.root;
            let target = self.info.target;
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
pub(super) struct Grants(Mutex<HashMap<Uuid, Arc<Grant>>>);
impl Grants {
    pub async fn open(
        &self,
        project: Uuid,
        term: Arc<Terminal>,
        target: Port,
        device: Option<DeviceLease>,
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
            id,
            project,
            terminal: m.id,
            target,
            url: format!("http://{authority}/"),
        };
        let grant = Arc::new(Grant {
            info: info.clone(),
            device: device.clone(),
            terminal: Arc::downgrade(&term),
            root,
            authority,
            stop: watch::channel(false).0,
        });
        self.0.lock().unwrap().retain(|_, g| g.active());
        self.0.lock().unwrap().insert(id, grant.clone());
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
            .map(|g| g.info.clone())
            .collect()
    }
    pub fn close(&self, project: Uuid, id: Uuid, device: Option<Uuid>) -> Result<()> {
        let mut all = self.0.lock().unwrap();
        if all.get(&id).is_none_or(|g| !g.belongs_to(project, device)) {
            return Err(super::missing());
        }
        all.remove(&id).unwrap().revoke();
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

async fn proxy(State(grant): State<Arc<Grant>>, mut req: Request) -> Response {
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
    let upstream = SocketAddr::new(grant.info.target.address, grant.info.target.port).to_string();
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
