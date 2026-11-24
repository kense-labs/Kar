use super::*;
use crate::{Cursor, Direction, Subscription};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;
use url::{Host, Url};

type FlowKey = (Uuid, Uuid, Uuid);
fn owner(headers: &HeaderMap, device: Option<Extension<DeviceLease>>) -> Result<String> {
    let client = super::super::acp_registry::auth::client_key(headers)?
        .ok_or_else(|| bad("Kar-Auth-Client is required for MCP sign-in"))?;
    Ok(format!(
        "{}:{client}",
        device
            .map(|Extension(d)| d.id.to_string())
            .unwrap_or_else(|| "owner".into())
    ))
}
#[derive(Default)]
pub(in crate::workbench) struct Flows(Mutex<HashMap<Uuid, Arc<Flow>>>);
struct Flow {
    chat: Chat,
    stream: Uuid,
    server: String,
    owner: String,
    born: Instant,
    done: AtomicBool,
    prior_attempt: bool,
    phase: AsyncMutex<Phase>,
}
#[derive(Default)]
enum Phase {
    #[default]
    New,
    Ready(Ready),
    Complete,
    Failed(String),
    Cancelled,
}
struct Ready {
    url: Url,
    redirect: Option<(Url, String)>,
    cursor: Option<Cursor>,
    callback_sent: bool,
    callback_key: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    stream: Uuid,
    server: String,
    id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Complete {
    callback_url: String,
}
impl Flows {
    fn claim(&self, key: Chat, input: &Start, owner: String) -> Result<Arc<Flow>> {
        let mut flows = self.0.lock().unwrap();
        let prior_attempt = flows
            .values()
            .any(|f| f.stream == input.stream && f.server == input.server);
        flows.retain(|_, f| f.born.elapsed() < Duration::from_secs(30 * 60));
        if let Some(f) = flows.get(&input.id) {
            if f.chat != key
                || f.stream != input.stream
                || f.server != input.server
                || f.owner != owner
            {
                return Err(bad(
                    "MCP sign-in request belongs to another client or controller",
                ));
            }
            return Ok(f.clone());
        }
        if flows.values().any(|f| {
            f.stream == input.stream && f.server == input.server && !f.done.load(Ordering::Acquire)
        }) {
            return Err(Error::Busy);
        }
        if flows.len() >= 1024 {
            return Err(Error::Busy);
        }
        let f = Arc::new(Flow {
            chat: key,
            stream: input.stream,
            server: input.server.clone(),
            owner,
            born: Instant::now(),
            prior_attempt,
            done: Default::default(),
            phase: Default::default(),
        });
        flows.insert(input.id, f.clone());
        Ok(f)
    }
    fn find(&self, (p, c, id): FlowKey, owner: &str) -> Result<Arc<Flow>> {
        self.0
            .lock()
            .unwrap()
            .get(&id)
            .filter(|f| {
                f.chat == (p, c) && f.owner == owner && f.born.elapsed() < Duration::from_secs(1800)
            })
            .cloned()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "unknown private sign-in flow")
                    .into()
            })
    }
}
fn url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| bad("invalid native OAuth URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(bad("invalid native OAuth URL"));
    }
    Ok(url)
}
fn unique(u: &Url, key: &str) -> Option<String> {
    let mut values = u.query_pairs().filter(|(k, _)| k == key);
    let value = values.next()?.1.into_owned();
    (!value.is_empty() && values.next().is_none()).then_some(value)
}
fn callback(raw: &str, redirect: Option<&(Url, String)>) -> Result<Url> {
    if raw.len() > 16384 {
        return Err(bad("OAuth callback is too large"));
    }
    let raw = raw.trim();
    let scheme = redirect.map(|(u, _)| u.scheme()).unwrap_or("http");
    let pasted = if raw.contains("://") {
        raw.into()
    } else {
        format!("{scheme}://{raw}")
    };
    let result = url(&pasted)?;
    if let Some((target, state)) = redirect {
        let pairs = |key: &str| result.query_pairs().filter(|(k, _)| k == key).count();
        if result.origin() != target.origin()
            || result.path() != target.path()
            || result.fragment().is_some()
            || unique(&result, "state").as_deref() != Some(state)
            || !matches!((pairs("code"), pairs("error")), (1, 0) | (0, 1))
            || unique(&result, "code")
                .or_else(|| unique(&result, "error"))
                .is_none()
        {
            return Err(bad("OAuth callback does not match this sign-in"));
        }
    }
    Ok(result)
}
async fn connected(s: &Session, server: &str) -> Result<bool> {
    let body = match s.protocol() {
        Protocol::Codex => {
            s.pages(
                "mcpServerStatus/list",
                json!({"threadId":s.id(),"serverName":server,"detail":"toolsAndAuthOnly"}),
            )
            .await?
        }
        Protocol::Claude => s.rpc("mcp_status", json!({})).await?["mcpServers"].take(),
        _ => {
            return Err(Error::Unsupported(
                "this provider does not expose MCP sign-in on its session".into(),
            ));
        }
    };
    Ok(body.as_array().is_some_and(|a| {
        a.iter().any(|r| {
            r["name"] == server && (r["runtimeStatus"] == "connected" || r["status"] == "connected")
        })
    }))
}
async fn start_native(s: &Session, server: &str) -> Result<Phase> {
    let cursor = s.latest_cursor();
    let reply = match s.protocol() {
        Protocol::Codex => {
            if connected(s, server).await.unwrap_or(false) {
                return Ok(Phase::Complete);
            }
            s.rpc(
                "mcpServer/oauth/login",
                json!({"threadId":s.id(),"name":server}),
            )
            .await?["authorizationUrl"]
                .take()
        }
        Protocol::Claude => {
            let r = s
                .rpc("mcp_authenticate", json!({"serverName":server}))
                .await?;
            if r["requiresUserAction"] != true {
                return Ok(Phase::Complete);
            }
            r["authUrl"].clone()
        }
        _ => {
            return Err(Error::Unsupported(
                "this provider does not expose MCP sign-in on its session".into(),
            ));
        }
    };
    let auth = url(reply
        .as_str()
        .ok_or_else(|| bad("provider omitted the authorization URL"))?)?;
    let redirect = unique(&auth, "redirect_uri")
        .and_then(|raw| url(&raw).ok())
        .zip(unique(&auth, "state"));
    Ok(Phase::Ready(Ready {
        url: auth,
        redirect,
        cursor,
        callback_sent: false,
        callback_key: None,
    }))
}
fn note(r: &crate::Record, s: &Session, server: &str) -> Option<bool> {
    let packet = &r.native;
    (r.direction == Direction::FromRuntime
        && packet["method"] == "mcpServer/oauthLogin/completed"
        && packet["params"]["name"] == server
        && packet["params"]["threadId"]
            .as_str()
            .is_none_or(|t| t == s.id()))
    .then(|| packet["params"]["success"].as_bool())
    .flatten()
}
async fn codex_outcome(
    mut sub: Subscription,
    s: &Session,
    server: &str,
    secs: u64,
    prior_attempt: bool,
) -> Result<bool> {
    tokio::time::timeout(Duration::from_secs(secs), async {
        while let Some(r) = sub.next().await {
            if let Some(ok) = note(&r, s, server) {
                if !prior_attempt {
                    return Ok(ok);
                }
                if connected(s, server).await? {
                    return Ok(true);
                }
                // The notification has no attempt ID. A cancelled attempt can settle late.
            }
        }
        Err(Error::Closed(
            "the OAuth controller stopped before reporting its result".into(),
        ))
    })
    .await
    .map_err(|_| Error::Timeout("waiting for Codex MCP sign-in confirmation".into()))?
}
async fn deliver(value: Url) -> Result<bool> {
    let mut b = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10));
    match value.host() {
        Some(Host::Ipv4(ip)) if ip == Ipv4Addr::LOCALHOST => {}
        Some(Host::Ipv6(ip)) if ip == Ipv6Addr::LOCALHOST => {}
        Some(Host::Domain("localhost")) => {
            let port = value
                .port_or_known_default()
                .ok_or_else(|| bad("invalid OAuth callback port"))?;
            let addrs = [
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
            ];
            b = b.resolve_to_addrs("localhost", &addrs);
        }
        _ => {
            return Err(bad(
                "Codex requires the callback to be opened by a browser that can reach its native host",
            ));
        }
    }
    Ok(b.build()
        .map_err(|_| bad("cannot open a private OAuth client"))?
        .get(value)
        .send()
        .await
        .is_ok())
}
async fn complete_native(
    s: &Session,
    server: &str,
    raw: &str,
    ready: &mut Ready,
    prior_attempt: bool,
) -> Result<bool> {
    if s.protocol() == Protocol::Codex {
        let mut sub = s.subscribe(ready.cursor.as_ref())?;
        while let Some(r) = sub.try_next() {
            if let Some(ok) = note(&r, s, server) {
                if !prior_attempt {
                    return Ok(ok);
                }
                if connected(s, server).await? {
                    return Ok(true);
                }
            }
        }
        let redirect=ready.redirect.as_ref().ok_or_else(||bad("Codex did not advertise a transferable OAuth redirect; finish in the original browser"))?;
        let cb = callback(raw, Some(redirect))?;
        let k = super::super::access::digest(cb.as_str());
        if ready.callback_key.as_ref().is_some_and(|prior| prior != &k) {
            return Err(bad(
                "a different callback was already submitted for this flow",
            ));
        }
        if !ready.callback_sent {
            ready.callback_sent = deliver(cb).await?;
            ready.callback_key = Some(k);
        }
        return codex_outcome(
            sub,
            s,
            server,
            if ready.callback_sent { 30 } else { 2 },
            prior_attempt,
        )
        .await;
    }
    if connected(s, server).await? {
        return Ok(true);
    }
    let cb = callback(raw, ready.redirect.as_ref())?;
    let key = super::super::access::digest(cb.as_str());
    if ready.callback_key.as_ref().is_some_and(|k| k != &key) {
        return Err(bad("another callback was already submitted"));
    }
    if !ready.callback_sent {
        s.rpc(
            "mcp_oauth_callback_url",
            json!({"serverName":server,"callbackUrl":cb.as_str()}),
        )
        .await?;
        ready.callback_sent = true;
        ready.callback_key = Some(key);
    }
    s.rpc("mcp_reconnect", json!({"serverName":server})).await?;
    Ok(true)
}
fn show(id: Uuid, phase: &Phase) -> Result<Json<Value>> {
    match phase {
        Phase::Ready(r) => Ok(Json(
            json!({"flow":id,"status":"authorization_required","authorization_url":r.url.as_str()}),
        )),
        Phase::Complete => Ok(Json(
            json!({"flow":id,"status":"complete","authorization_url":null}),
        )),
        Phase::Failed(msg) => Err(bad(msg)),
        Phase::Cancelled => Err(bad("MCP sign-in was cancelled by its client")),
        Phase::New => Err(Error::Busy),
    }
}
pub(super) async fn start(
    State(w): State<Workbench>,
    Path(key): Path<Chat>,
    device: Option<Extension<DeviceLease>>,
    headers: HeaderMap,
    Json(input): Json<Start>,
) -> Reply {
    let s = active(&w, key, input.stream)?;
    if !matches!(s.protocol(), Protocol::Codex | Protocol::Claude) {
        return Err(
            Error::Unsupported("this provider has no native session MCP sign-in".into()).into(),
        );
    }
    if input.server.trim().is_empty() || input.server.len() > 256 {
        return Err(bad("invalid MCP server name").into());
    }
    let f =
        w.0.provider_oauth
            .claim(key, &input, owner(&headers, device)?)?;
    let id = input.id;
    Ok(tokio::spawn(async move {
        let mut phase = f.phase.lock().await;
        if matches!(*phase, Phase::New) {
            match start_native(&s, &f.server).await {
                Ok(next) => *phase = next,
                Err(e) => {
                    *phase = Phase::Failed(e.to_string());
                    f.done.store(true, Ordering::Release);
                    return Err(e);
                }
            }
            f.done
                .store(matches!(*phase, Phase::Complete), Ordering::Release);
        }
        show(id, &phase)
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??)
}
pub(super) async fn complete(
    State(w): State<Workbench>,
    Path(key): Path<FlowKey>,
    device: Option<Extension<DeviceLease>>,
    headers: HeaderMap,
    Json(input): Json<Complete>,
) -> Reply {
    let f = w.0.provider_oauth.find(key, &owner(&headers, device)?)?;
    let s = active(&w, (key.0, key.1), f.stream)?;
    Ok(tokio::spawn(async move {
        let mut phase = f.phase.lock().await;
        if let Phase::Ready(r) = &mut *phase {
            let ok =
                complete_native(&s, &f.server, &input.callback_url, r, f.prior_attempt).await?;
            *phase = if ok {
                Phase::Complete
            } else {
                Phase::Failed("Codex reported that the MCP sign-in failed".into())
            };
            f.done.store(true, Ordering::Release);
        }
        show(key.2, &phase)
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??)
}
pub(super) async fn cancel(
    State(w): State<Workbench>,
    Path(key): Path<FlowKey>,
    device: Option<Extension<DeviceLease>>,
    headers: HeaderMap,
) -> Reply {
    let f = w.0.provider_oauth.find(key, &owner(&headers, device)?)?;
    let mut phase = f.phase.lock().await;
    *phase = Phase::Cancelled;
    f.done.store(true, Ordering::Release);
    Ok(Json(json!({"flow":key.2,"status":"cancelled"})))
}
