//! Owner-issued, one-use pairing grants and revocable device credentials.
pub(super) use guest::reconcile_guest_inputs;
mod guest;
use super::Workbench;
use crate::Error;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    path::{Path as FsPath, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Registry {
    grants: BTreeMap<String, u64>,
    devices: BTreeMap<Uuid, Device>,
    #[serde(default)]
    shares: BTreeMap<Uuid, guest::Share>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Device {
    id: Uuid,
    label: String,
    created_ms: u64,
    digest: String,
}

pub(super) struct AccessStore {
    file: PathBuf,
    registry: Mutex<Registry>,
    changed: watch::Sender<()>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn credential() -> crate::Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(encode_secret(&bytes))
}
fn encode_secret(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(out, "{b:02x}").expect("String accepts writes");
    }
    out
}
fn digest(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

#[derive(Debug)]
enum AccessError {
    Invalid,
    Denied,
    Missing,
    Conflict(String),
    Bad(String),
    Internal(Error),
}
impl From<Error> for AccessError {
    fn from(e: Error) -> Self {
        Self::Internal(e)
    }
}
impl IntoResponse for AccessError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            Self::Invalid => (
                StatusCode::UNAUTHORIZED,
                "invalid or expired credential".into(),
            ),
            Self::Denied => (StatusCode::FORBIDDEN, "owner access required".into()),
            Self::Missing => (StatusCode::NOT_FOUND, "unknown record".into()),
            Self::Bad(s) => (StatusCode::BAD_REQUEST, s),
            Self::Conflict(s) => (StatusCode::CONFLICT, s),
            Self::Internal(e) => return super::http::ApiError::from(e).into_response(),
        };
        (code, Json(json!({"error":msg}))).into_response()
    }
}
type AccessResult<T> = Result<T, AccessError>;

impl AccessStore {
    pub(super) fn open(state: &FsPath) -> crate::Result<Self> {
        let file = state.join("access.json");
        let registry: Registry = match File::open(&file) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e.into()),
        };
        let migrate = registry.shares.values().any(guest::needs_maintenance);
        let access = Self {
            file,
            registry: Mutex::new(registry),
            changed: watch::channel(()).0,
        };
        if migrate {
            access.update(|_| Ok(())).map_err(|e| match e {
                AccessError::Internal(e) => e,
                _ => Error::Protocol(format!("invalid saved share: {e:?}")),
            })?;
        }
        Ok(access)
    }
    fn update<T>(&self, f: impl FnOnce(&mut Registry) -> AccessResult<T>) -> AccessResult<T> {
        let mut current = self.registry.lock().unwrap();
        let mut db = current.clone();
        let result = f(&mut db)?;
        let parent = self.file.parent().expect("workbench state directory");
        guest::maintain(&mut db, parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
        serde_json::to_writer(&mut tmp, &db).map_err(Error::from)?;
        tmp.as_file().sync_all().map_err(Error::Io)?;
        tmp.persist(&self.file).map_err(|e| Error::Io(e.error))?;
        *current = db;
        self.changed.send_replace(());
        #[cfg(unix)]
        File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(Error::Io)?;
        Ok(result)
    }

    pub(super) fn notify(&self) {
        self.changed.send_replace(());
    }

    fn device_active(&self, id: Uuid, hashed: &str) -> bool {
        self.registry
            .lock()
            .unwrap()
            .devices
            .get(&id)
            .is_some_and(|d| bool::from(d.digest.as_bytes().ct_eq(hashed.as_bytes())))
    }
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
}

#[derive(Clone)]
pub(super) struct Auth {
    app: Workbench,
    owner: String,
}
impl Auth {
    pub(super) fn new(app: Workbench, owner: String) -> Self {
        Self { app, owner }
    }
    fn owner(&self, input: &str) -> bool {
        bool::from(self.owner.as_bytes().ct_eq(input.as_bytes()))
    }
}

async fn owner(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    if !auth.owner(bearer(req.headers())) {
        return AccessError::Denied.into_response();
    }
    next.run(req).await
}

