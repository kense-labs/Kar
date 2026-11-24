//! Owner-only transfer of an independently authorized ChatGPT registration between Kar workbenches.
use super::{Workbench, acp_registry::auth::client_key, http::ApiError};
use crate::{Error, Result, SessionOptions, chat::ChatStore};
use axum::{
    Json, Router,
    extract::{Path as Param, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
mod flow;
mod oidc;
mod store;
mod transfer;
pub(super) use flow::Jobs;
type Reply = std::result::Result<Json<Value>, ApiError>;
fn bad(s: &str) -> Error {
    Error::Refused(s.into())
}
fn unknown() -> Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, "unknown ChatGPT handoff").into()
}
fn random<const N: usize>() -> Result<[u8; N]> {
    let mut v = [0u8; N];
    getrandom::fill(&mut v).map_err(|_| bad("secure randomness unavailable"))?;
    Ok(v)
}
fn valid_client(s: &str) -> bool {
    s.strip_prefix("oaiapp_").is_some_and(|r| {
        !r.is_empty()
            && r.len() <= 128
            && r.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    })
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn owner(w: &Workbench, p: Uuid, id: &str, headers: &HeaderMap) -> Result<flow::Owner> {
    let proj = w.project(p)?;
    w.workspace(&proj)?;
    crate::provider_instance::for_launch("codex", id)?;
    Ok((
        p,
        id.to_owned(),
        client_key(headers)?
            .ok_or_else(|| bad("Kar-Auth-Client is required for ChatGPT profiles"))?,
    ))
}
fn environment(chats: &ChatStore) -> Result<Uuid> {
    let dir = chats.directory().join("workbench/chatgpt");
    store::ensure(&dir)?;
    let path = dir.join("environment.json");
    crate::workbench::mcp_apps::record::capture(&path, &Uuid::new_v4())?;
    crate::workbench::mcp_apps::record::read(&path)?
        .ok_or_else(|| bad("missing Kar environment identity"))
}
pub(crate) fn managed(chats: &ChatStore, id: &str) -> Result<bool> {
    Ok(store::dir(chats, id).join("profiles.sealed").try_exists()?)
}
pub(crate) fn status(chats: &ChatStore, id: &str) -> Result<Value> {
    let db = store::read(&store::dir(chats, id))?;
    Ok(match db.selected() {
        Some(p) => json!({"kind":"logged_in","source":"managed_chatgpt","account":p.view()}),
        None => json!({"kind":"logged_out","source":"managed_chatgpt"}),
    })
}
pub(crate) fn native_quota(chats: &ChatStore, id: &str) -> Result<()> {
    if managed(chats, id)? {
        return Err(Error::Unsupported(
            "managed token-sharing connections do not expose native account reset credits".into(),
        ));
    }
    Ok(())
}
pub(crate) async fn logout(chats: &ChatStore, id: &str) -> Result<Value> {
    let dir = store::dir(chats, id);
    let _lock = store::lock(&dir)?;
    let mut db = store::read(&dir)?;
    let Some(client) = db.active.take() else {
        return Ok(json!({"kind":"logged_out"}));
    };
    let p = db
        .saved
        .get_mut(&client)
        .ok_or_else(|| bad("missing active ChatGPT profile"))?;
    let confirmed = if let Some(refresh) = &p.refresh {
        match oidc::Issuer::discover().await {
            Ok(issuer) if issuer.doc.issuer == p.issuer => issuer.revoke(&client, refresh).await,
            _ => false,
        }
    } else {
        true
    };
    p.refresh = None;
    p.access.clear();
    p.id_token.clear();
    p.scopes.clear();
    p.expires = 0;
    p.earliest = None;
    db.last = Some(client);
    store::save(&dir, &db)?;
    Ok(json!({"kind":"logged_out","remote_revocation_confirmed":confirmed}))
}
async fn profiles(
    State(w): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    headers: HeaderMap,
) -> Reply {
    owner(&w, p, &id, &headers)?;
    let db = store::read(&store::dir(&w.0.chats, &id))?;
    Ok(Json(
        json!({"active":db.active,"profiles":db.saved.values().map(store::Profile::view).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    client_id: Option<String>,
    #[serde(default)]
    different_account: bool,
}
async fn prepare(
    State(w): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    headers: HeaderMap,
    Json(q): Json<Prepare>,
) -> Reply {
    let own = owner(&w, p, &id, &headers)?;
    let db = store::read(&store::dir(&w.0.chats, &id))?;
    if q.different_account && q.client_id.is_some() {
        return Err(bad("select a saved registration or a new account").into());
    }
    let selected = if let Some(c) = &q.client_id {
        Some(
            db.saved
                .get(c)
                .ok_or_else(|| bad("unknown ChatGPT registration"))?,
        )
    } else if q.different_account {
        None
    } else {
        db.selected()
            .or_else(|| db.last.as_ref().and_then(|c| db.saved.get(c)))
    };
    let (public_key, secret) = transfer::receiver()?;
    let target = flow::Target {
        environment: environment(&w.0.chats)?,
        instance: id,
        offer: Uuid::new_v4(),
        expires: now() + 600,
        public_key,
        nonce: B64.encode(random::<32>()?),
        client_id: selected.map(|s| s.client.clone()),
        subject: selected.map(|s| s.subject.clone()),
        email: selected.and_then(|s| s.email.clone()),
    };
    let mut offers = w.0.chatgpt.offers.lock().unwrap();
    offers.retain(|_, v| v.target.expires > now());
    if offers.len() >= 64 {
        return Err(Error::Busy.into());
    }
    offers.insert(
        target.offer,
        flow::Offer {
            owner: own,
            target: target.clone(),
            secret,
            result: None,
        },
    );
    Ok(Json(json!({"target":target})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    target: flow::Target,
}
async fn start(
    State(w): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    headers: HeaderMap,
    Json(q): Json<Start>,
) -> Reply {
    let own = owner(&w, p, &id, &headers)?;
    let (key, job) = flow::start(q.target, own).await?;
    let mut jobs = w.0.chatgpt.jobs.lock().unwrap();
    jobs.retain(|_, j| j.expires > now());
    if jobs.len() >= 64 {
        return Err(Error::Busy.into());
    }
    let state = job.status.borrow().clone();
    jobs.insert(key, job);
    Ok(Json(json!({"flow":key,"state":state})))
}
type FlowPath = (Uuid, String, Uuid);
fn with_job<T>(
    w: &Workbench,
    path: &FlowPath,
    headers: &HeaderMap,
    operation: impl FnOnce(&flow::Job) -> Result<T>,
) -> Result<T> {
    let own = owner(w, path.0, &path.1, headers)?;
    let jobs = w.0.chatgpt.jobs.lock().unwrap();
    operation(
        jobs.get(&path.2)
            .filter(|j| j.expires > now() && j.owner == own)
            .ok_or_else(unknown)?,
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Callback {
    callback_url: String,
}
async fn callback(
    State(w): State<Workbench>,
    Param(path): Param<FlowPath>,
    headers: HeaderMap,
    Json(c): Json<Callback>,
) -> Reply {
    with_job(&w, &path, &headers, |j| j.callback.submit(&c.callback_url))?;
    Ok(Json(json!({"accepted":true})))
}
async fn cancel(
    State(w): State<Workbench>,
    Param(path): Param<FlowPath>,
    headers: HeaderMap,
) -> Reply {
    let own = owner(&w, path.0, &path.1, &headers)?;
    let mut jobs = w.0.chatgpt.jobs.lock().unwrap();
    if jobs.get(&path.2).is_none_or(|j| j.owner != own) {
        return Err(unknown().into());
    }
    jobs.remove(&path.2);
    Ok(Json(json!({"removed":true})))
}
async fn events(
    State(w): State<Workbench>,
    Param(path): Param<FlowPath>,
    headers: HeaderMap,
) -> std::result::Result<
    Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>,
    ApiError,
> {
    let rx = with_job(&w, &path, &headers, |j| Ok(j.status.clone()))?;
    Ok(Sse::new(futures_util::stream::unfold(
        Some((rx, true)),
        |state| async move {
            let (mut rx, first) = state?;
            if !first && rx.changed().await.is_err() {
                return None;
            }
            let v = rx.borrow_and_update().clone();
            let term = matches!(v["phase"].as_str(), Some("finished" | "failed"));
            Some((
                Ok(Event::default().event("handoff").data(v.to_string())),
                (!term).then_some((rx, false)),
            ))
        },
    ))
    .keep_alive(KeepAlive::default()))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    offer: Uuid,
    profile: transfer::Envelope,
}
async fn import(
    State(w): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    headers: HeaderMap,
    Json(q): Json<Import>,
) -> Reply {
    let own = owner(&w, p, &id, &headers)?;
    let signature = crate::workbench::mcp_apps::record::hash(&json!(q.profile));
    let (target, bytes) = {
        let mut offers = w.0.chatgpt.offers.lock().unwrap();
        let o = offers
            .get_mut(&q.offer)
            .filter(|o| o.owner == own && o.target.expires > now())
            .ok_or_else(unknown)?;
        if let Some((s, r)) = &o.result {
            if s != &signature {
                return Err(
                    bad("this handoff was already completed with a different profile").into(),
                );
            }
            return Ok(Json(r.clone()));
        }
        let aad = o.target.aad();
        let key = o.secret.key(&q.profile.public, &aad)?;
        (
            o.target.clone(),
            store::decrypt(key, &aad, &q.profile.body)?,
        )
    };
    let profile: store::Profile =
        serde_json::from_slice(&bytes).map_err(|_| bad("invalid transferred ChatGPT profile"))?;
    if !valid_client(&profile.client)
        || profile.expires <= now()
        || !profile.scopes.iter().any(|s| s == oidc::SCOPE)
        || profile.access.is_empty()
        || profile.access.chars().any(char::is_whitespace)
        || target
            .client_id
            .as_ref()
            .is_some_and(|c| c != &profile.client)
        || target
            .subject
            .as_ref()
            .is_some_and(|s| s != &profile.subject)
    {
        return Err(bad("expired or conflicting transferred ChatGPT registration").into());
    }
    let issuer = oidc::Issuer::discover().await?;
    if issuer.doc.issuer != profile.issuer {
        return Err(bad("transferred issuer does not match destination").into());
    }
    let identity = issuer
        .identity(
            &profile.id_token,
            &profile.client,
            Some(&target.nonce),
            &profile.access,
        )
        .await?;
    if identity.sub != profile.subject || identity.email != profile.email {
        return Err(bad("signed ChatGPT identity differs from transfer").into());
    }
    let instance = crate::provider_instance::for_launch("codex", &id)?;
    let mut opts = SessionOptions::new(w.project(p)?.root);
    instance.configure(&mut opts);
    let binding = crate::account::binding::Key::new("codex", &opts);
    let dir = store::dir(&w.0.chats, &id);
    let _disk = store::lock(&dir)?;
    {
        let offers = w.0.chatgpt.offers.lock().unwrap();
        let o = offers
            .get(&q.offer)
            .filter(|o| o.target.expires > now())
            .ok_or_else(unknown)?;
        if let Some((s, r)) = &o.result {
            if s != &signature {
                return Err(bad("handoff already consumed").into());
            }
            return Ok(Json(r.clone()));
        }
    }
    let mut db = store::read(&dir)?;
    if db
        .saved
        .get(&profile.client)
        .is_some_and(|other| other.subject != profile.subject)
    {
        return Err(bad("saved registration belongs to a different account").into());
    }
    if !db.saved.contains_key(&profile.client) && db.saved.len() >= 30 {
        return Err(bad("saved ChatGPT profile limit reached").into());
    }
    let result = json!({"phase":"imported","profile":profile.view()});
    // Only a verified change to the active identity can stop live native sessions.
    if db
        .selected()
        .is_none_or(|s| s.revision() != profile.revision())
    {
        let _change = w.0.chats.accounts.change(binding).await?;
        if target.expires <= now() {
            return Err(bad("handoff expired").into());
        }
        db.last = Some(profile.client.clone());
        db.active = Some(profile.client.clone());
        db.saved.insert(profile.client.clone(), profile);
        store::save(&dir, &db)?;
    }
    let mut offers = w.0.chatgpt.offers.lock().unwrap();
    let o = offers.get_mut(&q.offer).ok_or_else(unknown)?;
    if o.result.as_ref().is_some_and(|(s, _)| s != &signature) {
        return Err(bad("handoff already consumed").into());
    }
    o.result = Some((signature, result.clone()));
    Ok(Json(result))
}
pub(super) fn routes() -> Router<Workbench> {
    let base = "/projects/{project}/accounts/{runtime}/chatgpt";
    Router::new()
        .nest(
            base,
            Router::new()
                .route("/profiles", get(profiles))
                .route("/prepare", post(prepare))
                .route("/handoffs", post(start))
                .route("/handoffs/{flow}", get(events).delete(cancel))
                .route("/handoffs/{flow}/callback", post(callback))
                .route("/import", post(import)),
        )
        .layer(axum::middleware::map_response(
            |mut r: axum::response::Response| async move {
                r.headers_mut().insert(
                    "cache-control",
                    axum::http::HeaderValue::from_static("no-store"),
                );
                r
            },
        ))
}
pub(crate) fn configure(chats: &ChatStore, opts: &mut SessionOptions) -> Result<()> {
    let dir = store::dir(chats, opts.account_instance.as_deref().unwrap_or("codex"));
    let db = store::read(&dir)?;
    let Some(profile) = db.selected() else {
        if dir.join("profiles.sealed").try_exists()? {
            return Err(bad(
                "managed ChatGPT profile is signed out; reconnect before starting a chat",
            ));
        }
        return Ok(());
    };
    if !profile.scopes.iter().any(|s| s == oidc::SCOPE) {
        return Err(bad(
            "ChatGPT sharing is disabled; reconnect the active profile",
        ));
    }
    let revision = profile.revision();
    let args = [
        "managed-chatgpt-token",
        dir.to_str()
            .ok_or_else(|| bad("invalid managed profile directory"))?,
        &revision,
    ];
    let command = match std::env::var_os("KAR_CHATGPT_HELPER_BIN") {
        Some(path) => {
            let p = PathBuf::from(path);
            if !p.is_absolute() || !p.is_file() {
                return Err(bad("configure an absolute Kar token helper executable"));
            }
            p
        }
        None => std::env::current_exe()?,
    };
    let command = command
        .to_str()
        .ok_or_else(|| bad("invalid Kar executable path"))?;
    let overlay = format!(
        "model_providers.openai_token_sharing={{name=\"OpenAI Token Sharing\",base_url=\"https://api.openai.com/v1\",model_catalog_url=\"https://api.openai.com/v1/models\",wire_api=\"responses\",requires_openai_auth=false,supports_websockets=false,auth={{command={},args={},timeout_ms=30000,refresh_interval_ms=0}}}}",
        json!(command),
        json!(args)
    );
    for k in ["ACCESS_TOKEN", "OPENAI_API_KEY", "OPENAI_BASE_URL"] {
        opts.remove_env(k)?;
    }
    for item in [
        "model_provider=\"openai_token_sharing\"",
        &overlay,
        "features.api_key_model_discovery=true",
        "features.multi_agent=false",
        "features.multi_agent_v2=false",
    ] {
        opts.launch_args.extend(["-c".to_owned(), item.to_owned()]);
    }
    Ok(())
}
/// Codex auth.command entry. Only the verified profile's access token goes to this pipe.
#[doc(hidden)]
pub async fn write_managed_chatgpt_token(dir: &Path, revision: &str) -> Result<()> {
    use std::io::Write;
    if !dir.is_absolute()
        || revision.len() != 64
        || !revision.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(bad("invalid managed ChatGPT invocation"));
    }
    let token = tokio::time::timeout(Duration::from_secs(28), store::access(dir, revision))
        .await
        .map_err(|_| bad("managed ChatGPT token retrieval timed out"))??;
    std::io::stdout().lock().write_all(token.as_bytes())?;
    Ok(())
}
