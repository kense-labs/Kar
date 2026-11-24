//! A loopback billing service and disposable provider profiles for vendor and protocol tests.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use kar::{InstallationOptions, account::AccountUsageOptions};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub const KEY: &str = "synthetic-billing-test-token";
pub const REFRESH: &str = "synthetic-test-refresh-token";
pub const ORG: &str = "synthetic-org";

#[derive(Clone)]
pub struct Server {
    pub origin: String,
    pub seen: Arc<Mutex<Vec<(String, bool, bool)>>>,
    response: Arc<Mutex<(StatusCode, Value)>>,
    profile: Arc<Mutex<Value>>,
    pub refreshes: Arc<std::sync::atomic::AtomicUsize>,
}
impl Server {
    pub async fn start() -> Self {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Self {
            origin: format!("http://{}", socket.local_addr().unwrap()),
            seen: Default::default(),
            response: Arc::new(Mutex::new((StatusCode::OK, Value::Null))),
            profile: Default::default(),
            refreshes: Default::default(),
        };
        let router = Router::new()
            .route("/api/oauth/token", post(refresh))
            .fallback(get(serve))
            .with_state(server.clone());
        tokio::spawn(async move {
            axum::serve(socket, router).await.unwrap();
        });
        server
    }
    pub fn answer(&self, status: u16, body: Value) {
        *self.response.lock().unwrap() = (StatusCode::from_u16(status).unwrap(), body);
    }
    pub fn identity(&self, value: Value) {
        *self.profile.lock().unwrap() = value;
    }
}
async fn refresh(State(server): State<Server>, body: String) -> Response {
    let form: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(body.as_bytes()).collect();
    if form.get("grant_type").is_none_or(|v| v != "refresh_token")
        || form.get("refresh_token").is_none_or(|v| v != REFRESH)
        || server
            .refreshes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            != 0
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"invalid_grant"})),
        )
            .into_response();
    }
    Json(json!({"access_token":KEY,"refresh_token":"rotated-local-refresh","expires_in":86400,"token_type":"Bearer","scope":"kimi-code"})).into_response()
}
async fn serve(State(s): State<Server>, url: Uri, headers: HeaderMap) -> Response {
    let valid = headers
        .get("authorization")
        .is_some_and(|v| v.as_bytes() == format!("Bearer {KEY}").as_bytes());
    let org = headers
        .get("x-factory-org-id")
        .is_some_and(|v| v.as_bytes() == ORG.as_bytes());
    s.seen
        .lock()
        .unwrap()
        .push((url.path().to_owned(), valid, org));
    if !valid {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if url.path().ends_with("/me") {
        return Json(s.profile.lock().unwrap().clone()).into_response();
    }
    let (status, body) = s.response.lock().unwrap().clone();
    (status, Json(body)).into_response()
}
pub fn options(home: &Path) -> AccountUsageOptions {
    let mut install = InstallationOptions {
        cwd: Some(home.into()),
        ..Default::default()
    };
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if !matches!(
            name.as_str(),
            "PATH"
                | "SYSTEMROOT"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "NO_PROXY"
                | "ALL_PROXY"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "NODE_EXTRA_CA_CERTS"
                | "NODE_TLS_REJECT_UNAUTHORIZED"
        ) {
            install.env.insert(key, None);
        }
    }
    for k in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "TMPDIR",
        "TEMP",
        "TMP",
        "FACTORY_HOME_OVERRIDE",
        "KIMI_CODE_HOME",
        "KIMI_SHARE_DIR",
    ] {
        install.env.insert(k.into(), Some(home.as_os_str().into()));
    }
    AccountUsageOptions {
        installation: install,
        timeout: Some(Duration::from_secs(25)),
        ..Default::default()
    }
}
pub fn set(opts: &mut AccountUsageOptions, name: &str, value: &str) {
    opts.installation
        .env
        .insert(name.into(), Some(value.into()));
}
pub fn configure_kimi(home: &Path, s: &Server, opts: &mut AccountUsageOptions) -> PathBuf {
    let base = format!("{}/coding/v1", s.origin);
    let auth = &s.origin;
    let digest = format!(
        "{:x}",
        Sha256::digest(format!(
            "{{\"oauthHost\":\"{auth}\",\"baseUrl\":\"{base}\"}}"
        ))
    );
    let slot = format!("kimi-code-env-{}", &digest[..16]);
    fs::write(home.join("config.toml"),format!("[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\nbase_url = \"{base}\"\napi_key = \"\"\n[providers.\"managed:kimi-code\".oauth]\nstorage = \"file\"\nkey = \"oauth/{slot}\"\noauthHost = \"{auth}\"\n")).unwrap();
    set(opts, "KIMI_CODE_BASE_URL", &base);
    set(opts, "KIMI_CODE_OAUTH_HOST", auth);
    set(opts, "KIMI_CODE_REGION_MARKER", "off");
    fs::create_dir_all(home.join("credentials")).unwrap();
    let path = home.join("credentials").join(format!("{slot}.json"));
    fs::write(&path,json!({"access_token":KEY,"refresh_token":REFRESH,"expires_at":4102444800u64,"expires_in":86400,"token_type":"Bearer","scope":"kimi-code"}).to_string()).unwrap();
    path
}

pub async fn query(id: &str, bin: &Path, opts: &AccountUsageOptions) -> kar::Result<Value> {
    let mut rt = kar::runtime(id)?;
    rt.program = bin.into();
    let mut scoped = opts.clone();
    scoped.installation.env.insert(
        format!("OAR_{}_BIN", id.to_uppercase()).into(),
        Some(bin.as_os_str().into()),
    );
    let value = serde_json::to_value(rt.account_usage(&scoped).await?).unwrap();
    assert!(!value.to_string().contains(KEY));
    assert!(!value.to_string().contains(REFRESH));
    Ok(value)
}

pub fn factory_limits() -> Value {
    let future = "2098-01-01T00:00:00Z";
    json!({"hasActiveSubscription":true,"limits":{"standard":{"fiveHour":{"usedPercent":99.99999,"windowEnd":future},"weekly":{"usedPercent":100,"windowEnd":future},"monthly":{"usedPercent":100,"windowEnd":"2000-01-01T00:00:00Z"}},"core":{"fiveHour":{"usedPercent":5,"windowEnd":future},"weekly":{"unlimited":true}}}})
}
pub fn kimi_limits() -> Value {
    json!({"usage":{"used":"100","limit":"100"},
          "limits":[{"window":{"duration":"300","timeUnit":"TIME_UNIT_MINUTE"},"detail":{"used":"21","limit":"100"}}],
          "usages":{"limit_7d":{"used_ratio":1},"limit_5h":{"used_ratio":0.21,"reset_time":"2030-01-01T00:00:00Z"}}})
}
