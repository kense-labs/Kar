use super::*;
use crate::workbench::access::DeviceLease;
use axum::{Extension, routing::delete};

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Peer {
    pub(super) id: Uuid,
    pub(super) request_id: Uuid,
    label: String,
    origin: String,
    token: String,
}
fn origin(v: &str) -> Result<url::Url> {
    let url = url::Url::parse(v).map_err(|_| bad("invalid remote environment address"))?;
    let local = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|a| a.is_loopback())
    });
    if url.host().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && local)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(bad("supply an HTTPS origin or a loopback HTTP origin"));
    }
    Ok(url)
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(8))
        .build()?)
}
/// Build a request only to a recorded peer origin. Treat each dynamic identifier as one path segment.
pub(in crate::workbench) fn peer_request(
    app: &Workbench,
    id: Uuid,
    method: reqwest::Method,
    segments: &[&str],
) -> Result<reqwest::RequestBuilder> {
    bound_peer_request(app,id,None,method,segments)
}
pub(in crate::workbench) fn peer_identity(db:&Catalog,id:Uuid)->Result<Uuid>{
    db.environment.links.get(&id).map(|p|p.request_id).ok_or_else(||bad("unknown environment link"))
}
pub(in crate::workbench) fn bound_peer_request(app:&Workbench,id:Uuid,expected:Option<Uuid>,method:reqwest::Method,segments:&[&str])->Result<reqwest::RequestBuilder>{
    let db = app.0.catalog.lock().unwrap();
    let p = db
        .environment
        .links
        .get(&id)
        .ok_or_else(|| bad("unknown environment link"))?;
    if expected.is_some_and(|id|p.request_id!=id){return Err(bad("remote environment pairing changed"));}
    let mut url = origin(&p.origin)?;
    if segments
        .iter()
        .any(|s| matches!(*s, "" | "." | "..") || s.contains(['/', '\\', '\0']))
    {
        return Err(bad("invalid remote path segment"));
    }
    url.path_segments_mut()
        .map_err(|_| bad("invalid linked environment origin"))?
        .clear()
        .extend(segments);
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(8))
        .read_timeout(std::time::Duration::from_secs(75))
        .build()?;
    Ok(http.request(method, url).bearer_auth(&p.token))
}
async fn request(peer: &Peer, method: reqwest::Method, path: &str) -> Result<Value> {
    let url = origin(&peer.origin)?
        .join(path)
        .map_err(|_| bad("invalid environment route"))?;
    let r = client()?
        .request(method, url)
        .bearer_auth(&peer.token)
        .send()
        .await
        .map_err(|_| bad("remote environment is unreachable"))?;
    if !r.status().is_success() {
        return Err(bad(&format!(
            "remote environment returned HTTP {}",
            r.status().as_u16()
        )));
    }
    let mut bytes = vec![];
    let mut r = r;
    while let Some(row) = r.chunk().await? {
        if bytes.len() + row.len() > 256000 {
            return Err(bad("remote environment description exceeded the limit"));
        }
        bytes.extend(row);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub(super) async fn read(app: &Workbench, id: Uuid) -> Result<Value> {
    let peer = app
        .0
        .catalog
        .lock()
        .unwrap()
        .environment
        .links
        .get(&id)
        .cloned()
        .ok_or_else(|| bad("unknown environment link"))?;
    let info = request(&peer, reqwest::Method::GET, "environment").await?;
    if info["environment_id"] != id.to_string() {
        return Err(bad("linked endpoint identity changed"));
    }
    Ok(info)
}
pub(super) async fn list(app: &Workbench) -> Result<Value> {
    let env = id(app)?;
    let peers: Vec<_> = app
        .0
        .catalog
        .lock()
        .unwrap()
        .environment
        .links
        .values()
        .cloned()
        .collect();
    let mut tasks = tokio::task::JoinSet::new();
    for peer in peers {
        tasks.spawn(async move {
            let ok = match request(&peer, reqwest::Method::GET, "environment").await {
                Ok(v) => v["environment_id"] == peer.id.to_string(),
                Err(_) => false,
            };
            json!({"environment_id":peer.id,"label":peer.label,"origin":peer.origin,"online":ok})
        });
    }
    let mut found = vec![];
    while let Some(row) = tasks.join_next().await {
        found.push(row.map_err(|_| bad("remote environment query failed"))?);
    }
    found.sort_by_key(|p| p["environment_id"].as_str().unwrap_or("").to_string());
    Ok(json!({"environment_id":env,"links":found}))
}
pub(super) fn request_link(app: &Workbench, c: &Caller, g: &Grant, input: Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Link {
        request_id: Uuid,
        environment_id: Option<Uuid>,
        hint: Option<String>,
        url: Option<String>,
    }
    let req: Link = serde_json::from_value(input)?;
    if req.hint.as_deref().is_some_and(|s| s.len() > 512) {
        return Err(bad("environment hint too long"));
    }
    if let Some(url) = &req.url {
        origin(url)?;
    }
    let env = id(app)?;
    if let Some(remote) = req.environment_id {
        if remote == env {
            return Err(bad("cannot pair the host with itself"));
        }
        let db = app.0.catalog.lock().unwrap();
        check(&db, c, g)?;
        if db.environment.links.contains_key(&remote) {
            return Ok(json!({"status":"linked","environment_id":remote}));
        }
    }
    cards::create(
        app,
        c,
        g,
        req.request_id,
        "link",
        json!({"environment_id":req.environment_id,"hint":req.hint,"url":req.url}),
        true,
        None,
    )
}
pub(super) async fn approve(app: &Workbench, card: &cards::Card, input: Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Approval {
        url: String,
        credential: String,
    }
    let a: Approval = serde_json::from_value(input)?;
    let address = origin(&a.url)?;
    if card.details["url"]
        .as_str()
        .is_some_and(|s| origin(s).ok().as_ref() != Some(&address))
    {
        return Err(bad("owner approval names a different remote origin"));
    }
    if a.credential.len() > 4096 || a.credential.trim().is_empty() {
        return Err(bad("invalid pairing credential"));
    }
    let client = client()?;
    let local = id(app)?;
    let r = client
        .post(address.join("access/pair").unwrap())
        .json(&json!({"credential":a.credential,"label":format!("Kar {}",local),"purpose":"peer-link"}))
        .send()
        .await
        .map_err(|_| bad("remote pairing outcome unknown; inspect the remote device list"))?;
    if !r.status().is_success() {
        return Err(bad("remote environment refused pairing"));
    }
    let mut size = 0;
    let mut data = Vec::new();
    let mut r = r;
    while let Some(chunk) = r.chunk().await? {
        size += chunk.len();
        if size > 8192 {
            return Err(bad("remote pairing reply is too large"));
        }
        data.extend(chunk);
    }
    let auth: Value = serde_json::from_slice(&data)?;
    let token = auth["token"]
        .as_str()
        .filter(|s| s.len() <= 4096 && !s.trim().is_empty())
        .ok_or_else(|| bad("remote pairing returned no device credential"))?;
    let mut peer = Peer {
        id: Uuid::nil(),
        request_id: card.id,
        origin: address.to_string(),
        token: token.into(),
        label: String::new(),
    };
    let done = async {
        let info = request(&peer, reqwest::Method::GET, "environment").await?;
        let remote: Uuid = serde_json::from_value(info["environment_id"].clone())?;
        if remote == local
            || card.details["environment_id"]
                .as_str()
                .is_some_and(|v| v != remote.to_string())
        {
            return Err(bad(
                "remote identity does not match the requested environment",
            ));
        }
        peer.id = remote;
        peer.label = info["label"]
            .as_str()
            .unwrap_or("Kar")
            .chars()
            .take(160)
            .collect();
        app.update(|db| {
            let env = &mut db.environment;
            if env.links.len() >= 32 || env.links.contains_key(&remote) {
                return Err(bad("environment already linked or link limit reached"));
            }
            env.links.insert(remote, peer.clone());
            Ok(json!({"status":"linked","environment_id":remote,"label":peer.label}))
        })
    }
    .await;
    if done.is_err() {
        let _ = request(&peer, reqwest::Method::DELETE, "access/current-device").await;
    }
    done
}
async fn revoke_remote(peer: &Peer) -> bool {
    if request(peer, reqwest::Method::DELETE, "access/current-device")
        .await
        .is_ok()
    {
        return true;
    }
    // The remote revocation may cancel the DELETE response stream on this very device.
    let probe = async {
        let url = origin(&peer.origin)?
            .join("environment")
            .map_err(|_| bad("invalid environment route"))?;
        let response = client()?.get(url).bearer_auth(&peer.token).send().await?;
        Ok::<_, Error>(response.status() == reqwest::StatusCode::UNAUTHORIZED)
    }
    .await;
    probe.unwrap_or(false)
}
pub(super) async fn unlink(app: &Workbench, c: &Caller, g: &Grant, input: Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Unlink {
        environment_id: Uuid,
    }
    let m: Unlink = serde_json::from_value(input)?;
    let peer = app.update(|db| {
        check(db, c, g)?;
        Ok(db.environment.links.remove(&m.environment_id))
    })?;
    let revoked = if let Some(peer) = peer {
        revoke_remote(&peer).await
    } else {
        false
    };
    Ok(
        json!({"environment_id":m.environment_id,"linked":false,"remote_revocation_confirmed":revoked}),
    )
}
async fn revoke_device(
    State(app): State<Workbench>,
    Extension(device): Extension<DeviceLease>,
) -> Reply {
    app.0.access.revoke_device(device.id)?;
    Ok(Json(json!({"revoked":true})))
}
pub(in crate::workbench) fn session_routes() -> Router<Workbench> {
    Router::new().route("/access/current-device", delete(revoke_device))
}
