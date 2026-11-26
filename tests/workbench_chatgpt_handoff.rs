//! Entirely synthetic issuer, two separate Kar workbenches, no external token or model request.
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod server;
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use reqwest::{Client, StatusCode};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
struct Mock {
    base: String,
    key: EcdsaKeyPair,
    grant: Mutex<Option<Grant>>,
    tweak: Mutex<String>,
    revision: Mutex<String>,
}
struct Grant {
    nonce: String,
    challenge: String,
    redirect: String,
    client: String,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
impl Mock {
    async fn serve() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rng = SystemRandom::new();
        let pk = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let s = Arc::new(Self {
            base: format!("http://{}", listener.local_addr().unwrap()),
            key: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pk.as_ref(), &rng)
                .unwrap(),
            grant: Mutex::new(None),
            tweak: Mutex::new(String::new()),
            revision: Mutex::new(String::new()),
        });
        let app=Router::new().route("/.well-known/openid-configuration",get(|State(s):State<Arc<Self>>|async move{
            Json(json!({"issuer":s.base,"authorization_endpoint":format!("{}/authorize",s.base),"token_endpoint":format!("{}/token",s.base),"jwks_uri":format!("{}/keys",s.base),"revocation_endpoint":format!("{}/revoke",s.base)}))
        })).route("/keys",get(|State(s):State<Arc<Self>>|async move{
            let p=s.key.public_key().as_ref();
            Json(json!({"keys":[{"kid":"mock","use":"sig","kty":"EC","crv":"P-256","x":B64.encode(&p[1..33]),"y":B64.encode(&p[33..])}]}))
        })).route("/token",post(tokens)).route("/revoke",post(||async{StatusCode::OK})).with_state(s.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        s
    }
}
async fn tokens(State(s): State<Arc<Mock>>, body: Bytes) -> Json<Value> {
    let form: BTreeMap<_, _> = url::form_urlencoded::parse(&body).into_owned().collect();
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    if form["grant_type"] == "refresh_token" {
        assert_eq!(form["refresh_token"], "mock-private-refresh");
        return Json(
            json!({"token_type":"bearer","access_token":"mock-renewed-private-access","refresh_token":"mock-rotated-refresh","scope":"chatgpt.tokens.use.direct","expires_in":3600}),
        );
    }
    let g = s.grant.lock().unwrap().take().unwrap();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "mock-single-code");
    assert_eq!(form["client_id"], g.client);
    assert_eq!(form["redirect_uri"], g.redirect);
    assert_eq!(
        B64.encode(Sha256::digest(form["code_verifier"].as_bytes())),
        g.challenge
    );
    let tweak = s.tweak.lock().unwrap().clone();
    let token = "mock-private-access";
    let mut c = json!({"iss":s.base,"aud":g.client,"sub":"mock-subject","email":"kar@example.test","nonce":g.nonce,"exp":now()+1200,"iat":now(),"at_hash":B64.encode(&Sha256::digest(token)[..16])});
    match tweak.as_str() {
        "wrong_subject" => c["sub"] = "unrelated-subject".into(),
        "wrong_audience" => c["aud"] = "oaiapp_other".into(),
        "expired" => c["exp"] = 0.into(),
        "wrong_nonce" => c["nonce"] = "unrelated".into(),
        _ => {}
    }
    let msg = format!(
        "{}.{}",
        B64.encode(br#"{"alg":"ES256","kid":"mock"}"#),
        B64.encode(serde_json::to_vec(&c).unwrap())
    );
    let sig = s.key.sign(&SystemRandom::new(), msg.as_bytes()).unwrap();
    let mut sig = sig.as_ref().to_vec();
    if tweak == "forged" {
        sig[0] ^= 1;
    }
    let id_token = format!("{msg}.{}", B64.encode(sig));
    let mut hash = Sha256::new();
    for s in [
        g.client.as_str(),
        c["sub"].as_str().unwrap(),
        id_token.as_str(),
    ] {
        hash.update(s.as_bytes());
        hash.update([0]);
    }
    *s.revision.lock().unwrap() = format!("{hash:x}", hash = hash.finalize());
    Json(
        json!({"access_token":token,"refresh_token":"mock-private-refresh","id_token":id_token,"scope":if tweak=="no_sharing"{"openid"}else{"chatgpt.tokens.use.direct"},"token_type":"bearer","expires_in":30}),
    )
}
fn client(secret: &str) -> Client {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        "authorization",
        "Bearer kar-workbench-fixture".parse().unwrap(),
    );
    h.insert("kar-auth-client", secret.parse().unwrap());
    Client::builder()
        .default_headers(h)
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}
async fn perform(mock: &Mock, c: &Client, wrong: &Client, helper: &str, target: &Value) -> Value {
    let r = server::json(
        c.post(format!("{helper}/handoffs"))
            .json(&json!({"target":target})),
    )
    .await;
    let flow = format!("{helper}/handoffs/{}", r["flow"].as_str().unwrap());
    assert_eq!(
        wrong.get(&flow).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let auth = url::Url::parse(r["state"]["authorization_url"].as_str().unwrap()).unwrap();
    let q: BTreeMap<_, _> = auth.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(
        q["ext_agent_host_id"],
        format!("urn:uuid:{}", target["environment"].as_str().unwrap())
    );
    assert_eq!(
        q["client_id"],
        target["client_id"]
            .as_str()
            .unwrap_or("dynamic_agent_client")
    );
    assert!(!q.contains_key("id_token_hint"));
    let id = target["client_id"].as_str().unwrap_or("oaiapp_kar_fixture");
    *mock.grant.lock().unwrap() = Some(Grant {
        nonce: q["nonce"].clone(),
        challenge: q["code_challenge"].clone(),
        redirect: q["redirect_uri"].clone(),
        client: id.into(),
    });
    let redirect = url::Url::parse(&q["redirect_uri"]).unwrap();
    let mut u = redirect.clone();
    u.query_pairs_mut()
        .append_pair("state", "wrong")
        .append_pair("code", "mock-single-code")
        .append_pair("client_id", id);
    assert!(
        !c.post(format!("{flow}/callback"))
            .json(&json!({"callback_url":u.as_str()}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let mut u = redirect;
    u.query_pairs_mut()
        .append_pair("state", &q["state"])
        .append_pair("code", "mock-single-code")
        .append_pair("client_id", id);
    // Browser callback is deliberately uncredentialed. Owner headers stay on the Kar API.
    Client::new()
        .get(u)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let r = c
        .get(flow)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(r.headers()["cache-control"], "no-store");
    let raw = r.text().await.unwrap();
    for s in [
        "mock-private-access",
        "mock-private-refresh",
        "mock-single-code",
    ] {
        assert!(!raw.contains(s));
    }
    raw.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|v| serde_json::from_str::<Value>(v).unwrap())
        .next_back()
        .unwrap()
}
async fn open(state: &Path, root: &Path, mock: &Mock) -> (tokio::process::Child, String) {
    let home = state.parent().unwrap();
    let params = [
        ("HOME", home.as_os_str()),
        ("CODEX_HOME", home.as_os_str()),
        (
            "KAR_CHATGPT_LOOPBACK_ISSUER",
            std::ffi::OsStr::new(&mock.base),
        ),
    ];
    let (host, base) = server::serve_with_env(state, root, home, &params).await;
    let p = server::json(
        server::client()
            .post(format!("{base}/projects"))
            .json(&json!({"root":root,"name":"handoff"})),
    )
    .await;
    (
        host,
        format!(
            "{base}/projects/{}/accounts/codex/chatgpt",
            p["id"].as_str().unwrap()
        ),
    )
}
#[tokio::test]
async fn transfer_verify_reconnect_and_renew_across_workbenches() {
    let mock = Mock::serve().await;
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let root_a = a.path().join("project");
    let root_b = b.path().join("project");
    std::fs::create_dir(&root_a).unwrap();
    std::fs::create_dir(&root_b).unwrap();
    let state = b.path().join("state");
    let (mut source, helper) = open(&a.path().join("state"), &root_a, &mock).await;
    let (mut dest, api) = open(&state, &root_b, &mock).await;
    let c = client(&Uuid::new_v4().simple().to_string());
    let other = client(&Uuid::new_v4().simple().to_string());
    let r = server::json(c.post(format!("{api}/prepare")).json(&json!({}))).await;
    let target = &r["target"];
    for failure in [
        "wrong_nonce",
        "wrong_audience",
        "expired",
        "forged",
        "no_sharing",
    ] {
        *mock.tweak.lock().unwrap() = failure.into();
        let f = perform(&mock, &c, &other, &helper, target).await;
        assert_eq!(f["phase"], "failed");
    }
    mock.tweak.lock().unwrap().clear();
    let final_state = perform(&mock, &c, &other, &helper, target).await;
    assert_eq!(final_state["phase"], "finished");
    let revision = mock.revision.lock().unwrap().clone();
    let input = json!({"offer":target["offer"],"profile":final_state["profile"]});
    assert_eq!(
        other
            .post(format!("{api}/import"))
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut tampered = input.clone();
    tampered["profile"]["ciphertext"] = "AAAA".into();
    assert!(
        !c.post(format!("{api}/import"))
            .json(&tampered)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let received = server::json(c.post(format!("{api}/import")).json(&input)).await;
    assert_eq!(received["profile"]["client_id"], "oaiapp_kar_fixture");
    assert_eq!(
        server::json(c.post(format!("{api}/import")).json(&input)).await,
        received
    );
    assert!(
        server::json(c.get(format!("{helper}/profiles"))).await["profiles"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    dest.kill().await.unwrap();
    dest.wait().await.unwrap();
    let (mut dest, api) = open(&state, &root_b, &mock).await;
    let profiles = server::json(c.get(format!("{api}/profiles"))).await;
    assert_eq!(profiles["active"], "oaiapp_kar_fixture");
    let r = server::json(
        c.post(format!("{api}/prepare"))
            .json(&json!({"client_id":profiles["active"]})),
    )
    .await;
    *mock.tweak.lock().unwrap() = "wrong_subject".into();
    let f = perform(&mock, &c, &other, &helper, &r["target"]).await;
    assert_eq!(f["phase"], "failed");
    assert_eq!(
        server::json(c.get(format!("{api}/profiles"))).await,
        profiles
    );
    // Active profile refresh goes through Kar's real Codex auth.command executable entry.
    let profiles_dir = state
        .join("workbench/chatgpt")
        .join(format!("{:x}", Sha256::digest("codex")));
    let token = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .arg("managed-chatgpt-token")
        .arg(&profiles_dir)
        .arg(&revision)
        .env("KAR_CHATGPT_LOOPBACK_ISSUER", &mock.base)
        .output()
        .await
        .unwrap();
    assert!(token.status.success());
    assert_eq!(token.stdout, b"mock-renewed-private-access");
    let wrong = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .arg("managed-chatgpt-token")
        .arg(&profiles_dir)
        .arg("0".repeat(64))
        .output()
        .await
        .unwrap();
    assert!(!wrong.status.success() && wrong.stdout.is_empty());
    let raw = std::fs::read_to_string(profiles_dir.join("profiles.sealed")).unwrap();
    for s in [
        "mock-private-access",
        "mock-private-refresh",
        "kar@example.test",
    ] {
        assert!(!raw.contains(s));
    }
    let status = server::json(c.get(api.strip_suffix("/chatgpt").unwrap())).await;
    assert_eq!(status["source"], "managed_chatgpt");
    source.kill().await.unwrap();
    source.wait().await.unwrap();
    dest.kill().await.unwrap();
    dest.wait().await.unwrap();
}
