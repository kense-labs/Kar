use super::*;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use ring::signature::{ED25519, Ed25519KeyPair, UnparsedPublicKey};
use std::sync::{Arc, Mutex};
const PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
fn pem(raw: &[u8]) -> String {
    let mut der = PREFIX.to_vec();
    der.extend(raw);
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----",
        base64::engine::general_purpose::STANDARD.encode(der)
    )
}
fn relay_jwt(
    k: &Ed25519KeyPair,
    issuer: &str,
    id: &str,
    kind: &str,
    jti: Uuid,
    nonce: Uuid,
    thumb: Option<&str>,
) -> String {
    let at = jiff::Timestamp::now().as_second();
    let h = B64.encode(
        serde_json::to_vec(&json!({"alg":"EdDSA","typ":format!("kar-cloud-{kind}+jwt")})).unwrap(),
    );
    let mut p = json!({"iss":issuer,"sub":"kar-fixture-account","aud":format!("kar-env:{id}"),"environmentId":id,"jti":jti,"nonce":nonce,"iat":at,"exp":at+120,"scope":[if kind=="health"{"environment:status"}else{"environment:connect"}]});
    if let Some(k) = thumb {
        p["cnf"] = json!({"jkt":k});
        p["clientProofKeyThumbprint"] = k.into();
    }
    let body = format!("{h}.{}", B64.encode(serde_json::to_vec(&p).unwrap()));
    format!("{body}.{}", B64.encode(k.sign(body.as_bytes()).as_ref()))
}
fn verify(token: &str, pem: &str, typ: &str) -> Value {
    let (data, sig) = token.rsplit_once('.').unwrap();
    let (h, p) = data.split_once('.').unwrap();
    let key = base64::engine::general_purpose::STANDARD
        .decode(
            pem.lines()
                .filter(|s| !s.starts_with('-'))
                .collect::<String>(),
        )
        .unwrap();
    UnparsedPublicKey::new(&ED25519, key.strip_prefix(PREFIX).unwrap())
        .verify(data.as_bytes(), &B64.decode(sig).unwrap())
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&B64.decode(h).unwrap()).unwrap()["typ"],
        typ
    );
    serde_json::from_slice(&B64.decode(p).unwrap()).unwrap()
}
type Log = Arc<Mutex<Vec<(String, Value)>>>;
async fn remote(
    State(log): State<Log>,
    Path((_id, op)): Path<(Uuid, String)>,
    h: axum::http::HeaderMap,
    Json(v): Json<Value>,
) -> Json<Value> {
    assert_eq!(h[header::AUTHORIZATION], "Bearer fixture-relay-environment");
    log.lock().unwrap().push((op, v));
    Json(json!({"ok":true}))
}
#[tokio::test]
async fn signed_relay_to_approved_device() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let native = dir.path().join("native");
    let state = dir.path().join("state");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&native).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = format!("http://{}", l.local_addr().unwrap());
    let log = Log::default();
    let api = Router::new()
        .route("/v1/environments/{id}/{op}", post(remote))
        .with_state(log.clone());
    let handler = tokio::spawn(async move { axum::serve(l, api).await.unwrap() });
    let vars = [
        ("KAR_PUBLIC_ORIGIN", std::ffi::OsStr::new(PUBLIC)),
        ("KAR_CONNECT_LOOPBACK_RELAY", std::ffi::OsStr::new(&relay)),
    ];
    let (mut proc, base) = server::serve_with_env(&state, &work, &native, &vars).await;
    let owner = server::client();
    let public = Client::new();
    let ok = server::json;
    let k = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .unwrap()
            .as_ref(),
    )
    .unwrap();
    let config = json!({"relayUrl":relay,"cloudUserId":"kar-fixture-account","environmentCredential":"fixture-relay-environment","cloudMintPublicKey":pem(k.public_key().as_ref()),"endpointRuntime":null});
    let link = || {
        owner
            .post(format!("{base}/api/connect/relay-config"))
            .json(&config)
    };
    assert_eq!(ok(link()).await["ok"], true);
    let lp=ok(owner.post(format!("{base}/api/connect/link-proof")).json(&json!({"challenge":"local-challenge","relayIssuer":relay,"origin":{"localHttpHost":"untrusted","localHttpPort":99},"endpoint":{"httpBaseUrl":"https://forwarded.example","wsBaseUrl":"wss://forwarded.example","providerKind":"manual"}}))).await;
    let lp = lp.as_str().unwrap();
    let payload: Value =
        serde_json::from_slice(&B64.decode(lp.split('.').nth(1).unwrap()).unwrap()).unwrap();
    let public_key = payload["environmentPublicKey"].as_str().unwrap();
    let lp = verify(lp, public_key, "kar-env-link+jwt");
    let env = lp["environmentId"].as_str().unwrap();
    assert_eq!(lp["origin"]["localHttpHost"], "127.0.0.1");
    assert_ne!(lp["origin"]["localHttpPort"], 99);
    assert_eq!(lp["challenge"], "local-challenge");
    let read_state = ok(owner.get(format!("{base}/api/connect/link-state"))).await;
    assert_eq!(read_state["cloudUserId"], "kar-fixture-account");
    assert!(!read_state.to_string().contains("fixture-relay-environment"));
    let health = |base: &str, t: &str| {
        public
            .post(format!("{base}/api/kar-connect/health"))
            .json(&json!({"proof":t}))
    };
    let nonce = Uuid::new_v4();
    let jwt = relay_jwt(&k, &relay, env, "health", Uuid::new_v4(), nonce, None);
    let online = ok(health(&base, &jwt)).await;
    let response = verify(
        online["proof"].as_str().unwrap(),
        public_key,
        "kar-env-health+jwt",
    );
    assert_eq!(response["requestNonce"], nonce.to_string());
    assert_eq!(status(health(&base, &jwt)).await, 401);
    let different = relay_jwt(&k, &relay, env, "health", Uuid::new_v4(), nonce, None);
    assert_eq!(status(health(&base, &different)).await, 401);
    assert_eq!(
        ok(owner
            .post(format!("{base}/api/connect/preferences"))
            .json(&json!({"publishAgentActivity":true,"holdWebhooksWhileOffline":true})))
        .await["holdWebhooksWhileOffline"],
        true
    );
    assert_eq!(
        ok(owner
            .post(format!("{base}/api/connect/live-activity-preference"))
            .json(&json!({"liveActivitiesEnabled":true})))
        .await["ok"],
        true
    );
    assert_eq!(log.lock().unwrap().len(), 2);
    let device = key();
    let pk = device.public_key().as_ref();
    let jwk = format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
        B64.encode(&pk[1..33]),
        B64.encode(&pk[33..])
    );
    let thumb = B64.encode(Sha256::digest(jwk));
    let mint = |base: &str| {
        public.post(format!("{base}/api/kar-connect/mint-credential")).json(&json!({"proof":relay_jwt(&k,&relay,env,"mint",Uuid::new_v4(),Uuid::new_v4(),Some(&thumb))}))
    };
    let issued = ok(mint(&base)).await;
    assert_eq!(
        verify(
            issued["proof"].as_str().unwrap(),
            public_key,
            "kar-env-mint+jwt"
        )["credential"],
        issued["credential"]
    );
    let credential = issued["credential"].as_str().unwrap();
    let authorize = |base: &str| {
        form(
            public
                .post(format!("{base}/oauth/token"))
                .header("dpop", proof(&device, "POST", "/oauth/token", None)),
            &[
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:token-exchange",
                ),
                ("requested_token_type", ACCESS),
                ("subject_token_type", BOOT),
                ("subject_token", credential),
            ],
        )
    };
    let waiting = authorize(&base).send().await.unwrap();
    assert_eq!(waiting.status(), 403);
    let code = waiting.json::<Value>().await.unwrap()["verificationCode"].clone();
    let devices = ok(owner.get(format!("{base}/api/auth/connect-devices"))).await;
    let pending = &devices["pending"][0];
    assert_eq!(pending["verificationCode"], code);
    assert_eq!(
        ok(owner
            .post(format!("{base}/api/auth/connect-devices/approve"))
            .json(&json!({"pairingLinkId":pending["pairingLinkId"]})))
        .await["approved"],
        true
    );
    let granted = ok(authorize(&base)).await;
    let raw = granted["access_token"].as_str().unwrap();
    let list = |base: &str| {
        public
            .get(format!("{base}/projects"))
            .header("Authorization", format!("DPoP {raw}"))
            .header("dpop", proof(&device, "GET", "/projects", Some(raw)))
    };
    assert_eq!(status(list(&base)).await, 200);
    proc.kill().await.unwrap();
    let (mut proc, base) = server::serve_with_env(&state, &work, &native, &vars).await;
    assert_eq!(status(list(&base)).await, 200);
    assert_eq!(status(health(&base, &jwt)).await, 401);
    assert_eq!(
        ok(owner
            .post(format!("{base}/api/auth/connect-devices/revoke"))
            .json(&json!({"proofKeyThumbprint":thumb})))
        .await["revoked"],
        true
    );
    assert_eq!(status(list(&base)).await, 401);
    assert_eq!(
        ok(owner.post(format!("{base}/api/connect/unlink"))).await["ok"],
        true
    );
    assert_eq!(status(mint(&base)).await, 401);
    assert_eq!(
        ok(owner.get(format!("{base}/api/connect/link-state"))).await["linked"],
        false
    );
    proc.kill().await.unwrap();
    handler.abort();
}
