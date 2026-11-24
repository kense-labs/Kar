//! Environment device sessions and short-lived, audience-restricted tickets.
use super::*;
use axum::{
    body::Bytes,
    extract::DefaultBodyLimit,
    http::{Method, Uri},
};
use std::collections::BTreeSet;
pub(super) mod connect;
mod exchange;
mod management;
mod proof;
const COOKIE: &str = "kar_session";
const TTL: u64 = 30 * 86_400_000;
const SCOPES: &[&str] = &[
    "orchestration:read",
    "orchestration:operate",
    "settings:write",
    "providers:manage",
    "environment:maintain",
    "preview:operate",
    "diagnostics:read",
    "terminal:read",
    "terminal:operate",
    "source-control:write",
    "filesystem:read",
    "filesystem:write",
    "access:read",
    "access:write",
    "relay:read",
    "relay:write",
    "relay:configure",
];
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Store {
    #[serde(default)]
    cloud: connect::Store,
    grants: BTreeMap<String, Grant>,
    tickets: BTreeMap<String, Ticket>,
    proofs: BTreeMap<String, u64>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Account {
    #[serde(default)]
    cloud: Option<Uuid>,
    method: String,
    #[serde(default)]
    peer_thread_read: bool,
    #[serde(default)]
    peer_tasks:BTreeMap<Uuid,Uuid>,
    pub(super) until: u64,
    #[serde(default)]
    renew_until: Option<u64>,
    scopes: BTreeSet<String>,
    proof: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Grant {
    #[serde(default)]
    cloud: Option<connect::Binding>,
    id: Uuid,
    until: u64,
    scopes: BTreeSet<String>,
    label: String,
    access: Option<String>,
    #[serde(default)]
    peer: bool,
    #[serde(default)]
    peer_tasks:BTreeMap<Uuid,Uuid>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Ticket {
    id: Uuid,
    hash: String,
    until: u64,
    purpose: Option<String>,
}
fn all() -> BTreeSet<String> {
    SCOPES.iter().map(|s| s.to_string()).collect()
}
fn date(n: u64) -> String {
    jiff::Timestamp::from_millisecond(n as i64)
        .expect("device expiry")
        .to_string()
}
fn bad(s: &str) -> AccessError {
    AccessError::Bad(s.into())
}
fn same_site(auth: &Auth, h: &HeaderMap) -> AccessResult<()> {
    let origin = h.get(header::ORIGIN).and_then(|s| s.to_str().ok());
    if h.get("sec-fetch-site").is_some_and(|v| v == "cross-site")
        || (h.contains_key(header::ORIGIN)
            && !origin.is_some_and(|s| super::oauth::origin(&auth.app).as_deref() == Some(s)))
    {
        return Err(AccessError::Denied);
    }
    Ok(())
}
fn cookies(h: &HeaderMap) -> Option<&str> {
    let mut matches = h
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|v| v.trim().split_once('='))
        .filter(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v);
    let one = matches.next()?;
    matches.next().is_none().then_some(one)
}
fn split_token(raw: &str) -> AccessResult<(Uuid, String)> {
    let (id, secret) = raw.split_once('.').ok_or(AccessError::Invalid)?;
    Ok((
        id.parse().map_err(|_| AccessError::Invalid)?,
        digest(secret),
    ))
}
fn find(auth: &Auth, raw: &str) -> AccessResult<Device> {
    let (id, hash) = split_token(raw)?;
    let db = auth.app.0.access.registry.lock().unwrap();
    db.devices
        .get(&id)
        .filter(|d| {
            bool::from(d.digest.as_bytes().ct_eq(hash.as_bytes()))
                && d.account.as_ref().is_none_or(|a| a.until > now())
        })
        .cloned()
        .ok_or(AccessError::Invalid)
}
fn lease(auth: &Auth, d: Device) -> DeviceLease {
    DeviceLease {
        app: std::sync::Arc::downgrade(&auth.app.0),
        id: d.id,
        hash: d.digest,
        changed: auth.app.0.access.changed.subscribe(),
    }
}
fn credentials(auth: &Auth, h: &HeaderMap, method: &Method, path: &str) -> AccessResult<Device> {
    if let Some(header) = h.get(header::AUTHORIZATION) {
        let (kind, raw) = header
            .to_str()
            .ok()
            .and_then(|s| s.split_once(' '))
            .ok_or(AccessError::Invalid)?;
        let d = find(auth, raw)?;
        match d.account.as_ref() {
            None if kind == "Bearer" => {}
            Some(a)
                if matches!(a.method.as_str(), "bearer-access-token" | "peer-link")
                    && kind == "Bearer" => {}
            Some(a) if a.method == "dpop-access-token" && kind == "DPoP" => {
                let p = proof::verify(auth, h, method, path, Some(raw), a.proof.as_deref())?;
                auth.app.0.access.update(|db| p.consume(db))?;
            }
            _ => return Err(AccessError::Invalid),
        }
        return Ok(d);
    }
    let d = find(auth, cookies(h).ok_or(AccessError::Invalid)?)?;
    if d.account
        .as_ref()
        .is_none_or(|a| a.method != "browser-session-cookie")
    {
        return Err(AccessError::Invalid);
    }
    same_site(auth, h)?;
    Ok(d)
}
pub(super) fn approval_actor(auth: &Auth, h: &HeaderMap) -> Option<String> {
    same_site(auth, h).ok()?;
    let d = find(auth, cookies(h)?).ok()?;
    d.account.filter(|a| {
        a.method == "browser-session-cookie"
            && a.scopes.contains("access:write")
            && a.scopes.contains("orchestration:read")
    })?;
    Some(d.digest)
}
pub(super) fn approval_mode(auth: &Auth, h: &HeaderMap, mode: &str) -> bool {
    if auth.owner(bearer(h)) {
        return true;
    }
    let Ok(d) = find(auth, cookies(h).unwrap_or("")) else {
        return false;
    };
    d.account.is_some_and(|a| {
        super::oauth::scopes(mode)
            .iter()
            .all(|s| a.scopes.contains(*s))
    })
}
fn consume(db: &mut Registry, code: &str) -> AccessResult<Grant> {
    let hashed = digest(code);
    if let Some(g) = db.environment.grants.remove(&hashed) {
        return (g.until > now()).then_some(g).ok_or(AccessError::Invalid);
    }
    if db.grants.remove(&hashed).is_some_and(|n| n > now()) {
        return Ok(Grant {
            id: Uuid::nil(),
            cloud: None,
            until: now(),
            scopes: all(),
            label: "Kar device".into(),
            access: None,
            peer: false,
            peer_tasks:Default::default(),
        });
    }
    Err(AccessError::Invalid)
}
pub(super) fn direct_peer(db: &mut Registry, raw: &str, label: &str) -> AccessResult<Value> {
    let mut g = consume(db, raw)?;
    if g.cloud.is_some()
        || g.access.is_some() && !g.peer
        || !g.scopes.contains("orchestration:read")
    {
        return Err(AccessError::Denied);
    }
    g.label = label.into();
    let mut scopes=BTreeSet::from(["orchestration:read".into()]);
    if g.peer && !g.peer_tasks.is_empty() {scopes.insert("orchestration:operate".into());}
    g.access = None;
    let (device, token) = session(
        db,
        g,
        "peer-link",
        Some(scopes),
        None,
    )?;
    Ok(json!({"id":device.id,"label":device.label,"token":token}))
}
pub(super) fn consume_approval(
    db: &mut Registry,
    raw: &str,
    mode: &str,
    peer: bool,
) -> AccessResult<()> {
    let g = consume(db, raw)?;
    if g.cloud.is_some()
        || (g.access.is_some() && g.peer != peer)
        || g.access.as_ref().is_some_and(|s| s != mode)
        || !super::oauth::scopes(mode)
            .iter()
            .all(|s| g.scopes.contains(*s))
    {
        return Err(AccessError::Denied);
    }
    Ok(())
}
// Use the matched operation family. Limited sessions fall closed on unclassified routes.
fn required(method: &Method, path: &str) -> Option<&'static str> {
    let read = method == Method::GET || method == Method::HEAD;
    let parts = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["server","config"]|["server","config","events"]|["server","settings"] if read=>Some("orchestration:read"),
        ["server","settings"|"keybindings"] if !read=>Some("settings:write"),
        ["peer","delegations",_,_,_] if matches!(*method,Method::GET|Method::PUT|Method::DELETE)=>Some("orchestration:operate"),
        ["peer","threads",_,_,"history" | "item" | "events"] if method == Method::GET => Some("orchestration:read"),
        ["providers",_,"install",..] if read=>Some("providers:manage"),
        ["environment"] | ["projects"] | ["projects", _, "chats"] if read => {
            Some("orchestration:read")
        }
        ["search", "chats"] | ["projects", _, "search"] if read => Some("orchestration:read"),
        ["projects", _, "chats"] if !read => Some("orchestration:operate"),
        [
            "projects",
            _,
            "chats",
            _,
            "snapshot" | "events" | "handoffs" | "queue" | "timeline",
        ] if read => Some("orchestration:read"),
        [
            "projects",
            _,
            "chats",
            _,
            "messages" | "queue" | "archive" | "interrupt",
        ] if !read => Some("orchestration:operate"),
        ["projects", _, "previews" | "preview-profiles", ..]
        | ["devices", "screenshot" | "stream" | "video" | "input"] => Some("preview:operate"),
        ["projects", _, "terminals", ..] => Some(if read {
            "terminal:read"
        } else {
            "terminal:operate"
        }),
        ["projects", _, "git" | "forge", ..] => Some(if read {
            "orchestration:read"
        } else {
            "source-control:write"
        }),
        ["projects", _, "files", ..] => Some(if read {
            "filesystem:read"
        } else {
            "filesystem:write"
        }),
        ["observability", ..] if read => Some("diagnostics:read"),
        ["access", "current-device"] if method == Method::DELETE => Some("orchestration:read"),
        _ => None,
    }
}
fn audience(path: &str) -> Option<&'static str> {
    let parts = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["devices", "screenshot" | "stream" | "video" | "input"]
        | ["projects", _, "previews", _, "screen" | "tunnel" | "stream"] => Some("media"),
        ["projects", _, "chats", _, "events"]
        | ["projects", _, "terminals", _, "output"]
        | ["projects", _, "git", "events"]
        | ["projects", _, "preview-profiles", "report"] => Some("rpc"),
        _ => None,
    }
}
pub(super) fn authenticate(auth: &Auth, req: &Request) -> AccessResult<DeviceLease> {
    let path = req.uri().path();
    let query: Vec<_> = url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "wsTicket")
        .map(|(_, v)| v.into_owned())
        .collect();
    let d = if query.is_empty() {
        credentials(auth, req.headers(), req.method(), path)?
    } else {
        if query.len() != 1
            || req.method() != Method::GET
            || req.headers().contains_key(header::AUTHORIZATION)
        {
            return Err(AccessError::Invalid);
        }
        let use_ = audience(path).ok_or(AccessError::Invalid)?;
        let key = digest(&query[0]);
        let (d, purpose) = {
            let db = auth.app.0.access.registry.lock().unwrap();
            let t = db
                .environment
                .tickets
                .get(&key)
                .filter(|t| t.until > now() && t.purpose.as_deref().is_none_or(|p| p == use_))
                .ok_or(AccessError::Invalid)?;
            let d = db
                .devices
                .get(&t.id)
                .filter(|d| {
                    d.digest == t.hash && d.account.as_ref().is_none_or(|a| a.until > now())
                })
                .cloned()
                .ok_or(AccessError::Invalid)?;
            (d, t.purpose.clone())
        };
        let p = if use_ == "rpc" && purpose.as_deref() == Some("rpc") {
            d.account
                .as_ref()
                .and_then(|a| a.proof.as_deref())
                .map(|thumb| proof::ticket(auth, req, &query[0], thumb))
                .transpose()?
        } else {
            None
        };
        if use_ == "rpc" {
            auth.app.0.access.update(|db| {
                let t = db
                    .environment
                    .tickets
                    .remove(&key)
                    .filter(|t| t.until > now())
                    .ok_or(AccessError::Invalid)?;
                if let Some(p) = p {
                    p.consume(db)?
                }
                Ok(t.id)
            })?;
        }
        d
    };
    let peer_task=path.starts_with("/peer/delegations/");
    if peer_task {
        let selected=path.strip_prefix("/peer/delegations/").and_then(|tail| {
            let parts=tail.split('/').collect::<Vec<_>>();
            match parts.as_slice(){[env,p,job] if Uuid::parse_str(env).is_ok() && Uuid::parse_str(job).is_ok()=>Uuid::parse_str(p).ok(),_=>None}
        });
        if req.headers().contains_key("origin") || !matches!(*req.method(),Method::GET|Method::PUT|Method::DELETE)
            || !d.account.as_ref().is_some_and(|a|a.method=="peer-link" && selected.is_some_and(|p|a.peer_tasks.contains_key(&p))) {return Err(AccessError::Denied);}
    }
    let thread_read = path.starts_with("/peer/threads/");
    if thread_read && (req.headers().contains_key("origin") || !d.account.as_ref().is_some_and(|a| a.peer_thread_read)) {
        return Err(AccessError::Denied);
    }
    if let Some(a) = &d.account {
        if a.method == "peer-link"
            && !(peer_task || (thread_read && req.method() == Method::GET) || (path == "/environment" && req.method() == Method::GET)
                || (path == "/access/current-device" && req.method() == Method::DELETE))
        {
            return Err(AccessError::Denied);
        }
        if path == "/mcp" {
            return Err(AccessError::Invalid);
        }
        if a.scopes.len() != SCOPES.len()
            && required(req.method(), path).is_none_or(|s| !a.scopes.contains(s))
        {
            return Err(AccessError::Denied);
        }
    }
    Ok(lease(auth, d))
}
fn session(
    db: &mut Registry,
    grant: Grant,
    method: &str,
    requested: Option<BTreeSet<String>>,
    proof: Option<String>,
) -> AccessResult<(Device, String)> {
    if grant.access.is_some() {
        return Err(AccessError::Denied);
    }
    let cloud = connect::validate_device(db, &grant, proof.as_deref())?;
    let scopes = requested.unwrap_or_else(|| grant.scopes.clone());
    if !scopes.is_subset(&grant.scopes) || scopes.is_empty() {
        return Err(AccessError::Denied);
    }
    let id = Uuid::new_v4();
    let raw = credential()?;
    let d = Device {
        id,
        label: grant.label,
        created_ms: now(),
        digest: digest(&raw),
        account: Some(Account {
            cloud,
            method: method.into(),
            peer_thread_read: method == "peer-link" && grant.peer,
            peer_tasks:if method=="peer-link" && grant.peer {grant.peer_tasks}else{Default::default()},
            until: now() + if proof.is_some() { 3600000 } else { TTL },
            renew_until: (proof.is_some() && cloud.is_none()).then(|| now() + TTL),
            scopes,
            proof,
        }),
    };
    db.devices.insert(id, d.clone());
    Ok((d, format!("{id}.{raw}")))
}
fn status(d: &Device) -> Value {
    let a = d.account.as_ref();
    json!({"authenticated":true,"sessionId":d.id,"sessionMethod":a.map(|a|a.method.as_str()).unwrap_or("bearer-access-token"),"scopes":a.map(|a|a.scopes.clone()).unwrap_or_else(all),"expiresAt":a.map(|a|date(a.until))})
}
#[derive(Deserialize)]
struct Bootstrap {
    credential: String,
}
async fn browser_session(
    State(auth): State<Auth>,
    h: HeaderMap,
    Json(b): Json<Bootstrap>,
) -> AccessResult<Response> {
    same_site(&auth, &h)?;
    let old = cookies(&h).and_then(|s| find(&auth, s).ok()).filter(|d| {
        d.account
            .as_ref()
            .is_some_and(|a| a.method == "browser-session-cookie")
    });
    let (d, raw) = auth.app.0.access.update(|db| {
        let grant = consume(db, &b.credential)?;
        let result = session(db, grant, "browser-session-cookie", None, None)?;
        if let Some(old) = old {
            db.devices.remove(&old.id);
        }
        Ok(result)
    })?;
    let secure = if oauth::origin(&auth.app).is_some_and(|s| s.starts_with("https:")) {
        "; Secure"
    } else {
        ""
    };
    Ok((
        [(
            header::SET_COOKIE,
            format!(
                "{COOKIE}={raw}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
                TTL / 1000,
                secure
            ),
        )],
        Json(status(&d)),
    )
        .into_response())
}
async fn state(State(auth): State<Auth>, h: HeaderMap) -> AccessResult<Json<Value>> {
    let mut s = match credentials(&auth, &h, &Method::GET, "/api/auth/session") {
        Ok(d) => status(&d),
        Err(AccessError::Invalid) => json!({"authenticated":false}),
        Err(e) => return Err(e),
    };
    s["auth"] = json!({"policy":"remote-reachable","bootstrapMethods":["one-time-token"],"sessionMethods":["browser-session-cookie","bearer-access-token","dpop-access-token"],"sessionCookieName":COOKIE});
    Ok(Json(s))
}
async fn mint(State(auth): State<Auth>, h: HeaderMap, uri: Uri) -> AccessResult<Json<Value>> {
    let purpose = uri
        .path()
        .strip_prefix("/api/auth/websocket-ticket")
        .and_then(|s| s.strip_prefix('/'))
        .map(str::to_owned);
    if purpose
        .as_deref()
        .is_some_and(|s| !matches!(s, "rpc" | "media"))
    {
        return Err(bad("unknown ticket audience"));
    }
    let d = credentials(&auth, &h, &Method::POST, uri.path())?;
    let raw = credential()?;
    let until = now() + 60_000;
    auth.app.0.access.update(|db| {
        db.environment.tickets.retain(|_, t| t.until > now());
        if db.environment.tickets.len() >= 8192 {
            return Err(bad("too many active tickets"));
        }
        db.environment.tickets.insert(
            digest(&raw),
            Ticket {
                id: d.id,
                hash: d.digest,
                until,
                purpose,
            },
        );
        Ok(())
    })?;
    Ok(Json(json!({"ticket":raw,"expiresAt":date(until)})))
}
pub(super) fn routes(auth: Auth) -> Router {
    Router::new()
        .route("/api/auth/session", get(state))
        .route("/api/auth/browser-session", post(browser_session))
        .route("/api/auth/websocket-ticket", post(mint))
        .route("/api/auth/websocket-ticket/{purpose}", post(mint))
        .route("/oauth/token", post(exchange::token))
        .merge(management::routes())
        .merge(connect::routes())
        .layer(DefaultBodyLimit::max(32768))
        .layer(middleware::from_fn(super::oauth::no_cache))
        .with_state(auth)
}

impl AccessStore {
    pub(in crate::workbench) fn peer_task_template(&self,device:Uuid,project:Uuid)->Option<Uuid>{
        let db=self.registry.lock().ok()?;
        let a=db.devices.get(&device)?.account.as_ref()?;
        if a.method!="peer-link" || a.until<=now() || !a.scopes.contains("orchestration:operate"){return None;}
        a.peer_tasks.get(&project).copied()
    }
}
