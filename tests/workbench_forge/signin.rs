use super::*;
use axum::routing::post;
use std::collections::HashMap;
#[derive(Default)]
struct Hub {
    base: String,
    n: usize,
    polls: usize,
    refresh: usize,
    refuse: bool,
}
type HubRef = Arc<Mutex<Hub>>;
fn body(bytes: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(bytes).into_owned().collect()
}

async fn device(State(h): State<HubRef>, b: axum::body::Bytes) -> Json<Value> {
    assert_eq!(body(&b)["client_id"], "kar-fixture-app");
    let mut h = h.lock().unwrap();
    h.n += 1;
    h.polls = 0;
    Json(
        json!({"user_code":format!("U{}",h.n),"device_code":format!("D{}",h.n),"interval":1,"expires_in":30,"verification_uri":format!("{}/activate",h.base)}),
    )
}
async fn token(State(h): State<HubRef>, b: axum::body::Bytes) -> Json<Value> {
    let b = body(&b);
    let mut h = h.lock().unwrap();
    if b["grant_type"] == "refresh_token" {
        assert_eq!(b["refresh_token"], "fixture-refresh");
        h.refresh += 1;
        h.refuse = false;
        return Json(
            json!({"access_token":"fixture-rotated","expires_in":3600,"refresh_token":"fixture-refreshed"}),
        );
    }
    assert_eq!(
        b["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    h.polls += 1;
    Json(match b["device_code"].as_str() {
        "D2" => json!({"error":"access_denied"}),
        "D3" => json!({"error":"expired_token"}),
        "D4" if h.polls == 1 => json!({"error":"authorization_pending"}),
        "D4" if h.polls == 2 => json!({"error":"slow_down","interval":1}),
        "D4" => {
            json!({"access_token":"fixture-session","refresh_token":"fixture-refresh","expires_in":3600})
        }
        _ => json!({"error":"authorization_pending"}),
    })
}
fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
}
async fn viewer(State(h): State<HubRef>, headers: HeaderMap) -> Response {
    if h.lock().unwrap().refuse {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    assert!(matches!(
        bearer(&headers),
        "Bearer fixture-session" | "Bearer fixture-rotated"
    ));
    Json(json!({"id":123,"login":"connected-viewer"})).into_response()
}
async fn installs(headers: HeaderMap) -> Json<Value> {
    assert!(matches!(
        bearer(&headers),
        "Bearer fixture-session" | "Bearer fixture-rotated"
    ));
    Json(
        json!({"installations":[{"id":1,"account":{"login":"allorg"},"repository_selection":"all"},{"id":2,"account":{"login":"selectorg"},"repository_selection":"selected"}]}),
    )
}
async fn repos() -> Json<Value> {
    Json(json!({"repositories":[{"full_name":"selectorg/listed"}]}))
}
pub(super) async fn run() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let root = temp.path().join("root");
    let native = temp.path().join("native");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&native).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oauth = format!("http://{}", listener.local_addr().unwrap());
    let hub = Arc::new(Mutex::new(Hub {
        base: oauth.clone(),
        ..Default::default()
    }));
    let router = Router::new()
        .route("/api/login/device/code", post(device))
        .route("/api/login/oauth/access_token", post(token))
        .route("/api/user", get(viewer))
        .route("/api/user/installations", get(installs))
        .route("/api/user/installations/2/repositories", get(repos))
        .route("/api/graphql", post(|| async { StatusCode::UNAUTHORIZED }))
        .with_state(hub.clone());
    let peer = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let config = temp.path().join("hosts.toml");
    fs::write(
        &config,
        format!("[[hosts]]\nhost='github.com'\nkind='github'\napi_base='{oauth}/api'\n"),
    )
    .unwrap();
    let env = [
        ("KAR_FORGE_CONFIG", config.as_os_str()),
        ("KAR_GITHUB_APP_CLIENT_ID", OsStr::new("kar-fixture-app")),
    ];
    let (mut proc, url) = server::serve_with_env(&state, &root, &native, &env).await;
    let client = server::client();
    let req = json!({"provider":"github","host":"github.com"});
    let sign = |op: &str| client.post(format!("{url}/forge/sign-in/{op}")).json(&req);
    let access = |repo: &str| {
        client
            .post(format!("{url}/forge/app-access"))
            .json(&json!({"host":"github.com","provider":"github","repository":repo}))
    };
    assert_eq!(ok(access("allorg/abc")).await["status"], "signed-out");
    assert_eq!(
        sign("wait")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["kind"],
        "not-started"
    );
    let prompt = ok(sign("start")).await;
    assert_eq!(prompt["userCode"], "U1");
    assert_eq!(prompt["verificationUri"], format!("{oauth}/activate"));
    assert!(!prompt.to_string().contains("D1"));
    ok(sign("cancel")).await;
    assert_eq!(
        sign("wait")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["kind"],
        "cancelled"
    );
    for reason in ["denied", "expired"] {
        ok(sign("start")).await;
        assert_eq!(
            sign("wait")
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["kind"],
            reason
        );
    }
    ok(sign("start")).await;
    let account = ok(sign("wait")).await;
    assert_eq!(account, json!({"account":"connected-viewer"}));
    assert_eq!(ok(access("allorg/arbitrary")).await["status"], "covered");
    assert_eq!(
        ok(access("https://github.com/selectorg/listed.git")).await["status"],
        "covered"
    );
    assert_eq!(
        ok(access("selectorg/missing")).await["status"],
        "not-covered"
    );
    git(&root, &["init", "-b", "main"]);
    git(
        &root,
        &["remote", "add", "origin", "https://github.com/allorg/repo"],
    );
    let project = ok(client
        .post(format!("{url}/projects"))
        .json(&json!({"name":"app","root":root})))
    .await;
    let p = project["id"].as_str().unwrap();
    proc.kill().await.unwrap();
    let (mut proc, url) = server::serve_with_env(&state, &root, &native, &env).await;
    let routing = format!("{url}/projects/{p}/forge/routing");
    assert_eq!(ok(client.get(&routing)).await["viewer"], "connected-viewer");
    hub.lock().unwrap().refuse = true;
    assert!(
        !client
            .get(&routing)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(ok(client.get(&routing)).await["viewer"], "connected-viewer");
    assert_eq!(hub.lock().unwrap().refresh, 1);
    proc.kill().await.unwrap();
    peer.abort();
}
