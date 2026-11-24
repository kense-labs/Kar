use super::super::billing_fixture::*;
use aes_gcm::{
    AesGcm, KeyInit,
    aead::{AeadInPlace, consts::U16},
    aes::Aes256,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
};

pub(super) fn peer(case: &str, args: &[&str]) {
    if args == ["provider", "list", "--json"] {
        let cfg = match case {
            "unconfigured" => json!({}),
            "keyring" => {
                json!({"managed:kimi-code":{"type":"kimi","oauth":{"storage":"system","key":"oauth/kimi-code"}}})
            }
            _ => json!({"managed:kimi-code":{"type":"kimi"}}),
        };
        println!("{}", json!({"providers":cfg}));
        return;
    }
    assert_eq!(
        args,
        ["web", "--no-open", "--host", "127.0.0.1", "--port", "0"]
    );
    assert_eq!(
        std::env::var("KIMI_CODE_MODEL_CATALOG_REFRESH_ON_START").unwrap(),
        "0"
    );
    assert_eq!(
        std::env::var("KIMI_CODE_MODEL_CATALOG_REFRESH_INTERVAL_MS").unwrap(),
        "0"
    );
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    println!(
        "Visit http://{}/#token=local-web-test",
        socket.local_addr().unwrap()
    );
    for stream in socket.incoming() {
        let mut stream = stream.unwrap();
        let mut headers = BufReader::new(&stream).lines().map(|l| l.unwrap());
        let request = headers.next().unwrap();
        let path = request.split_whitespace().nth(1).unwrap();
        assert!(
            headers
                .take_while(|s| !s.is_empty())
                .any(|s| s.eq_ignore_ascii_case("authorization: bearer local-web-test"))
        );
        let method = request.split_whitespace().next().unwrap();
        let data = match path {
            "/api/v1/workspaces" if method == "GET" && case == "existing" => {
                json!({"items":[{"id":"ws"}]})
            }
            "/api/v1/workspaces" if method == "GET" => json!({"items":[]}),
            "/api/v1/workspaces" if method == "POST" => json!({"id":"ws"}),
            "/api/v1/workspaces/ws/skills" if case == "malformed" => {
                json!({"skills":[{"content":KEY}]})
            }
            "/api/v1/workspaces/ws/skills" => {
                json!({"skills":[{"name":"alpha","source":"project","disable_model_invocation":true,"content":KEY}]})
            }
            "/api/v1/workspaces/ws" if method == "DELETE" => {
                fs::write("removed-workspace", "ws").unwrap();
                json!({})
            }
            "/api/v1/oauth/usage" if case == "rejection" => json!({"kind":"error","status":403}),
            "/api/v1/oauth/usage" => {
                json!({"kind":"ok","quota":{"usages":{"limit7d":{"usedRatio":1},"monthCode":{"usedRatio":0.12}},"extraUsage":{"balanceCents":20,"monthlyChargeLimitEnabled":true,"monthlyUsedCents":1,"monthlyChargeLimitCents":10}}})
            }
            "/api/v1/oauth/userinfo" => {
                json!({"kind":"ok","userInfo":{"email":KEY,"nickname":"Fixture","userLevelName":"Plus"}})
            }
            "/api/v1/shutdown" => {
                fs::write("native-stopped", "ok").unwrap();
                json!({})
            }
            _ => panic!("unexpected Kimi account request"),
        };
        let body = json!({"code":0,"data":data}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
        if path == "/api/v1/shutdown" {
            return;
        }
    }
}
fn seal(home: &Path) {
    let d = home.join(".factory");
    fs::create_dir_all(&d).unwrap();
    let key = [9u8; 32];
    let nonce = [3u8; 16];
    let mut bytes =
        json!({"access_token":KEY,"active_organization_id":ORG,"whoami":{"region":"eu"}})
            .to_string()
            .into_bytes();
    let tag = AesGcm::<Aes256, U16>::new_from_slice(&key)
        .unwrap()
        .encrypt_in_place_detached((&nonce).into(), &[], &mut bytes)
        .unwrap();
    fs::write(d.join("auth.v2.key"), STANDARD.encode(key)).unwrap();
    fs::write(
        d.join("auth.v2.file"),
        format!(
            "{}:{}:{}",
            STANDARD.encode(nonce),
            STANDARD.encode(tag),
            STANDARD.encode(bytes)
        ),
    )
    .unwrap();
}
pub(super) async fn exercise() {
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path();
    let exe = std::env::current_exe().unwrap();
    let server = Server::start().await;
    let mut opts = options(h);
    set(&mut opts, "KAR_ACCOUNT_TEST", "factory");
    set(&mut opts, "ACCOUNT_CASE", "quota_billing");
    let us = format!("{}/us", server.origin);
    let eu = format!("{}/eu", server.origin);
    set(&mut opts, "FACTORY_API_BASE_URL", &us);
    set(&mut opts, "FACTORY_API_BASE_URL_EU", &eu);
    assert_eq!(
        query("factory", &exe, &opts).await.unwrap()["reason"],
        "credentials_missing"
    );
    seal(h);
    let factory = h.join(".factory");
    fs::write(factory.join("auth.v2.keyring"), "irrelevant").unwrap();
    assert_eq!(
        query("factory", &exe, &opts).await.unwrap()["reason"],
        "unsupported_auth_storage"
    );
    set(&mut opts, "FACTORY_DISABLE_KEYRING", "1");
    server.answer(200, factory_limits());
    let v = query("factory", &exe, &opts).await.unwrap();
    assert_eq!(v["rateLimited"], true);
    let w = &v["windows"];
    assert_eq!(w.as_array().unwrap().len(), 4);
    assert_eq!(w[0]["usedRatio"], 0.999999);
    assert_eq!(w[1]["resetsAt"], "2098-01-01T00:00:00.000Z");
    assert_eq!(w[2]["usedRatio"], 0.0);
    assert_eq!(w[3]["scope"]["kind"], "models");
    assert_eq!(
        server.seen.lock().unwrap().last().unwrap(),
        &("/eu/api/billing/limits".into(), true, true)
    );
    set(&mut opts, "FACTORY_API_KEY", KEY);
    let mut extra = factory_limits();
    extra["extraUsageAllowed"] = true.into();
    extra["extraUsageBalanceCents"] = 10.into();
    extra["overagePreference"] = "extraUsage".into();
    server.answer(200, extra);
    assert_eq!(
        query("factory", &exe, &opts).await.unwrap()["rateLimited"],
        false
    );
    assert_eq!(
        server.seen.lock().unwrap().last().unwrap(),
        &("/us/api/billing/limits".into(), true, false)
    );
    fs::create_dir_all(factory.join("api-key-identity")).unwrap();
    fs::write(factory.join("api-key-identity/seed.json"),json!({"version":2,"credentialId":format!("{:x}",Sha256::digest(KEY)),"apiBaseUrl":us,"routing":{"region":"eu"}}).to_string()).unwrap();
    for (status, expected) in [
        (401, "credentials_rejected"),
        (403, "credentials_rejected"),
        (404, "endpoint_unavailable"),
    ] {
        server.answer(status, json!({"ignored":KEY}));
        assert_eq!(
            query("factory", &exe, &opts).await.unwrap()["reason"],
            expected
        );
        assert_eq!(
            server.seen.lock().unwrap().last().unwrap(),
            &("/eu/api/billing/limits".into(), true, false)
        );
    }
    set(&mut opts, "FACTORY_API_KEY", "invalid token");
    let n = server.seen.lock().unwrap().len();
    assert!(query("factory", &exe, &opts).await.is_err());
    assert_eq!(server.seen.lock().unwrap().len(), n);
    opts.installation.env.insert("FACTORY_API_KEY".into(), None);
    fs::write(factory.join("auth.v2.file"), KEY).unwrap();
    let err = query("factory", &exe, &opts).await.unwrap_err();
    assert!(!err.to_string().contains(KEY));
    set(&mut opts, "KAR_ACCOUNT_TEST", "kimi");
    set(&mut opts, "ACCOUNT_CASE", "quota_unconfigured");
    assert_eq!(
        query("kimi", &exe, &opts).await.unwrap()["reason"],
        "auth_configuration_unavailable"
    );
    set(&mut opts, "ACCOUNT_CASE", "quota_keyring");
    assert_eq!(
        query("kimi", &exe, &opts).await.unwrap()["reason"],
        "unsupported_auth_storage"
    );
    set(&mut opts, "ACCOUNT_CASE", "quota_billing");
    assert_eq!(
        query("kimi", &exe, &opts).await.unwrap()["reason"],
        "credentials_missing"
    );
    let slot = configure_kimi(h, &server, &mut opts);
    let fresh = fs::read(&slot).unwrap();
    set(&mut opts, "KIMI_DISABLE_OAUTH_LOCK", "1");
    server.answer(200, kimi_limits());
    server
        .identity(json!({"user_id":"42","email":KEY,"nickname":"Sample","user_level_name":"Pro"}));
    let v = query("kimi", &exe, &opts).await.unwrap();
    assert_eq!(v["plan"], "Pro");
    assert_eq!(v["displayName"], "Sample");
    assert!(v.get("email").is_none());
    assert_eq!(v["windows"][1]["label"], "5h limit");
    assert_eq!(v["windows"][1]["usedRatio"], 0.21);
    assert_eq!(v["rateLimited"], true);
    fs::write(
        &slot,
        json!({"access_token":KEY,"refresh_token":REFRESH,"expires_at":1}).to_string(),
    )
    .unwrap();
    let n = server.seen.lock().unwrap().len();
    assert_eq!(
        query("kimi", &exe, &opts).await.unwrap()["reason"],
        "access_token_expired"
    );
    fs::write(&slot, r#"{"access_token":"old","expires_at":1}"#).unwrap();
    assert_eq!(
        query("kimi", &exe, &opts).await.unwrap()["reason"],
        "credentials_missing"
    );
    fs::write(
        &slot,
        r#"{"access_token":"","refresh_token":"","expires_at":0}"#,
    )
    .unwrap();
    assert_eq!(
        query("kimi", &exe, &opts).await.unwrap()["reason"],
        "credentials_rejected"
    );
    assert_eq!(server.seen.lock().unwrap().len(), n);
    fs::write(&slot, fresh).unwrap();
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        set(&mut opts, "KIMI_DISABLE_OAUTH_LOCK", "0");
        let v = query("kimi", &exe, &opts).await.unwrap();
        assert_eq!(v["plan"], "Plus");
        assert_eq!(v["rateLimited"], false);
        assert_eq!(v["windows"][2]["usedRatio"], 0.1);
        assert!(v.get("email").is_none());
        assert!(h.join("native-stopped").is_file());
        set(&mut opts, "ACCOUNT_CASE", "quota_rejection");
        assert_eq!(
            query("kimi", &exe, &opts).await.unwrap()["reason"],
            "credentials_rejected"
        );
    }
}
