//! Public OAuth metadata, self-contained client registration, and MCP-only credentials.
use super::*;
use axum::{
    extract::{DefaultBodyLimit, Query},
    http::HeaderValue,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, KeyInit, Mac};
use url::Url;
mod consent;
const READ: &str = "orchestration:read";
const OPERATE: &str = "orchestration:operate";
pub(super) const SESSION_MS: u64 = 30 * 86_400_000;
pub(super) const MODES: [&str; 5] = [
    "read-only",
    "approval-required",
    "auto-accept-edits",
    "auto",
    "full-access",
];
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Store {
    signing: Option<String>,
    codes: BTreeMap<String, Code>,
    sessions: BTreeMap<String, Session>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Code {
    request: RequestData,
    issuer: String,
    until: u64,
    mode: String,
    name: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Session {
    id: Uuid,
    until: u64,
    mode: String,
    name: String,
    issuer: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Client {
    #[serde(default)]
    peer: bool,
    name: String,
    redirects: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct RequestData {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    code_challenge_method: String,
    state: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
}
type OResult<T> = Result<T, Failure>;
pub(super) struct Failure(&'static str, &'static str);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":self.0,"error_description":self.1})),
        )
            .into_response()
    }
}
fn invalid() -> Failure {
    Failure("invalid_request", "Invalid OAuth request")
}
fn rejected() -> Failure {
    Failure(
        "invalid_grant",
        "Authorization code, redirect, client, resource or verifier is invalid",
    )
}
fn internal() -> Failure {
    Failure("server_error", "Could not persist authorization")
}
pub(in crate::workbench) fn origin(app: &Workbench) -> Option<String> {
    let input = std::env::var("KAR_PUBLIC_ORIGIN")
        .ok()
        .or_else(|| app.0.mcp.origin())?;
    let u = Url::parse(&input).ok()?;
    (u.path() == "/"
        && u.query().is_none()
        && u.fragment().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && (u.scheme() == "https" || (u.scheme() == "http" && loopback(&u))))
    .then(|| u.origin().ascii_serialization())
}
fn loopback(u: &Url) -> bool {
    matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}
fn redirect(s: &str) -> Option<Url> {
    let u = Url::parse(s).ok()?;
    (s.len() <= 512
        && u.has_host()
        && u.fragment().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && (u.scheme() == "https" || u.scheme() == "http" && loopback(&u)))
    .then_some(u)
}
fn matches_redirect(a: &str, b: &str) -> bool {
    let (Some(a), Some(b)) = (redirect(a), redirect(b)) else {
        return false;
    };
    a == b
        || a.scheme() == "http"
            && b.scheme() == "http"
            && a.host() == b.host()
            && a.path() == b.path()
            && a.query() == b.query()
}
fn matches_resource(s: Option<&str>, issuer: &str) -> bool {
    s.is_none_or(|s| {
        Url::parse(s).is_ok_and(|u| {
            u.fragment().is_none()
                && u.username().is_empty()
                && u.password().is_none()
                && u.origin().ascii_serialization() == issuer
                && u.path().trim_end_matches('/') == "/mcp"
        })
    })
}
pub(super) fn scopes(mode: &str) -> Vec<&'static str> {
    match mode {
        "read-only" => vec![READ],
        "full-access" => vec![READ, OPERATE, "settings:write", "source-control:write"],
        _ => vec![READ, OPERATE],
    }
}
fn signature(key: &str, purpose: &str, payload: &str) -> String {
    let mut mac = Hmac::<sha2_mac::Sha256>::new_from_slice(key.as_bytes()).expect("HMAC key");
    mac.update(purpose.as_bytes());
    mac.update(&[0]);
    mac.update(payload.as_bytes());
    B64.encode(mac.finalize().into_bytes())
}
fn signed(key: &str, purpose: &str, value: &impl Serialize) -> OResult<String> {
    let body = B64.encode(serde_json::to_vec(value).map_err(|_| internal())?);
    Ok(format!("{body}.{}", signature(key, purpose, &body)))
}
fn unpack<T: serde::de::DeserializeOwned>(key: &str, purpose: &str, s: &str) -> Option<T> {
    if s.len() > 16_384 {
        return None;
    }
    let (payload, mac) = s.split_once('.')?;
    if !bool::from(
        signature(key, purpose, payload)
            .as_bytes()
            .ct_eq(mac.as_bytes()),
    ) {
        return None;
    }
    serde_json::from_slice(&B64.decode(payload).ok()?).ok()
}
fn signing(app: &Workbench) -> OResult<String> {
    if let Some(key) = app.0.access.registry.lock().unwrap().oauth.signing.clone() {
        return Ok(key);
    }
    app.0
        .access
        .update(|db| {
            if db.oauth.signing.is_none() {
                db.oauth.signing = Some(credential()?)
            }
            Ok(db.oauth.signing.clone().unwrap())
        })
        .map_err(|_| internal())
}
fn urls(app: &Workbench) -> OResult<String> {
    origin(app).ok_or(Failure(
        "server_error",
        "Configure a valid Kar public origin",
    ))
}
fn current_origin(h: &HeaderMap, issuer: &str) -> OResult<()> {
    if h.contains_key(header::ORIGIN)
        && h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(issuer)
    {
        return Err(invalid());
    }
    Ok(())
}
async fn metadata(State(auth): State<Auth>) -> OResult<Json<Value>> {
    let u = urls(&auth.app)?;
    Ok(Json(
        json!({"issuer":u,"authorization_endpoint":format!("{u}/oauth/mcp/authorize"),"token_endpoint":format!("{u}/oauth/mcp/token"),"registration_endpoint":format!("{u}/oauth/mcp/register"),"revocation_endpoint":format!("{u}/oauth/mcp/revoke"),"response_types_supported":["code"],"grant_types_supported":["authorization_code"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],"scopes_supported":[READ,OPERATE],"authorization_response_iss_parameter_supported":true}),
    ))
}
async fn resource(State(auth): State<Auth>) -> OResult<Json<Value>> {
    let u = urls(&auth.app)?;
    Ok(Json(
        json!({"resource":format!("{u}/mcp"),"authorization_servers":[u],"scopes_supported":[READ,OPERATE],"bearer_methods_supported":["header"],"resource_name":"Kar"}),
    ))
}
pub(in crate::workbench) fn challenge(app: &Workbench, mut r: Response) -> Response {
    if r.status() == StatusCode::UNAUTHORIZED {
        if let Some(v) = origin(app).and_then(|u| {
            HeaderValue::from_str(&format!(
                "Bearer resource_metadata=\"{u}/.well-known/oauth-protected-resource/mcp\""
            ))
            .ok()
        }) {
            r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
    }
    r
}
#[derive(Deserialize)]
struct Registration {
    software_id: Option<String>,
    client_name: Option<String>,
    redirect_uris: Vec<String>,
}
async fn register(State(auth): State<Auth>, Json(body): Json<Registration>) -> OResult<Response> {
    let redirects = body.redirect_uris;
    if redirects.is_empty()
        || redirects.len() > 10
        || redirects.iter().any(|s| redirect(s).is_none())
    {
        return Err(Failure(
            "invalid_redirect_uri",
            "Register one to ten HTTPS or loopback HTTP callbacks without a fragment or credentials",
        ));
    }
    let name = body.client_name.unwrap_or_default();
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    let name = if name.is_empty() {
        "MCP client".into()
    } else {
        name
    };
    let client = Client {
        name,
        redirects,
        peer: body.software_id.as_deref() == Some("kar-peer-link"),
    };
    let id = signed(&signing(&auth.app)?, "client-id", &client)?;
    if id.len() > 10240 {
        return Err(Failure(
            "invalid_client_metadata",
            "Registered redirect URIs exceed size limit",
        ));
    }
    Ok((StatusCode::CREATED,Json(json!({"client_id":id,"client_name":client.name,"redirect_uris":client.redirects,"grant_types":["authorization_code"],"response_types":["code"],"token_endpoint_auth_method":"none"}))).into_response())
}
pub(super) fn form(h: &HeaderMap, b: &[u8]) -> OResult<BTreeMap<String, String>> {
    if h.get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .map(str::trim)
        != Some("application/x-www-form-urlencoded")
    {
        return Err(invalid());
    }
    let mut out = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(b) {
        if out.insert(k.into_owned(), v.into_owned()).is_some() {
            return Err(invalid());
        }
    }
    Ok(out)
}
async fn token(
    State(auth): State<Auth>,
    h: HeaderMap,
    body: axum::body::Bytes,
) -> OResult<Json<Value>> {
    let params = form(&h, &body)?;
    let get = |k: &str| params.get(k).map(String::as_str).unwrap_or("");
    let issuer = urls(&auth.app)?;
    if get("grant_type") != "authorization_code" {
        return Err(Failure(
            "unsupported_grant_type",
            "Only authorization_code is supported",
        ));
    }
    if unpack::<Client>(&signing(&auth.app)?, "client-id", get("client_id")).is_none() {
        return Err(Failure("invalid_client", "Unknown client ID"));
    }
    let code = digest(get("code"));
    let verifier = get("code_verifier");
    let result=auth.app.0.access.update(|db|{
        let Some(c)=db.oauth.codes.remove(&code) else{return Ok(None)};
        if c.until<=now() || c.request.client_id!=get("client_id") || c.request.redirect_uri!=get("redirect_uri") ||
            c.issuer!=issuer || !matches_resource(params.get("resource").map(String::as_str),&issuer) ||
            !(43..=128).contains(&verifier.len()) || !verifier.bytes().all(|v|v.is_ascii_alphanumeric()||b"-._~".contains(&v)) ||
            !bool::from(c.request.code_challenge.as_bytes().ct_eq(B64.encode(Sha256::digest(verifier)).as_bytes())){return Ok(None)}
        let raw=format!("kmcp_{}",credential()?);
        let scope=scopes(&c.mode).join(" ");
        db.oauth.sessions.retain(|_,v|v.until>now());
        db.oauth.sessions.insert(digest(&raw),Session{id:Uuid::new_v4(),until:now()+SESSION_MS,mode:c.mode,name:c.name,issuer});
        Ok(Some(json!({"access_token":raw,"token_type":"Bearer","expires_in":SESSION_MS/1000,"scope":scope})))
    }).map_err(|_|internal())?.ok_or_else(rejected)?;
    Ok(Json(result))
}
async fn revoke(
    State(auth): State<Auth>,
    h: HeaderMap,
    b: axum::body::Bytes,
) -> OResult<StatusCode> {
    let params = form(&h, &b)?;
    let raw = params
        .get("token")
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)?;
    if raw.starts_with("kmcp_") {
        auth.app
            .0
            .access
            .update(|db| Ok(db.oauth.sessions.remove(&digest(raw))))
            .map_err(|_| internal())?;
    }
    Ok(StatusCode::OK)
}
#[derive(Clone)]
pub(in crate::workbench) struct Lease {
    pub(in crate::workbench) id: Uuid,
    pub(in crate::workbench) mode: String,
    app: Workbench,
    key: String,
    until: u64,
}
impl Lease {
    pub(in crate::workbench) async fn revoked(self) {
        let mut changed = self.app.0.access.changed.subscribe();
        while self.until > now()
            && self
                .app
                .0
                .access
                .registry
                .lock()
                .unwrap()
                .oauth
                .sessions
                .contains_key(&self.key)
        {
            tokio::select! {_=tokio::time::sleep(std::time::Duration::from_millis(self.until.saturating_sub(now())))=>return,r=changed.changed()=>{if r.is_err(){return}}}
        }
    }
}
pub(in crate::workbench) fn resolve(app: &Workbench, key: &str) -> Option<Lease> {
    if !key.starts_with("kmcp_") {
        return None;
    }
    let key = digest(key);
    let db = app.0.access.registry.lock().unwrap();
    let s = db.oauth.sessions.get(&key)?;
    (s.until > now() && Some(&s.issuer) == origin(app).as_ref()).then(|| Lease {
        id: s.id,
        mode: s.mode.clone(),
        until: s.until,
        key,
        app: app.clone(),
    })
}
pub(super) fn clients(db: &Registry) -> Vec<Value> {
    db.oauth.sessions.values().filter(|s|s.until>now()).map(|s|json!({"id":s.id,"label":s.name,"kind":"mcp","access":s.mode,"scopes":scopes(&s.mode),"expires_ms":s.until})).collect()
}
pub(super) fn revoke_all(db: &mut Registry) -> usize {
    let size = db.oauth.sessions.len();
    db.oauth.sessions.clear();
    size
}
pub(super) fn revoke_id(db: &mut Registry, id: Uuid) {
    db.oauth.sessions.retain(|_, s| s.id != id);
}
pub(super) fn routes(auth: Auth) -> Router {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(resource))
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/mcp/register", post(register))
        .route("/oauth/mcp/token", post(token))
        .route("/oauth/mcp/revoke", post(revoke))
        .merge(consent::routes())
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn(no_cache))
        .with_state(auth)
}
pub(super) async fn no_cache(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        res.headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    res
}
