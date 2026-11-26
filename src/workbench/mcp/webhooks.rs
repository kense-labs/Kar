//! SEP-3415 Outbound MCP Event Webhooks (`events/subscribe` & `events/unsubscribe`)
//! with IANA non-global IP blocklist, DNS-rebinding-safe `reqwest::dns::Resolve`,
//! Standard Webhooks (`whsec_`) HMAC-SHA256 signing, secret rotation grace period,
//! challenge verification handshake, and ordered retry / `410 Gone` delivery.

use super::{
    Caller, Workbench,
    milestones::{Args, Params, read_page_for_webhook},
};
use crate::{Error, Result, workbench::policy::ManagedPolicy};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2_mac::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;
use url::Url;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub fn allowed_hosts_from_env() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for var in [
        "KAR_MCP_WEBHOOK_ALLOWED_HOSTS",
        "T3CODE_MCP_WEBHOOK_ALLOWED_HOSTS",
    ] {
        if let Ok(val) = std::env::var(var) {
            for item in val.split(',') {
                let host = item.trim().trim_matches(['[', ']']).to_ascii_lowercase();
                if !host.is_empty() {
                    out.insert(host);
                }
            }
        }
    }
    out
}

fn ipv4_in_subnet(ip: Ipv4Addr, net: [u8; 4], prefix: u8) -> bool {
    let ip_u32 = u32::from(ip);
    let net_u32 = u32::from(Ipv4Addr::from(net));
    if prefix == 0 {
        return true;
    }
    let mask = !0u32 << (32 - prefix);
    (ip_u32 & mask) == (net_u32 & mask)
}

fn ipv6_in_subnet(ip: Ipv6Addr, net: u128, prefix: u8) -> bool {
    let ip_u128 = u128::from(ip);
    if prefix == 0 {
        return true;
    }
    let mask = !0u128 << (128 - prefix);
    (ip_u128 & mask) == (net & mask)
}

/// Exact IANA special-purpose / non-globally-routable IP filter matching T3's `McpWebhookTransport`.
pub fn is_globally_routable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            const BLOCKED_V4: &[([u8; 4], u8)] = &[
                ([0, 0, 0, 0], 8),
                ([10, 0, 0, 0], 8),
                ([100, 64, 0, 0], 10),
                ([127, 0, 0, 0], 8),
                ([169, 254, 0, 0], 16),
                ([172, 16, 0, 0], 12),
                ([192, 0, 0, 0], 24),
                ([192, 0, 2, 0], 24),
                ([192, 88, 99, 0], 24),
                ([192, 168, 0, 0], 16),
                ([198, 18, 0, 0], 15),
                ([198, 51, 100, 0], 24),
                ([203, 0, 113, 0], 24),
                ([224, 0, 0, 0], 4),
                ([240, 0, 0, 0], 4),
            ];
            !BLOCKED_V4
                .iter()
                .any(|(net, pfx)| ipv4_in_subnet(v4, *net, *pfx))
        }
        IpAddr::V6(v6) => {
            // Must be in IPv6 Global Unicast 2000::/3 (excludes ::1, fe80::/10, fc00::/7, and ::ffff:0:0/96 IPv4-mapped).
            let global_unicast = 0x2000_0000_0000_0000_0000_0000_0000_0000u128;
            if !ipv6_in_subnet(v6, global_unicast, 3) {
                return false;
            }
            const BLOCKED_V6: &[(u128, u8)] = &[
                (0x2001_0000_0000_0000_0000_0000_0000_0000u128, 23),
                (0x2001_0db8_0000_0000_0000_0000_0000_0000u128, 32),
                (0x2002_0000_0000_0000_0000_0000_0000_0000u128, 16),
                (0x3fff_0000_0000_0000_0000_0000_0000_0000u128, 20),
            ];
            !BLOCKED_V6
                .iter()
                .any(|(net, pfx)| ipv6_in_subnet(v6, *net, *pfx))
        }
    }
}

struct SafeWebhookResolver {
    allowed_hosts: Arc<BTreeSet<String>>,
}