pub(super) async fn authorize(State(auth): State<Auth>, mut req: Request, next: Next) -> Response {
    let key = bearer(req.headers());
    if auth.owner(key) {
        return next.run(req).await;
    }
    let Some((id, secret)) = key.split_once('.') else {
        return AccessError::Invalid.into_response();
    };
    let Ok(id) = id.parse() else {
        return AccessError::Invalid.into_response();
    };
    let hash = digest(secret);
    let device = DeviceLease {
        app: std::sync::Arc::downgrade(&auth.app.0),
        id,
        hash,
        changed: auth.app.0.access.changed.subscribe(),
    };
    if !device.active() {
        return AccessError::Invalid.into_response();
    }
    req.extensions_mut().insert(device.clone());
    restrict(req, next, device.revoked()).await
}

#[derive(Clone)]
pub(super) struct DeviceLease {
    app: std::sync::Weak<super::Inner>,
    pub(super) id: Uuid,
    hash: String,
    changed: watch::Receiver<()>,
}
impl DeviceLease {
    pub(super) fn active(&self) -> bool {
        self.app
            .upgrade()
            .is_some_and(|app| app.access.device_active(self.id, &self.hash))
    }
    pub(super) async fn revoked(mut self) {
        while self.active() && self.changed.changed().await.is_ok() {}
    }
}

async fn restrict(
    req: Request,
    next: Next,
    revoked: impl Future<Output = ()> + Send + 'static,
) -> Response {
    let mut revoked = Box::pin(revoked);
    let response = tokio::select! {
        biased;
        _ = &mut revoked => return AccessError::Invalid.into_response(),
        r=next.run(req) => r,
    };
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::from_stream(body.into_data_stream().take_until(revoked)),
    )
}

async fn update<T: Send + 'static>(
    app: Workbench,
    f: impl FnOnce(&mut Registry) -> AccessResult<T> + Send + 'static,
) -> AccessResult<Json<T>> {
    tokio::task::spawn_blocking(move || app.0.access.update(f))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
        .map(Json)
}

async fn grant(State(app): State<Workbench>) -> AccessResult<Json<Value>> {
    update(app, |db| {
        let current = now();
        db.grants.retain(|_, until| *until > current);
        let until = current + 5 * 60 * 1000;
        let code = credential()?;
        db.grants.insert(digest(&code), until);
        Ok(json!({"credential":code,"expires_ms":until}))
    })
    .await
}
#[derive(Deserialize)]
struct Pair {
    credential: String,
    label: String,
}
async fn pair(State(app): State<Workbench>, Json(p): Json<Pair>) -> AccessResult<Json<Value>> {
    update(app, move |db| {
        let label = p.label.trim();
        if label.is_empty() || label.chars().count() > 120 || label.chars().any(char::is_control) {
            return Err(AccessError::Bad(
                "specify a device label of 1 to 120 characters".into(),
            ));
        }
        let current = now();
        if db
            .grants
            .remove(&digest(&p.credential))
            .is_none_or(|expires| expires <= current)
        {
            return Err(AccessError::Invalid);
        }
        let id = Uuid::new_v4();
        let secret = credential()?;
        db.devices.insert(
            id,
            Device {
                id,
                label: label.into(),
                created_ms: current,
                digest: digest(&secret),
            },
        );
        Ok(json!({"id":id,"label":label,"token":format!("{id}.{secret}")}))
    })
    .await
}
async fn devices(State(app): State<Workbench>) -> Json<Value> {
    let db = app.0.access.registry.lock().unwrap();
    Json(json!(
        db.devices
            .values()
            .map(|d| json!({"id":d.id,"label":d.label,"created_ms":d.created_ms}))
            .collect::<Vec<_>>()
    ))
}
async fn revoke(State(app): State<Workbench>, Path(id): Path<Uuid>) -> AccessResult<Json<()>> {
    update(app, move |db| {
        db.devices.remove(&id);
        Ok(())
    })
    .await
}

pub(super) fn routes(app: Workbench, auth: Auth) -> Router {
    let admin = guest::admin()
        .route("/access/pairings", post(grant))
        .route("/access/devices", get(devices))
        .route("/access/devices/{id}", delete(revoke))
        .route_layer(middleware::from_fn_with_state(auth, owner));
    Router::new()
        .merge(admin)
        .merge(guest::public(app.clone()))
        .route("/access/pair", post(pair))
        .with_state(app)
}
