//! Local HTTP authentication with real PKCE and P-256 proofs. No hosted account is used.
#[path = "support/workbench_server.rs"]
#[allow(dead_code)]
mod server;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use reqwest::{Client, RequestBuilder, StatusCode, header};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
const PUBLIC: &str = "https://kar.test";
const BOOT: &str = "urn:kar:params:oauth:token-type:environment-bootstrap";
const ACCESS: &str = "urn:ietf:params:oauth:token-type:access_token";
async fn status(r: RequestBuilder) -> StatusCode {
    r.send().await.unwrap().status()
}
fn form(r: RequestBuilder, entries: &[(&str, &str)]) -> RequestBuilder {
    r.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(entries.iter().copied())
                .finish(),
        )
}
fn key() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let b = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, b.as_ref(), &rng).unwrap()
}
fn proof(key: &EcdsaKeyPair, method: &str, path: &str, token: Option<&str>) -> String {
    let public = key.public_key().as_ref();
    let head = json!({"typ":"dpop+jwt","alg":"ES256","jwk":{"kty":"EC","crv":"P-256","x":B64.encode(&public[1..33]),"y":B64.encode(&public[33..65])}});
    let mut claims = json!({"htm":method,"htu":format!("{PUBLIC}{path}"),"jti":Uuid::new_v4(),"iat":jiff::Timestamp::now().as_second()});
    if let Some(t) = token {
        claims["ath"] = B64.encode(Sha256::digest(t)).into();
    }
    let input = format!(
        "{}.{}",
        B64.encode(serde_json::to_vec(&head).unwrap()),
        B64.encode(serde_json::to_vec(&claims).unwrap())
    );
    let signature = key.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
    format!("{input}.{}", B64.encode(signature.as_ref()))
}
fn callback(link: &Value, k: &str) -> String {
    url::Url::parse(link["redirectTo"].as_str().unwrap())
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == k)
        .unwrap()
        .1
        .into_owned()
}
async fn mcp(remote: &Client, base: &str, token: &str) -> StatusCode {
    status(remote.post(format!("{base}/mcp")).bearer_auth(token).json(&json!({"id":1,"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}))).await
}
#[tokio::test]
async fn public_mcp_and_environment_sessions() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("projects");
    let state = home.path().join("state");
    let native = home.path().join("native");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&native).unwrap();
    let vars = [("KAR_PUBLIC_ORIGIN", std::ffi::OsStr::new(PUBLIC))];
    let (mut child, base) = server::serve_with_env(&state, &root, &native, &vars).await;
    let owner = server::client();
    let client = Client::new();
    let ok = server::json;
    let discovery = client.get(format!("{base}/mcp")).send().await.unwrap();
    assert_eq!(discovery.status(), 401);
    assert!(
        discovery.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains(PUBLIC)
    );
    assert_eq!(
        ok(client.get(format!("{base}/.well-known/oauth-protected-resource/mcp"))).await["resource"],
        format!("{PUBLIC}/mcp")
    );
    assert_eq!(
        ok(client.get(format!("{base}/.well-known/oauth-authorization-server"))).await["issuer"],
        PUBLIC
    );
    let register = || client.post(format!("{base}/oauth/mcp/register"));
    assert_eq!(
        status(register().json(&json!({"redirect_uris":["http://outside.test/cb"]}))).await,
        400
    );
    let reg = ok(register()
        .json(&json!({"client_name":"Reader","redirect_uris":["http://127.0.0.1:1111/cb"]})))
    .await;
    let callback_url = "http://127.0.0.1:8901/cb";
    let verifier = "a".repeat(52);
    let auth = json!({"client_id":reg["client_id"],"redirect_uri":callback_url,"response_type":"code","code_challenge_method":"S256","code_challenge":B64.encode(Sha256::digest(&verifier)),"state":"keep-state","scope":"orchestration:read","resource":format!("{PUBLIC}/mcp")});
    let request = || client.post(format!("{base}/oauth/mcp/decision"));
    assert_eq!(
        callback(
            &ok(request().json(&json!({"authorization":auth,"decision":{"_tag":"deny"}}))).await,
            "error"
        ),
        "access_denied"
    );
    let grant = ok(owner
        .post(format!("{base}/api/auth/pairing-token"))
        .json(&json!({"scopes":["orchestration:read","access:write"]})))
    .await;
    let res = client
        .post(format!("{base}/api/auth/browser-session"))
        .json(&json!({"credential":grant["credential"]}))
        .send()
        .await
        .unwrap();
    let cookie = res.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
    let cookie = cookie.split(';').next().unwrap();
    let browser = || {
        client
            .post(format!("{base}/oauth/mcp/approval"))
            .header(header::COOKIE, cookie)
    };
    let approval = ok(browser().json(&auth)).await;
    assert_eq!(approval["oneClickAccess"], json!(["read-only"]));
    let decide = |q: &Value, c: &Value| {
        request().header(header::COOKIE,cookie).json(&json!({"authorization":q,"decision":{"_tag":"browser-session","csrfToken":c,"access":"read-only"}}))
    };
    assert_eq!(status(decide(&auth, &json!("wrong"))).await, 400);
    assert_eq!(
        status(decide(&auth, &approval["csrfToken"]).header(header::ORIGIN, "https://other.test"))
            .await,
        400
    );
    let issue = || decide(&auth, &approval["csrfToken"]);
    let exchange = |code: &str, v: &str| {
        form(
            client.post(format!("{base}/oauth/mcp/token")),
            &[
                ("grant_type", "authorization_code"),
                ("client_id", reg["client_id"].as_str().unwrap()),
                ("redirect_uri", callback_url),
                ("code", code),
                ("code_verifier", v),
            ],
        )
    };
    let failed = callback(&ok(issue()).await, "code");
    assert_eq!(status(exchange(&failed, &"b".repeat(52))).await, 400);
    assert_eq!(status(exchange(&failed, &verifier)).await, 400);
    let yes = ok(issue()).await;
    assert_eq!(callback(&yes, "state"), "keep-state");
    let code = callback(&yes, "code");
    let result = ok(exchange(&code, &verifier)).await;
    assert_eq!(result["scope"], "orchestration:read");
    let token = result["access_token"].as_str().unwrap();
    assert_eq!(mcp(&client, &base, token).await, 200);
    assert_eq!(
        status(client.get(format!("{base}/projects")).bearer_auth(token)).await,
        401
    );
    assert_eq!(
        status(
            client
                .post(format!("{base}/projects"))
                .header(header::COOKIE, cookie)
                .json(&json!({"root":root,"name":"no"}))
        )
        .await,
        403
    );
    let key = key();
    let g = ok(owner
        .post(format!("{base}/api/auth/pairing-token"))
        .json(&json!({"scopes":["orchestration:read"]})))
    .await;
    let device = |kind: &str, sub: &str| {
        form(
            client
                .post(format!("{base}/oauth/token"))
                .header("DPoP", proof(&key, "POST", "/oauth/token", None)),
            &[
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:token-exchange",
                ),
                ("requested_token_type", ACCESS),
                ("subject_token_type", kind),
                ("subject_token", sub),
            ],
        )
    };
    let result = ok(device(BOOT, g["credential"].as_str().unwrap())).await;
    assert_eq!(result["token_type"], "DPoP");
    let old = result["access_token"].as_str().unwrap();
    assert_eq!(mcp(&client, &base, old).await, 401);
    let renewed = ok(device(ACCESS, old)).await;
    let access = renewed["access_token"].as_str().unwrap();
    let req = |path: &str, token: &str, proof: &str| {
        client
            .get(format!("{base}{path}"))
            .header(header::AUTHORIZATION, format!("DPoP {token}"))
            .header("DPoP", proof)
    };
    assert_eq!(
        status(req(
            "/projects",
            old,
            &proof(&key, "GET", "/projects", Some(old))
        ))
        .await,
        401
    );
    let jwt = proof(&key, "GET", "/projects", Some(access));
    assert_eq!(status(req("/projects", access, &jwt)).await, 200);
    assert_eq!(status(req("/projects", access, &jwt)).await, 401);
    let path = "/api/auth/websocket-ticket/rpc";
    let ticket = ok(client
        .post(format!("{base}{path}"))
        .header(header::AUTHORIZATION, format!("DPoP {access}"))
        .header("DPoP", proof(&key, "POST", path, Some(access))))
    .await;
    let ticket = ticket["ticket"].as_str().unwrap();
    assert_eq!(
        status(
            client
                .get(format!("{base}/devices/screenshot"))
                .query(&[("wsTicket", ticket)])
        )
        .await,
        401
    );
    let path = format!(
        "/projects/{}/chats/{}/events",
        Uuid::new_v4(),
        Uuid::new_v4()
    );
    let read = |p: &str| {
        client
            .get(format!("{base}{path}"))
            .query(&[("wsTicket", ticket), ("dpop", p)])
    };
    let jwt = proof(&key, "GET", &path, Some(ticket));
    let first = status(read(&jwt)).await;
    assert_ne!(first, StatusCode::UNAUTHORIZED);
    assert_ne!(first, StatusCode::FORBIDDEN);
    assert_eq!(status(read(&jwt)).await, 401);
    child.kill().await.unwrap();
    let (mut child, base) = server::serve_with_env(&state, &root, &native, &vars).await;
    assert_eq!(mcp(&client, &base, token).await, 200);
    let revoke = || {
        form(
            client.post(format!("{base}/oauth/mcp/revoke")),
            &[("token", token)],
        )
    };
    assert_eq!(status(revoke()).await, 200);
    assert_eq!(status(revoke()).await, 200);
    assert_eq!(mcp(&client, &base, token).await, 401);
    let state = ok(client
        .get(format!("{base}/api/auth/session"))
        .header(header::COOKIE, cookie))
    .await;
    assert_eq!(state["authenticated"], true);
    let revoked = ok(owner
        .post(format!("{base}/api/auth/clients/revoke"))
        .json(&json!({"sessionId":state["sessionId"]})))
    .await;
    assert_eq!(revoked["revoked"], true);
    assert_eq!(
        ok(client
            .get(format!("{base}/api/auth/session"))
            .header(header::COOKIE, cookie))
        .await["authenticated"],
        false
    );
    child.kill().await.unwrap();
}