impl Resolve for SafeWebhookResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().trim_matches(['[', ']']).to_ascii_lowercase();
        let allowed = self.allowed_hosts.clone();
        Box::pin(async move {
            let lookup_host = host.clone();
            let addrs: Vec<SocketAddr> = tokio::task::spawn_blocking(move || {
                (lookup_host.as_str(), 0u16)
                    .to_socket_addrs()
                    .map(Iterator::collect)
            })
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            if addrs.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    format!("{host} could not be resolved"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            if !allowed.contains(&host) && addrs.iter().any(|sa| !is_globally_routable_ip(sa.ip()))
            {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{host} is not a public address"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

pub async fn validate_webhook_url(
    raw_url: &str,
    allowed_hosts: &BTreeSet<String>,
) -> std::result::Result<Url, String> {
    let url = Url::parse(raw_url).map_err(|_| "Invalid webhook callback URL".to_owned())?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("Webhook URL must not include credentials or fragments".into());
    }
    let host = url
        .host_str()
        .map(|h| h.trim_matches(['[', ']']).to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "Webhook URL requires a host".to_owned())?;
    let host_allowed = allowed_hosts.contains(&host);
    match url.scheme() {
        "https" => {}
        "http" if host_allowed => {}
        "http" => return Err("Webhook URL requires https:// unless host is allowlisted".into()),
        _ => return Err("Unsupported webhook URL scheme".into()),
    }
    if host_allowed {
        return Ok(url);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_globally_routable_ip(ip) {
            return Err(format!("{host} is not a public address"));
        }
        return Ok(url);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let lookup_host = host.clone();
    let resolved = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || (lookup_host.as_str(), port).to_socket_addrs()),
    )
    .await
    .map_err(|_| format!("{host} could not be resolved"))?
    .map_err(|_| format!("{host} could not be resolved"))?
    .map_err(|_| format!("{host} could not be resolved"))?
    .collect::<Vec<_>>();

    if resolved.is_empty() || resolved.iter().any(|sa| !is_globally_routable_ip(sa.ip())) {
        return Err(format!("{host} is not a public address"));
    }
    Ok(url)
}

fn build_webhook_client(allowed_hosts: Arc<BTreeSet<String>>) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .dns_resolver(Arc::new(SafeWebhookResolver { allowed_hosts }))
        .build()?)
}

fn decode_signing_secret(secret: &str) -> Result<Vec<u8>> {
    if secret.trim().len() < 8 {
        return Err(Error::Refused(
            "webhook secret must be at least 8 characters".into(),
        ));
    }
    if let Some(b64) = secret.strip_prefix("whsec_") {
        let bytes = STANDARD
            .decode(b64)
            .or_else(|_| URL_SAFE_NO_PAD.decode(b64.trim_end_matches('=')))
            .map_err(|_| Error::Refused("invalid whsec_ base64 webhook secret".into()))?;
        if bytes.len() < 16 {
            return Err(Error::Refused(
                "whsec_ secret must decode to at least 16 bytes".into(),
            ));
        }
        return Ok(bytes);
    }
    Ok(secret.as_bytes().to_vec())
}

pub fn sign_standard_webhook(
    secrets: &[String],
    webhook_id: &str,
    timestamp_secs: u64,
    body: &[u8],
) -> Result<String> {
    let mut prefix = format!("{webhook_id}.{timestamp_secs}.").into_bytes();
    prefix.extend_from_slice(body);
    let mut sigs = Vec::with_capacity(secrets.len());
    for s in secrets {
        let key = decode_signing_secret(s)?;
        let mut mac = HmacSha256::new_from_slice(&key)
            .map_err(|_| Error::Refused("invalid HMAC key".into()))?;
        mac.update(&prefix);
        let tag = STANDARD.encode(mac.finalize().into_bytes());
        sigs.push(format!("v1,{tag}"));
    }
    Ok(sigs.join(" "))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_iso() -> String {
    jiff::Timestamp::now().to_string()
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliverySpec {
    mode: String,
    url: Option<String>,
    secret: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubscribeInput {
    name: String,
    delivery: DeliverySpec,
    cursor: Option<String>,
    max_age_ms: Option<u64>,
    ttl_ms: Option<u64>,
    #[serde(default)]
    arguments: Args,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnsubscribeInput {
    name: String,
    delivery: DeliverySpec,
}

struct SubscriptionState {
    id: String,
    name: String,
    url: String,
    current_secret: String,
    previous_secret: Option<(String, Instant)>,
    cursor: String,
    arguments: Args,
    expires_at: Instant,
    last_error: Option<String>,
    last_delivery_at: Option<String>,
    verified: bool,
    cancelled: bool,
    wake: Arc<Notify>,
}

type SubscriptionKey = (String, String);
type SharedSubscription = Arc<Mutex<SubscriptionState>>;

#[derive(Default)]
pub struct WebhookRegistry {
    subs: Mutex<BTreeMap<SubscriptionKey, SharedSubscription>>,
}

static WEBHOOK_REGISTRY: std::sync::OnceLock<Arc<WebhookRegistry>> = std::sync::OnceLock::new();

fn registry() -> Arc<WebhookRegistry> {
    WEBHOOK_REGISTRY
        .get_or_init(|| Arc::new(WebhookRegistry::default()))
        .clone()
}

fn typed_err(http: StatusCode, id: &Value, code: i32, message: &str, data: Value) -> Response {
    (
        http,
        Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}})),
    )
        .into_response()
}

