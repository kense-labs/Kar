//! GitHub App device sign-in on the workbench host. Device grants are never sent to clients.
use super::*;
use axum::{Router, extract::State, routing::post};
use std::{sync::Mutex, time::Duration};
use tokio::sync::watch;
mod sessions;
use sessions::Session;

#[derive(Default)]
pub(in crate::workbench) struct Service {
    pending: Mutex<Option<Attempt>>,
    credential: tokio::sync::Mutex<()>,
    coverage: Mutex<std::collections::HashMap<String, (std::time::Instant, Option<bool>)>>,
    refused: Mutex<std::collections::HashSet<[u8; 32]>>,
}
impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(slot) = self.pending.get_mut()
            && let Some(attempt) = slot.take()
        {
            attempt.worker.abort();
        }
    }
}
struct Attempt {
    id: Uuid,
    result: watch::Sender<Option<Result<Value, ForgeError>>>,
    worker: tokio::task::JoinHandle<()>,
}
#[derive(Deserialize)]
struct Input {
    provider: String,
    host: String,
    repository: Option<String>,
}
struct Endpoints {
    client_id: String,
    oauth: Url,
    context: Context,
}
fn failure(reason: &str, detail: &str) -> ForgeError {
    ForgeError::new(
        reason,
        match reason {
            "unsupported" => 422,
            "expired" => 410,
            "denied" => 403,
            "failed" => 502,
            _ => 409,
        },
        detail,
    )
}
fn configured(c: &Context) -> Option<(String, Url)> {
    let id = std::env::var("KAR_GITHUB_APP_CLIENT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    if c.kind != "github" || !c.host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let api = &c.api_base;
    if api.as_str().trim_end_matches('/') == "https://api.github.com" {
        return Some((id, Url::parse("https://github.com/").unwrap()));
    }
    // A localhost protocol fixture is opt-in through the host's forge config. A saved session is
    // also bound to this exact API URL and client ID, so production credentials cannot follow it.
    let host = api.host_str()?;
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback());
    loopback.then(|| (id, api.clone()))
}
fn endpoints(app: &Workbench, input: &Input) -> Result<Endpoints, ForgeError> {
    if input.provider != "github" || !input.host.trim().eq_ignore_ascii_case("github.com") {
        return Err(failure(
            "unsupported",
            "device sign-in is only available for github.com",
        ));
    }
    let c = Context::located(
        "github.com".into(),
        input.repository.clone().unwrap_or_default(),
        app.0.state.clone(),
    )?;
    let (id, oauth) = configured(&c).ok_or_else(|| {
        failure(
            "unsupported",
            "the Kar GitHub App has no configured client ID for this host",
        )
    })?;
    Ok(Endpoints {
        client_id: id,
        oauth,
        context: c,
    })
}
fn url(base: &Url, parts: &[&str]) -> Url {
    let mut u = base.clone();
    u.path_segments_mut()
        .expect("validated HTTP endpoint")
        .pop_if_empty()
        .extend(parts);
    u
}
async fn form(e: &Endpoints, path: &[&str], fields: &[(&str, &str)]) -> Result<Value, ForgeError> {
    let mut response = e
        .context
        .client
        .post(url(&e.oauth, path))
        .header("Accept", "application/json")
        .form(fields)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| failure("failed", "GitHub sign-in request failed"))?;
    if !response.status().is_success() {
        return Err(failure("failed", "GitHub rejected the sign-in request"));
    }
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("failed", "interrupted GitHub sign-in response"))?
    {
        if data.len() + chunk.len() > 64 * 1024 {
            return Err(failure("failed", "oversized GitHub sign-in response"));
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| failure("failed", "invalid GitHub sign-in response"))
}
fn now() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}
fn future(ms: i64, seconds: u64) -> i64 {
    ms.saturating_add(i64::try_from(seconds.saturating_mul(1000)).unwrap_or(i64::MAX))
}
pub(in crate::workbench::forge) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/forge/sign-in/start", post(start))
        .route("/forge/sign-in/wait", post(wait))
        .route("/forge/sign-in/cancel", post(cancel))
        .route("/forge/app-access", post(access))
}

