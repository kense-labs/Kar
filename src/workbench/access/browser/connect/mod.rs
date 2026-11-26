//! Server side of an optional account relay. All public callbacks verify the configured relay key.
use super::*;
use crate::workbench::mcp::collaboration_tools::environment;
use axum::http::StatusCode;
use url::Url;
mod devices;
mod signed;
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::workbench) struct ManagedTunnelConfig {
    provider_kind: String,
    pub(in crate::workbench) connector_token: String,
    pub(in crate::workbench) tunnel_id: Option<String>,
    pub(in crate::workbench) tunnel_name: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredTunnelConfig {
    provider_kind: String,
    connector_token: crate::workbench::secrets::SecretValue,
    tunnel_id: Option<String>,
    tunnel_name: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Binding {
    link: Uuid,
    thumb: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Link {
    id: Uuid,
    user: String,
    url: String,
    issuer: String,
    credential: crate::workbench::secrets::SecretValue,
    mint_key: String,
    runtime: Option<StoredTunnelConfig>,
    scopes: BTreeSet<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Store {
    signing: Option<String>,
    link: Option<Link>,
    publish: bool,
    hold: bool,
    require_approval: Option<bool>,
    trusted: BTreeSet<String>,
    pending: BTreeMap<String, Pending>,
    used: BTreeMap<String, u64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    id: Uuid,
    until: u64,
}
impl Workbench {
    pub(in crate::workbench) fn managed_tunnel_config(&self) -> Option<ManagedTunnelConfig> {
        let rt = self
            .0
            .access
            .registry
            .lock()
            .unwrap()
            .environment
            .cloud
            .link
            .as_ref()?
            .runtime
            .clone()?;
        let svc = crate::workbench::secrets::SecretService::for_workbench(self).ok()?;
        let token = svc
            .resolve_local_sync("relay:tunnel_token", &rt.connector_token)
            .ok()?;
        Some(ManagedTunnelConfig {
            provider_kind: rt.provider_kind,
            connector_token: token.as_str().to_owned(),
            tunnel_id: rt.tunnel_id,
            tunnel_name: rt.tunnel_name,
        })
    }
    pub(in crate::workbench) fn connect_changes(&self) -> watch::Receiver<()> {
        self.0.access.changed.subscribe()
    }
}
fn permits(
    auth: &Auth,
    h: &HeaderMap,
    method: &Method,
    uri: &Uri,
    scope: &str,
) -> AccessResult<BTreeSet<String>> {
    Ok(super::management::authorize(auth, h, method, uri.path(), scope)?.1)
}
fn bounded(s: &str, n: usize) -> bool {
    !s.trim().is_empty() && s.len() <= n && !s.chars().any(char::is_control)
}
fn relay_url(raw: &str) -> AccessResult<String> {
    let u = Url::parse(raw).map_err(|_| bad("Invalid relay URL"))?;
    // Development relays require an exact host-side opt-in including the loopback port.
    let local = u.scheme() == "http"
        && matches!(u.host_str(), Some("127.0.0.1" | "[::1]"))
        && std::env::var("KAR_CONNECT_LOOPBACK_RELAY").ok().as_deref()
            == Some(u.origin().ascii_serialization().as_str());
    if !(u.scheme() == "https" || local)
        || !u.has_host()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || !bounded(raw, 2048)
    {
        return Err(bad("Relay URL requires HTTPS"));
    }
    Ok(u.as_str().trim_end_matches('/').into())
}
fn environment_id(auth: &Auth) -> AccessResult<Uuid> {
    Ok(environment::id(&auth.app)?)
}
fn descriptor(id: Uuid) -> Value {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        "linux" => "linux",
        _ => "unknown",
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => "other",
    };
    json!({"environmentId":id,"label":std::env::var("KAR_ENVIRONMENT_LABEL").unwrap_or_else(|_|"Kar".into()),"platform":{"os":os,"arch":arch},"serverVersion":env!("CARGO_PKG_VERSION"),"capabilities":{"repositoryIdentity":true,"accessTokenRenewal":true,"attachmentUploads":true}})
}
fn revoke(db: &mut Registry) {
    let s = &mut db.environment.cloud;
    s.link = None;
    s.trusted.clear();
    s.pending.clear();
    s.publish = false;
    s.hold = false;
    s.require_approval = None;
    db.environment.grants.retain(|_, g| g.cloud.is_none());
    db.devices
        .retain(|_, d| d.account.as_ref().is_none_or(|a| a.cloud.is_none()));
}
pub(super) fn clear_other_clients(db: &mut Registry, keep: Option<Uuid>) {
    let thumb = keep.and_then(|id| db.devices.get(&id)?.account.as_ref()?.proof.clone());
    db.environment
        .cloud
        .trusted
        .retain(|k| Some(k) == thumb.as_ref());
    db.environment.cloud.pending.clear();
    db.environment.grants.retain(|_, g| {
        g.cloud
            .as_ref()
            .is_none_or(|b| Some(&b.thumb) == thumb.as_ref())
    });
}
pub(super) fn validate_device(
    db: &mut Registry,
    g: &Grant,
    key: Option<&str>,
) -> AccessResult<Option<Uuid>> {
    let Some(b) = &g.cloud else { return Ok(None) };
    let s = &mut db.environment.cloud;
    if key != Some(b.thumb.as_str()) || s.link.as_ref().is_none_or(|l| l.id != b.link) {
        return Err(AccessError::Invalid);
    }
    if !s.trusted.contains(&b.thumb) && s.require_approval.unwrap_or(true) {
        return Err(AccessError::Pending(devices::verification(&b.thumb)));
    }
    s.trusted.insert(b.thumb.clone());
    Ok(Some(b.link))
}
fn state(auth: &Auth, available: &BTreeSet<String>) -> Value {
    let db = auth.app.0.access.registry.lock().unwrap();
    let s = &db.environment.cloud;
    let l = s.link.as_ref();
    let tunnel = auth.app.managed_tunnel_status();
    json!({"managedTunnelActive":tunnel["connected"], "managedTunnel":tunnel, "linked":l.is_some(),"relayUrl":l.map(|l|&l.url),"relayIssuer":l.map(|l|&l.issuer),"cloudUserId":l.map(|l|&l.user),"publishAgentActivity":s.publish,"holdWebhooksWhileOffline":s.hold,"requireConnectDeviceApproval":s.require_approval.unwrap_or(true),"linkableHere":available.contains("relay:configure")&&available.contains("relay:write")})
}
async fn get_state(State(auth): State<Auth>, h: HeaderMap, uri: Uri) -> AccessResult<Json<Value>> {
    let scopes = permits(&auth, &h, &Method::GET, &uri, "relay:read")?;
    Ok(Json(state(&auth, &scopes)))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    relay_url: String,
    relay_issuer: Option<String>,
    cloud_user_id: String,
    environment_credential: String,
    cloud_mint_public_key: String,
    endpoint_runtime: Option<ManagedTunnelConfig>,
}
async fn configure(
    State(auth): State<Auth>,
    h: HeaderMap,
    uri: Uri,
    Json(req): Json<Config>,
) -> AccessResult<Json<Value>> {
    let scopes = permits(&auth, &h, &Method::POST, &uri, "relay:configure")?;
    let url = relay_url(&req.relay_url)?;
    let issuer = relay_url(req.relay_issuer.as_deref().unwrap_or(&url))?;
    if !bounded(&req.cloud_user_id, 256) || !bounded(&req.environment_credential, 8192) {
        return Err(bad("Invalid relay account or credential"));
    }
    if req.endpoint_runtime.as_ref().is_some_and(|t| {
        t.provider_kind != "cloudflare_tunnel"
            || !bounded(&t.connector_token, 16384)
            || t.tunnel_id.as_ref().is_some_and(|s| !bounded(s, 256))
            || t.tunnel_name.as_ref().is_some_and(|s| !bounded(s, 256))
    }) {
        return Err(bad("Unsupported or invalid managed tunnel configuration"));
    }
    let mint_key = signed::normalize_key(&req.cloud_mint_public_key)?;
    let secrets = crate::workbench::secrets::SecretService::for_workbench(&auth.app)?;
    let sealed_credential = secrets.seal_input("relay:credential", req.environment_credential)?;
    let sealed_runtime = match req.endpoint_runtime {
        Some(rt) => Some(StoredTunnelConfig {
            provider_kind: rt.provider_kind,
            connector_token: secrets.seal_input("relay:tunnel_token", rt.connector_token)?,
            tunnel_id: rt.tunnel_id,
            tunnel_name: rt.tunnel_name,
        }),
        None => None,
    };
    let _guard = auth.app.0.access.connect_lock.lock().await;
    auth.app.0.access.update(|db| {
        let mut id = Uuid::new_v4();
        if let Some(l) = &db.environment.cloud.link {
            if l.user != req.cloud_user_id || l.issuer != issuer {
                return Err(AccessError::Conflict(
                    "Unlink the current relay account before switching".into(),
                ));
            }
            if l.mint_key == mint_key && l.scopes == scopes {
                id = l.id
            } else {
                revoke(db)
            }
        }
        db.environment.cloud.link = Some(Link {
            id,
            user: req.cloud_user_id,
            url,
            issuer,
            credential: sealed_credential,
            mint_key,
            runtime: sealed_runtime,
            scopes,
        });
        Ok(())
    })?;
    Ok(Json(json!({"ok":true})))
}
async fn unlink(State(auth): State<Auth>, h: HeaderMap, uri: Uri) -> AccessResult<Json<Value>> {
    permits(&auth, &h, &Method::POST, &uri, "relay:write")?;
    let _guard = auth.app.0.access.connect_lock.lock().await;
    auth.app.0.access.update(|db| {
        revoke(db);
        Ok(())
    })?;
    Ok(Json(json!({"ok":true})))
}
fn link(auth: &Auth) -> AccessResult<Link> {
    auth.app
        .0
        .access
        .registry
        .lock()
        .unwrap()
        .environment
        .cloud
        .link
        .clone()
        .ok_or(AccessError::Missing)
}
async fn send(
    app: &Workbench,
    link: &Link,
    id: Uuid,
    path: &str,
    value: Value,
) -> AccessResult<()> {
    let bearer = crate::workbench::secrets::SecretService::for_workbench(app)?
        .resolve_secret_value("relay:credential", &link.credential)
        .await?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| bad("Could not initialize relay transport"))?;
    let r = client
        .post(format!("{}/v1/environments/{id}/{path}", link.url))
        .bearer_auth(bearer.as_str())
        .json(&value)
        .send()
        .await
        .map_err(|_| {
            AccessError::Conflict("Relay outcome is unknown; retry with the same preference".into())
        })?;
    if !r.status().is_success() {
        return Err(AccessError::Conflict(format!(
            "Relay rejected preference with HTTP {}",
            r.status()
        )));
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Preferences {
    publish_agent_activity: bool,
    hold_webhooks_while_offline: Option<bool>,
    require_connect_device_approval: Option<bool>,
}
async fn preferences(
    State(auth): State<Auth>,
    h: HeaderMap,
    uri: Uri,
    Json(p): Json<Preferences>,
) -> AccessResult<Json<Value>> {
    let scopes = permits(&auth, &h, &Method::POST, &uri, "relay:write")?;
    if p.require_connect_device_approval.is_some() && !scopes.contains("access:write") {
        return Err(AccessError::Denied);
    }
    let _guard = auth.app.0.access.connect_lock.lock().await;
    let l = link(&auth)?;
    if let Some(hold) = p.hold_webhooks_while_offline {
        send(
            &auth.app,
            &l,
            environment_id(&auth)?,
            "link-preferences",
            json!({"holdWebhooksWhileOffline":hold}),
        )
        .await?
    }
    auth.app.0.access.update(|db| {
        let s = &mut db.environment.cloud;
        s.publish = p.publish_agent_activity;
        if let Some(v) = p.hold_webhooks_while_offline {
            s.hold = v
        }
        if let Some(v) = p.require_connect_device_approval {
            s.require_approval = Some(v)
        }
        Ok(())
    })?;
    Ok(Json(state(&auth, &scopes)))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Activity {
    live_activities_enabled: bool,
}
async fn activities(
    State(auth): State<Auth>,
    h: HeaderMap,
    uri: Uri,
    Json(p): Json<Activity>,
) -> AccessResult<Json<Value>> {
    permits(&auth, &h, &Method::POST, &uri, "relay:write")?;
    let _guard = auth.app.0.access.connect_lock.lock().await;
    let Ok(l) = link(&auth) else {
        return Ok(Json(json!({"ok":false})));
    };
    send(
        &auth.app,
        &l,
        environment_id(&auth)?,
        "live-activity-preference",
        json!({"cloudUserId":l.user,"liveActivitiesEnabled":p.live_activities_enabled}),
    )
    .await?;
    Ok(Json(json!({"ok":true})))
}
async fn reachability(
    State(auth): State<Auth>,
    h: HeaderMap,
    uri: Uri,
) -> AccessResult<Json<Value>> {
    permits(&auth, &h, &Method::POST, &uri, "relay:read")?;
    let id = environment_id(&auth)?;
    let origin = oauth::origin(&auth.app);
    let tunnel = auth.app.managed_tunnel_status();
    let check = async {
        link(&auth).ok()?;
        let u = origin.as_ref().filter(|u| u.starts_with("https://"))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let mut r = client
            .get(format!("{u}/api/connect/descriptor"))
            .send()
            .await
            .ok()?;
        if r.status() != StatusCode::OK {
            return None;
        }
        let mut data = Vec::new();
        while let Some(chunk) = r.chunk().await.ok()? {
            if data.len() + chunk.len() > 8192 {
                return None;
            }
            data.extend(chunk)
        }
        let body: Value = serde_json::from_slice(&data).ok()?;
        (body["environmentId"] == id.to_string()).then_some(())
    }
    .await;
    Ok(Json(if check.is_some() {
        json!({"status":"reachable","managedTunnel":tunnel,"checkedAt":date(now()),"readyConnections":null,"protocol":"https"})
    } else {
        json!({"status":"unreachable","managedTunnel":tunnel,"checkedAt":date(now()),"cause":"unexpected_response","reason":"Public endpoint did not confirm this environment ID","hint":null,"fix":"Verify the relay/tunnel and KAR_PUBLIC_ORIGIN"})
    }))
}
async fn public_descriptor(State(auth): State<Auth>) -> AccessResult<Json<Value>> {
    Ok(Json(json!({"environmentId":environment_id(&auth)?})))
}

fn relay_snapshot(app: &Workbench) -> (Option<Link>, bool, bool) {
    let db = app.0.access.registry.lock().unwrap();
    let s = &db.environment.cloud;
    (s.link.clone(), s.publish, s.hold)
}

async fn drain_held_webhooks_once(app: &Workbench, link: &Link) {
    let Ok(env_id) = environment::id(app) else {
        return;
    };
    let Ok(svc) = crate::workbench::secrets::SecretService::for_workbench(app) else {
        return;
    };
    let Ok(bearer) = svc
        .resolve_secret_value("relay:credential", &link.credential)
        .await
    else {
        return;
    };
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return;
    };
    let Ok(resp) = client
        .post(format!(
            "{}/v1/environments/{env_id}/held-webhooks/drain",
            link.url
        ))
        .bearer_auth(bearer.as_str())
        .json(&json!({"maxItems": 32}))
        .send()
        .await
    else {
        return;
    };
    if !resp.status().is_success() {
        return;
    }
    let Ok(payload) = resp.json::<Value>().await else {
        return;
    };
    let Some(items) = payload["webhooks"].as_array() else {
        return;
    };
    if items.is_empty() {
        return;
    }
    let hooks_router = crate::workbench::workflows::hooks().with_state(app.clone());
    for item in items.iter().take(32) {
        let Some(raw_path) = item["path"]
            .as_str()
            .filter(|p| p.starts_with("/hooks/workflows/"))
        else {
            continue;
        };
        let body_bytes = if let Some(b64) = item["bodyBase64"].as_str() {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap_or_default()
        } else if let Some(s) = item["body"].as_str() {
            s.as_bytes().to_vec()
        } else {
            serde_json::to_vec(&item["body"]).unwrap_or_default()
        };
        let mut builder = axum::http::Request::builder()
            .method(Method::POST)
            .uri(raw_path);
        if let Some(headers) = item["headers"].as_object() {
            for (k, v) in headers {
                if let Some(val) = v.as_str() {
                    builder = builder.header(k.as_str(), val);
                }
            }
        }
        if let Ok(req) = builder.body(axum::body::Body::from(body_bytes)) {
            use tower::ServiceExt as _;
            let _ = hooks_router.clone().oneshot(req).await;
        }
    }
}

pub(in crate::workbench) fn start_relay_workers(app: Workbench) {
    // 1. AgentAwarenessRelay: pushes live agent activity transitions when `publish == true`.
    let relay_app = app.clone();
    tokio::spawn(async move {
        let weak = std::sync::Arc::downgrade(&relay_app.0);
        let mut hub_rx = relay_app.0.milestone_hub.subscribe();
        let mut th_rx = relay_app.0.thread_updates.subscribe();
        let mut wf_rx = relay_app.0.workflows.subscribe();
        drop(relay_app);
        let mut last_payload: Option<Value> = None;

        loop {
            tokio::select! {
                r = hub_rx.changed() => { if r.is_err() { break; } }
                r = th_rx.changed() => { if r.is_err() { break; } }
                r = wf_rx.changed() => { if r.is_err() { break; } }
            }
            let Some(inner) = weak.upgrade() else {
                break;
            };
            let app = Workbench(inner);
            let (Some(link), true, _) = relay_snapshot(&app) else {
                last_payload = None;
                continue;
            };
            let Ok(env_id) = environment::id(&app) else {
                continue;
            };
            let active_chats: Vec<Uuid> = app
                .0
                .controllers
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(id, c)| {
                    c.owner
                        .try_lock()
                        .ok()
                        .and_then(|g| g.as_ref().filter(|s| !s.is_closed()).map(|_| *id))
                })
                .collect();
            if active_chats.is_empty() && last_payload.is_none() {
                continue;
            }
            let summary = json!({
                "environmentId": env_id,
                "cloudUserId": link.user,
                "activeChatIds": active_chats,
                "activeChatCount": active_chats.len(),
                "threadSequence": *app.0.thread_updates.borrow(),
            });
            if last_payload.as_ref() == Some(&summary) {
                continue;
            }
            let mut body = summary.clone();
            body["publishedAt"] = json!(date(now()));
            if send(&app, &link, env_id, "agent-activity", body)
                .await
                .is_ok()
            {
                last_payload = Some(summary);
            }
        }
    });

    // 2. HeldHooksWaker: drains queued offline webhooks on connect/wake when `hold == true`.
    tokio::spawn(async move {
        let weak = std::sync::Arc::downgrade(&app.0);
        let mut cfg_rx = app.connect_changes();
        drop(app);
        loop {
            if let Some(inner) = weak.upgrade() {
                let live = Workbench(inner);
                if let (Some(link), _, true) = relay_snapshot(&live) {
                    drain_held_webhooks_once(&live, &link).await;
                }
            } else {
                break;
            }
            tokio::select! {
                r = cfg_rx.changed() => { if r.is_err() { break; } }
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
            }
        }
    });
}

pub(super) fn routes() -> Router<Auth> {
    Router::new()
        .route("/api/connect/link-state", get(get_state))
        .route("/api/connect/relay-config", post(configure))
        .route("/api/connect/descriptor", get(public_descriptor))
        .route("/api/connect/reachability", post(reachability))
        .route("/api/connect/unlink", post(unlink))
        .route("/api/connect/preferences", post(preferences))
        .route("/api/connect/live-activity-preference", post(activities))
        .merge(signed::routes())
        .merge(devices::routes())
}