fn rpc_ok(id: &Value, result: Value, stateless: bool) -> Response {
    super::response(id, result, stateless)
}

async fn verify_endpoint(
    client: &reqwest::Client,
    url: &Url,
    secret: &str,
    event_name: &str,
    sub_id: &str,
) -> std::result::Result<(), String> {
    let challenge = format!("chal_{}", Uuid::new_v4().simple());
    let webhook_id = format!("msg_{}", Uuid::new_v4().simple());
    let ts = now_secs();
    let payload = serde_json::to_vec(&json!({
        "type": "verification",
        "name": event_name,
        "challenge": challenge,
    }))
    .map_err(|e| e.to_string())?;
    let sig = sign_standard_webhook(&[secret.to_owned()], &webhook_id, ts, &payload)
        .map_err(|e| e.to_string())?;

    let resp = client
        .post(url.clone())
        .header("content-type", "application/json")
        .header("webhook-id", &webhook_id)
        .header("webhook-timestamp", ts.to_string())
        .header("webhook-signature", sig)
        .header("x-mcp-subscription-id", sub_id)
        .body(payload)
        .send()
        .await
        .map_err(|e| format!("Webhook verification request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "Webhook endpoint rejected verification with HTTP {}",
            resp.status().as_u16()
        ));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|_| "Webhook verification response was not valid JSON".to_owned())?;
    if body["challenge"].as_str() != Some(challenge.as_str()) {
        return Err("Webhook endpoint did not echo the verification challenge".into());
    }
    Ok(())
}

fn spawn_delivery_worker(
    app: Workbench,
    sub: Arc<Mutex<SubscriptionState>>,
    allowed_hosts: Arc<BTreeSet<String>>,
) {
    tokio::spawn(async move {
        let Ok(client) = build_webhook_client(allowed_hosts) else {
            return;
        };
        let mut hub_rx = app.0.milestone_hub.subscribe();
        let mut wf_rx = app.0.workflows.subscribe();
        let mut th_rx = app.0.thread_updates.subscribe();
        let mut fg_rx = app.0.forge_updates.subscribe();

        let wake = sub.lock().unwrap().wake.clone();

        loop {
            let (id, name, url, secrets, cursor, arguments, expired) = {
                let mut s = sub.lock().unwrap();
                if s.cancelled || Instant::now() >= s.expires_at {
                    s.cancelled = true;
                    break;
                }
                let mut secrets = vec![s.current_secret.clone()];
                if let Some((prev, until)) = &s.previous_secret {
                    if Instant::now() < *until {
                        secrets.insert(0, prev.clone());
                    } else {
                        s.previous_secret = None;
                    }
                }
                (
                    s.id.clone(),
                    s.name.clone(),
                    s.url.clone(),
                    secrets,
                    s.cursor.clone(),
                    s.arguments.clone(),
                    false,
                )
            };
            if expired {
                break;
            }

            let params = Params {
                name: name.clone(),
                cursor: Some(cursor.clone()),
                max_age_ms: None,
                max_events: Some(50),
                heartbeat_ms: None,
                arguments,
            };

            let Ok(page) = read_page_for_webhook(app.clone(), params).await else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };

            if page.truncated {
                // Deliver a gap notification first before advancing past truncated history.
                let gap_id = format!("gap_{}", Uuid::new_v4().simple());
                let payload_val = json!({
                    "type": "gap",
                    "eventId": gap_id,
                    "name": name,
                    "cursor": page.replay_start(),
                });
                let delivered =
                    deliver_one(&client, &url, &id, &gap_id, &secrets, &payload_val, &sub).await;
                if !delivered {
                    continue;
                }
            }

            let mut stalled_on_retry = false;
            for (next_cursor, item) in page.events {
                if sub.lock().unwrap().cancelled {
                    return;
                }
                let raw_id = item["id"].as_str().unwrap_or("evt");
                let clean_id: String = raw_id
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let event_id = format!("evt_{clean_id}");
                let payload_val = json!({
                    "type": "event",
                    "eventId": event_id,
                    "name": name,
                    "cursor": next_cursor,
                    "data": item,
                });
                if !deliver_one(&client, &url, &id, &event_id, &secrets, &payload_val, &sub).await {
                    stalled_on_retry = true;
                    break;
                }
                let mut s = sub.lock().unwrap();
                s.cursor = next_cursor;
            }

            if !stalled_on_retry && !page.has_more {
                {
                    let mut s = sub.lock().unwrap();
                    s.cursor = page.cursor;
                }
                tokio::select! {
                    _ = wake.notified() => {},
                    _ = hub_rx.changed() => {},
                    _ = wf_rx.changed() => {},
                    _ = th_rx.changed() => {},
                    _ = fg_rx.changed() => {},
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                }
            }
        }
    });
}