fn finish(tx: &watch::Sender<Option<Result<Value, ForgeError>>>, v: Result<Value, ForgeError>) {
    tx.send_if_modified(|current| {
        if current.is_some() {
            false
        } else {
            *current = Some(v);
            true
        }
    });
}
async fn start(State(app): State<Workbench>, Json(input): Json<Input>) -> Reply<Value> {
    let e = endpoints(&app, &input)?;
    let result = form(
        &e,
        &["login", "device", "code"],
        &[("client_id", &e.client_id)],
    )
    .await?;
    let field = |k: &str| {
        result[k]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| failure("failed", "incomplete GitHub device-code response"))
    };
    let device = field("device_code")?.to_owned();
    let user = field("user_code")?;
    let uri = field("verification_uri")?;
    let verification =
        Url::parse(uri).map_err(|_| failure("failed", "invalid GitHub verification URI"))?;
    if verification.origin() != e.oauth.origin()
        || !verification.username().is_empty()
        || verification.password().is_some()
    {
        return Err(failure("failed", "unexpected GitHub verification host"));
    }
    let expires = future(
        now(),
        result["expires_in"]
            .as_u64()
            .ok_or_else(|| failure("failed", "missing GitHub device-code lifetime"))?,
    );
    let interval = result["interval"].as_u64().unwrap_or(5).max(1);
    let id = Uuid::new_v4();
    let (tx, _) = watch::channel(None);
    let mut pending = app.0.signin.pending.lock().unwrap();
    if let Some(old) = pending.take() {
        finish(
            &old.result,
            Err(failure("failed", "a newer sign-in replaced this one")),
        );
        old.worker.abort();
    }
    let app2 = std::sync::Arc::downgrade(&app.0);
    let tx2 = tx.clone();
    let worker = tokio::spawn(async move {
        let value = poll(&app2, id, e, device, expires, interval).await;
        let Some(owner) = app2.upgrade() else {
            return;
        };
        let lock = owner.signin.pending.lock().unwrap();
        if lock.as_ref().is_some_and(|p| p.id == id) {
            finish(&tx2, value);
        }
    });
    *pending = Some(Attempt {
        id,
        result: tx,
        worker,
    });
    Ok(Json(
        json!({"userCode":user,"verificationUri":uri,"expiresAt":expires}),
    ))
}
async fn poll(
    owner: &std::sync::Weak<crate::workbench::Inner>,
    id: Uuid,
    mut e: Endpoints,
    device: String,
    expires: i64,
    mut interval: u64,
) -> Result<Value, ForgeError> {
    loop {
        let remaining = (expires - now()).max(0) as u64;
        tokio::time::sleep(Duration::from_secs(interval).min(Duration::from_millis(remaining)))
            .await;
        if now() >= expires {
            return Err(failure("expired", "the GitHub sign-in code expired"));
        }
        let Ok(v) = form(
            &e,
            &["login", "oauth", "access_token"],
            &[
                ("client_id", &e.client_id),
                ("device_code", &device),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await
        else {
            continue;
        };
        if let Some(session) = Session::from_answer(&e, &v) {
            let app = Workbench(
                owner
                    .upgrade()
                    .ok_or_else(|| failure("cancelled", "workbench stopped"))?,
            );
            let guard = app.0.signin.credential.lock().await;
            {
                let pending = app.0.signin.pending.lock().unwrap();
                if !pending
                    .as_ref()
                    .is_some_and(|p| p.id == id && p.result.borrow().is_none())
                {
                    return Err(failure("cancelled", "sign-in cancelled"));
                }
                sessions::write(&app, &session)?;
                app.0.signin.refused.lock().unwrap().clear();
            }
            drop(guard);
            e.context.token = Some(session.access);
            let viewer = github::request(&e.context, reqwest::Method::GET, &["user"], &[], None)
                .await
                .map_err(|_| {
                    failure("failed", "signed in but could not read the GitHub account")
                })?;
            return Ok(
                json!({"account":viewer["login"].as_str().filter(|s|!s.trim().is_empty()).ok_or_else(||failure("failed","GitHub omitted the signed-in account"))?}),
            );
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = v["interval"]
                    .as_u64()
                    .unwrap_or_else(|| interval.saturating_add(5))
                    .max(1)
            }
            Some("access_denied") => return Err(failure("denied", "sign-in was denied on GitHub")),
            Some("expired_token") => {
                return Err(failure("expired", "the GitHub sign-in code expired"));
            }
            _ => return Err(failure("failed", "GitHub refused the device sign-in")),
        }
    }
}
async fn wait(State(app): State<Workbench>, Json(input): Json<Input>) -> Reply<Value> {
    endpoints(&app, &input)?;
    let mut rx = app
        .0
        .signin
        .pending
        .lock()
        .unwrap()
        .as_ref()
        .map(|a| a.result.subscribe())
        .ok_or_else(|| failure("not-started", "no GitHub sign-in has been started"))?;
    loop {
        if let Some(v) = rx.borrow_and_update().clone() {
            return v.map(Json);
        }
        rx.changed()
            .await
            .map_err(|_| failure("cancelled", "GitHub sign-in was interrupted"))?;
    }
}
async fn cancel(State(app): State<Workbench>, Json(input): Json<Input>) -> Reply<Value> {
    endpoints(&app, &input)?;
    if let Some(a) = app.0.signin.pending.lock().unwrap().as_ref() {
        finish(
            &a.result,
            Err(failure("cancelled", "GitHub sign-in was cancelled")),
        );
        a.worker.abort();
    }
    Ok(Json(Value::Null))
}
async fn access(State(app): State<Workbench>, Json(input): Json<Input>) -> Reply<Value> {
    let Ok(mut e) = endpoints(&app, &input) else {
        return Ok(Json(json!({"status":"signed-out"})));
    };
    let Some(repository) = input.repository.as_deref() else {
        return Err(ForgeError::bad("expected repository"));
    };
    match sessions::token(&app, &e).await {
        Ok(None) => Ok(Json(json!({"status":"signed-out"}))),
        Ok(Some(token)) => {
            e.context.token = Some(token);
            let covered = coverage(&app, &e.context, repository).await.unwrap_or(true);
            Ok(Json(
                json!({"status":if covered{"covered"}else{"not-covered"}}),
            ))
        }
        Err(_) => Ok(Json(json!({"status":"covered"}))),
    }
}
pub(in crate::workbench::forge) async fn attach(
    app: &Workbench,
    c: &mut Context,
) -> Result<(), ForgeError> {
    if c.kind != "github" || c.token.is_some() {
        return Ok(());
    }
    let mut app_token = None;
    if let Some((client_id, oauth)) = configured(c) {
        let env = Endpoints {
            client_id,
            oauth,
            context: c.clone(),
        };
        app_token = sessions::token(app, &env).await?;
    }
    let mut app_context = c.clone();
    app_context.token = app_token.clone();
    let use_personal =
        app_token.is_none() || coverage(app, &app_context, &c.repository).await != Some(true);
    if use_personal && let Some(t) = providers::credentials::resolve(c).await? {
        c.token = Some(t);
    } else if let Some(t) = app_token {
        c.token = Some(t);
        c.signin = Some(std::sync::Arc::downgrade(&app.0));
    }
    Ok(())
}
async fn coverage(app: &Workbench, c: &Context, repository: &str) -> Option<bool> {
    use sha2::{Digest, Sha256};
    let key = format!(
        "{}:{}:{:x}",
        c.api_base,
        repository.to_lowercase(),
        Sha256::digest(c.token.as_deref()?.as_bytes())
    );
    if let Some((until, value)) = app.0.signin.coverage.lock().unwrap().get(&key)
        && *until > std::time::Instant::now()
    {
        return *value;
    }
    let result = sessions::covered(c, repository).await.ok();
    let mut cache = app.0.signin.coverage.lock().unwrap();
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(
        key,
        (
            std::time::Instant::now()
                + Duration::from_secs(if result.is_some() { 120 } else { 30 }),
            result,
        ),
    );
    result
}
pub(in crate::workbench::forge) fn reject(c: &Context) {
    use sha2::{Digest, Sha256};
    if let Some(app) = c.signin.as_ref().and_then(std::sync::Weak::upgrade)
        && let Some(t) = &c.token
    {
        app.signin
            .refused
            .lock()
            .unwrap()
            .insert(Sha256::digest(t.as_bytes()).into());
    }
}
