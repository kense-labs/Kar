//! Revocable, device-scoped access for a chat's native agent.
use super::*;
use axum::{
    extract::DefaultBodyLimit,
    http::{HeaderMap, StatusCode},
};
use std::{
    io::Write,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Weak},
};
use subtle::ConstantTimeEq;

pub(super) struct AgentServer {
    address: tokio::sync::OnceCell<SocketAddr>,
    dir: PathBuf,
    stop: watch::Sender<()>,
}
#[derive(Clone)]
pub(super) struct Grant(Arc<Lease>);
pub(super) struct Lease {
    pub file: PathBuf,
    hash: String,
}
impl std::ops::Deref for Grant {
    type Target = Lease;
    fn deref(&self) -> &Lease {
        &self.0
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}
impl AgentServer {
    pub fn new(state: &Path) -> Result<Self> {
        let dir = state.join("device-agents");
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let path = e.path();
            if e.file_type()?.is_file()
                && path.extension().is_some_and(|s| s == "json")
                && path
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .is_some_and(|v| Uuid::parse_str(v).is_ok())
            {
                std::fs::remove_file(path)?;
            }
        }
        Ok(Self {
            address: tokio::sync::OnceCell::new(),
            dir,
            stop: watch::channel(()).0,
        })
    }
    pub async fn grant(&self, app: &Workbench) -> Result<Grant> {
        let address = self
            .address
            .get_or_try_init(|| async {
                let socket = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
                let address = socket.local_addr()?;
                let weak = Arc::downgrade(&app.0);
                let router = Router::new()
                    .route("/detail", get(agent_detail))
                    .route("/screenshot", get(agent_screenshot))
                    .route("/action", post(agent_action))
                    .with_state(weak)
                    .layer(DefaultBodyLimit::max(256 * 1024));
                let mut stop = self.stop.subscribe();
                tokio::spawn(async move {
                    let _ = axum::serve(socket, router)
                        .with_graceful_shutdown(async move {
                            let _ = stop.changed().await;
                        })
                        .await;
                });
                Ok::<_, std::io::Error>(address)
            })
            .await?;
        let key = crate::workbench::access::credential()?;
        let hash = crate::workbench::access::digest(&key);
        let file = self.dir.join(format!("{}.json", Uuid::new_v4()));
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(
            &mut tmp,
            &json!({"endpoint":format!("http://{address}"),"token":key}),
        )?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(&file).map_err(|e| e.error)?;
        Ok(Grant(Arc::new(Lease { file, hash })))
    }
}
impl Session {
    pub(super) fn revoke(&mut self) {
        if let Some(grant) = self.grant.take() {
            let _ = std::fs::remove_file(&grant.file);
        }
        self.agent_config = None;
    }
}
type WeakApp = Weak<crate::workbench::Inner>;
enum Operation {
    Detail,
    Screenshot,
    Action(Value),
}
async fn use_device(weak: WeakApp, headers: HeaderMap, op: Operation) -> Response {
    let Some(inner) = weak.upgrade() else {
        return StatusCode::GONE.into_response();
    };
    let token = crate::workbench::access::bearer(&headers);
    if token.len() != 64 {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let hash = crate::workbench::access::digest(token);
    let app = Workbench(inner);
    let svc = &app.0.devices;
    let mut s = svc.live.lock().await;
    let claim = s.sessions.iter().find(|session| {
        session
            .grant
            .as_ref()
            .is_some_and(|g| bool::from(g.hash.as_bytes().ct_eq(hash.as_bytes())))
    });
    let Some(claim) = claim.filter(|c| {
        s.settings.enabled
            && s.settings.agent_access_enabled
            && app.binding(c.project, c.chat).is_ok()
            && !app.archived(c.chat)
    }) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let device = claim.device_id.clone();
    let host = claim.host_id.clone();
    let result: Result<Response> = async {
        let d = svc.resolve(&mut s, &host, &device).await?;
        Ok(match op {
            Operation::Screenshot => (
                [(axum::http::header::CONTENT_TYPE, "image/png")],
                remote::at(d.ssh.as_ref(), native::screenshot(&d)).await?,
            )
                .into_response(),
            Operation::Detail => {
                Json(remote::at(d.ssh.as_ref(), actions::detail(&d)).await?).into_response()
            }
            Operation::Action(q) => {
                if q.get("device_id").is_some() || q.get("host_id").is_some() {
                    return Err(Error::Refused(
                        "agent action targets its bound device".into(),
                    ));
                }
                remote::at(d.ssh.as_ref(), actions::apply(&d, &q)).await?;
                Json(remote::at(d.ssh.as_ref(), actions::detail(&d)).await?).into_response()
            }
        })
    }
    .await;
    result.unwrap_or_else(|e| ApiError::from(e).into_response())
}
async fn agent_detail(State(s): State<WeakApp>, h: HeaderMap) -> Response {
    use_device(s, h, Operation::Detail).await
}
async fn agent_screenshot(State(s): State<WeakApp>, h: HeaderMap) -> Response {
    use_device(s, h, Operation::Screenshot).await
}
async fn agent_action(State(s): State<WeakApp>, h: HeaderMap, Json(q): Json<Value>) -> Response {
    use_device(s, h, Operation::Action(q)).await
}