/// Delivers one webhook payload with retry on 5xx / network error and permanent skip on 410 Gone.
/// Returns `true` once acknowledged (`2xx`) or dropped (`410`), or `false` if subscription ended.
async fn deliver_one(
    client: &reqwest::Client,
    url: &str,
    sub_id: &str,
    webhook_id: &str,
    secrets: &[String],
    payload_val: &Value,
    sub: &Arc<Mutex<SubscriptionState>>,
) -> bool {
    let Ok(body) = serde_json::to_vec(payload_val) else {
        return true;
    };
    let mut backoff_ms = 500u64;
    loop {
        let current_secrets = {
            let mut s = sub.lock().unwrap();
            if s.cancelled || Instant::now() >= s.expires_at {
                s.cancelled = true;
                return false;
            }
            let mut list = vec![s.current_secret.clone()];
            if let Some((prev, until)) = &s.previous_secret
                && Instant::now() < *until
            {
                list.insert(0, prev.clone());
            }
            if list.is_empty() {
                secrets.to_vec()
            } else {
                list
            }
        };
        let ts = now_secs();
        let Ok(sig) = sign_standard_webhook(&current_secrets, webhook_id, ts, &body) else {
            return false;
        };
        let res = client
            .post(url)
            .header("content-type", "application/json")
            .header("webhook-id", webhook_id)
            .header("webhook-timestamp", ts.to_string())
            .header("webhook-signature", sig)
            .header("x-mcp-subscription-id", sub_id)
            .body(body.clone())
            .send()
            .await;

        match res {
            Ok(r) if r.status().is_success() => {
                let mut s = sub.lock().unwrap();
                s.last_error = None;
                s.last_delivery_at = Some(now_iso());
                return true;
            }
            Ok(r) if r.status() == StatusCode::GONE => {
                let mut s = sub.lock().unwrap();
                s.last_error = Some("http_410".into());
                return true;
            }
            Ok(r) => {
                let code = r.status().as_u16();
                let label = if code >= 500 {
                    "http_5xx".to_owned()
                } else {
                    format!("http_{code}")
                };
                sub.lock().unwrap().last_error = Some(label);
            }
            Err(_) => {
                sub.lock().unwrap().last_error = Some("network_error".into());
            }
        }

        let wake = sub.lock().unwrap().wake.clone();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {},
            _ = wake.notified() => {},
        }
        backoff_ms = (backoff_ms * 2).min(10_000);
    }
}

pub(super) async fn handle_subscribe(
    app: Workbench,
    _caller: &Caller,
    raw_params: &Value,
    id: &Value,
    stateless: bool,
) -> Response {
    if let Ok(snap) = ManagedPolicy::load()
        && snap.policy.check_mcp_outbound_webhooks().is_err()
    {
        return typed_err(
            StatusCode::OK,
            id,
            -32026,
            "Outbound MCP webhooks are disabled by ManagedPolicy",
            json!({"feature": "deliveryMode", "value": "webhook"}),
        );
    }

    let mode = raw_params
        .pointer("/delivery/mode")
        .cloned()
        .unwrap_or(Value::Null);
    if mode.as_str() != Some("webhook") {
        return typed_err(
            StatusCode::OK,
            id,
            -32026,
            "Unsupported delivery mode",
            json!({"feature": "deliveryMode", "value": mode}),
        );
    }

    let mut cleaned = raw_params.clone();
    if let Some(obj) = cleaned.as_object_mut() {
        obj.remove("_meta");
    }
    let Ok(input) = serde_json::from_value::<SubscribeInput>(cleaned) else {
        return typed_err(
            StatusCode::OK,
            id,
            -32602,
            "Invalid subscribe parameters",
            Value::Null,
        );
    };
    let Some(raw_url) = input.delivery.url.as_deref().filter(|s| !s.is_empty()) else {
        return typed_err(
            StatusCode::OK,
            id,
            -32602,
            "Webhook delivery requires url and secret",
            json!({"field": "delivery.url"}),
        );
    };
    let Some(secret) = input.delivery.secret.as_deref().filter(|s| !s.is_empty()) else {
        return typed_err(
            StatusCode::OK,
            id,
            -32602,
            "Webhook delivery requires url and secret",
            json!({"field": "delivery.secret"}),
        );
    };
    if let Err(e) = decode_signing_secret(secret) {
        return typed_err(StatusCode::OK, id, -32602, &e.to_string(), Value::Null);
    }

    let allowed = Arc::new(allowed_hosts_from_env());
    let url = match validate_webhook_url(raw_url, &allowed).await {
        Ok(u) => u,
        Err(reason) => {
            return typed_err(StatusCode::OK, id, -32027, &reason, json!({"url": raw_url}));
        }
    };

    let ttl = Duration::from_millis(input.ttl_ms.unwrap_or(3_600_000).clamp(5_000, 86_400_000));
    let key = (input.name.clone(), url.as_str().to_owned());

    // Check if an active subscription already exists (refresh + optional secret rotation).
    let existing = {
        let reg = registry();
        let map = reg.subs.lock().unwrap();
        map.get(&key)
            .cloned()
            .filter(|s| !s.lock().unwrap().cancelled)
    };

    if let Some(sub) = existing {
        let resp = {
            let mut s = sub.lock().unwrap();
            s.expires_at = Instant::now() + ttl;
            s.arguments = input.arguments;
            if s.current_secret != secret {
                let old = std::mem::replace(&mut s.current_secret, secret.to_owned());
                s.previous_secret = Some((old, Instant::now() + Duration::from_secs(300)));
            }
            s.wake.notify_waiters();
            json!({
                "id": s.id,
                "cursor": s.cursor,
                "ttlMs": ttl.as_millis() as u64,
                "deliveryStatus": {
                    "active": true,
                    "verified": s.verified,
                    "lastError": s.last_error,
                    "lastDeliveryAt": s.last_delivery_at,
                }
            })
        };
        return rpc_ok(id, resp, stateless);
    }

    // Bootstrap starting cursor from MilestoneHub (`None` starts from now!).
    let init_params = Params {
        name: input.name.clone(),
        cursor: input.cursor.clone(),
        max_age_ms: input.max_age_ms,
        max_events: Some(1),
        heartbeat_ms: None,
        arguments: input.arguments.clone(),
    };
    let bootstrap_cursor = match read_page_for_webhook(app.clone(), init_params).await {
        Ok(p) => {
            if input.cursor.is_none() {
                p.cursor
            } else {
                p.replay_start()
            }
        }
        Err(e) => {
            return typed_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                id,
                -32603,
                &e.to_string(),
                Value::Null,
            );
        }
    };

    let sub_id = format!("sub_{}", Uuid::new_v4().simple());
    let Ok(client) = build_webhook_client(allowed.clone()) else {
        return typed_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            id,
            -32603,
            "Webhook client unavailable",
            Value::Null,
        );
    };
    if let Err(reason) = verify_endpoint(&client, &url, secret, &input.name, &sub_id).await {
        return typed_err(
            StatusCode::OK,
            id,
            -32028,
            &reason,
            json!({"url": url.as_str()}),
        );
    }

    let state = Arc::new(Mutex::new(SubscriptionState {
        id: sub_id.clone(),
        name: input.name,
        url: url.as_str().to_owned(),
        current_secret: secret.to_owned(),
        previous_secret: None,
        cursor: bootstrap_cursor.clone(),
        arguments: input.arguments,
        expires_at: Instant::now() + ttl,
        last_error: None,
        last_delivery_at: None,
        verified: true,
        cancelled: false,
        wake: Arc::new(Notify::new()),
    }));
    registry().subs.lock().unwrap().insert(key, state.clone());
    spawn_delivery_worker(app, state, allowed);

    rpc_ok(
        id,
        json!({
            "id": sub_id,
            "cursor": bootstrap_cursor,
            "ttlMs": ttl.as_millis() as u64,
            "deliveryStatus": {
                "active": true,
                "verified": true,
                "lastError": null,
                "lastDeliveryAt": null,
            }
        }),
        stateless,
    )
}

pub(super) async fn handle_unsubscribe(
    raw_params: &Value,
    id: &Value,
    stateless: bool,
) -> Response {
    let mut cleaned = raw_params.clone();
    if let Some(obj) = cleaned.as_object_mut() {
        obj.remove("_meta");
    }
    let Ok(input) = serde_json::from_value::<UnsubscribeInput>(cleaned) else {
        return typed_err(
            StatusCode::OK,
            id,
            -32602,
            "Invalid unsubscribe parameters",
            Value::Null,
        );
    };
    if input.delivery.mode != "webhook" {
        return typed_err(
            StatusCode::OK,
            id,
            -32026,
            "Unsupported delivery mode",
            json!({"feature": "deliveryMode", "value": input.delivery.mode}),
        );
    }
    let Some(raw_url) = input.delivery.url.as_deref() else {
        return typed_err(
            StatusCode::OK,
            id,
            -32602,
            "Missing delivery.url",
            Value::Null,
        );
    };
    let Ok(parsed) = Url::parse(raw_url) else {
        return typed_err(
            StatusCode::OK,
            id,
            -32602,
            "Invalid delivery.url",
            Value::Null,
        );
    };
    let key = (input.name, parsed.as_str().to_owned());
    let removed = registry().subs.lock().unwrap().remove(&key);
    let Some(sub) = removed else {
        return typed_err(
            StatusCode::OK,
            id,
            -32024,
            "Subscription not found",
            Value::Null,
        );
    };
    let mut s = sub.lock().unwrap();
    s.cancelled = true;
    s.wake.notify_waiters();
    rpc_ok(id, json!({"unsubscribed": true, "id": s.id}), stateless)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_blocklist_rejects_loopback_private_link_local_and_ipv4_mapped_v6() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.5.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            let ip: IpAddr = blocked.parse().unwrap();
            assert!(!is_globally_routable_ip(ip), "{blocked} should be rejected");
        }
        for public in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let ip: IpAddr = public.parse().unwrap();
            assert!(is_globally_routable_ip(ip), "{public} should be allowed");
        }
    }

    #[tokio::test]
    async fn validate_webhook_url_blocks_private_hosts_unless_allowlisted() {
        let empty = BTreeSet::new();
        assert!(
            validate_webhook_url("http://127.0.0.1:8080/hook", &empty)
                .await
                .is_err()
        );
        assert!(
            validate_webhook_url("https://127.0.0.1:8080/hook", &empty)
                .await
                .is_err()
        );

        let allowed = BTreeSet::from(["127.0.0.1".to_owned()]);
        assert!(
            validate_webhook_url("http://127.0.0.1:8080/hook", &allowed)
                .await
                .is_ok()
        );
    }

    #[test]
    fn standard_webhook_hmac_supports_whsec_and_dual_secret_rotation() {
        let s1 = format!("whsec_{}", STANDARD.encode([1u8; 32]));
        let s2 = format!("whsec_{}", STANDARD.encode([2u8; 32]));
        let sig =
            sign_standard_webhook(&[s1, s2], "msg_123", 1_700_000_000, b"{\"ok\":true}").unwrap();
        let parts: Vec<&str> = sig.split(' ').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("v1,"));
        assert!(parts[1].starts_with("v1,"));
        assert_ne!(parts[0], parts[1]);
    }
}
