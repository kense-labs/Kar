//! Real device-code clients against isolated profiles and loopback issuers.
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use kar::{
    InstallationOptions,
    account::{
        AuthStatus, AuthStatusOptions, KimiRegion, LoginEvent, LoginOptions, LoginResult,
        LogoutResult,
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "support/cursor_login.rs"]
mod cursor;

const EMAIL: &str = "device-fixture@example.org";
const ACCOUNT: &str = "9e446c6e-da77-40ad-93f7-5172a535fb82";
const CODE: &str = "PAIR-LOCAL";
#[derive(Default)]
struct Issuer {
    shown: AtomicBool,
    exchanges: AtomicUsize,
}

fn private_env(home: &std::path::Path) -> BTreeMap<OsString, Option<OsString>> {
    let mut env: BTreeMap<_, _> = std::env::vars_os().map(|(k, _)| (k, None)).collect();
    for k in [
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TEMP",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        env.insert(k.into(), Some(home.as_os_str().into()));
    }
    for k in ["SystemRoot", "PATH"] {
        env.insert(k.into(), std::env::var_os(k));
    }
    for k in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        env.insert(k.into(), Some("http://127.0.0.1:1".into()));
    }
    env.insert("NO_PROXY".into(), Some("localhost,127.0.0.1".into()));
    env
}
async fn user_code() -> Json<Value> {
    Json(json!({"device_auth_id":"local-flow","user_code":CODE,"interval":"1"}))
}
async fn poll(
    State(state): State<Arc<Issuer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(body["device_auth_id"], "local-flow");
    if body["user_code"] != CODE || !state.shown.load(Ordering::SeqCst) {
        return (StatusCode::FORBIDDEN, Json(json!({})));
    }
    (
        StatusCode::OK,
        Json(
            json!({"authorization_code":"local-grant","code_verifier":"local-verifier","code_challenge":"local-challenge"}),
        ),
    )
}
async fn exchange(State(state): State<Arc<Issuer>>, body: String) -> Json<Value> {
    assert!(body.contains("code=local-grant"));
    state.exchanges.fetch_add(1, Ordering::SeqCst);
    let payload = json!({"sub":"local-user","email":EMAIL,"exp":2240611200_i64,
        "https://api.openai.com/auth":{"chatgpt_account_id":ACCOUNT,"chatgpt_plan_type":"plus","chatgpt_user_id":"local-user"},
        "https://api.openai.com/profile":{"email":EMAIL}});
    let jwt = format!(
        "{}.{}.c2ln",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(payload.to_string())
    );
    Json(
        json!({"access_token":jwt,"id_token":jwt,"refresh_token":"private-loopback-refresh","token_type":"bearer","expires_in":3600}),
    )
}

#[tokio::test]
#[ignore = "requires OAR_CODEX_BIN with a configurable device-login issuer"]
async fn native_device_code_and_cancellation() {
    let binary = std::env::var_os("OAR_CODEX_BIN").expect("set OAR_CODEX_BIN");
    let state = Arc::new(Issuer::default());
    let app = Router::new()
        .route("/api/accounts/deviceauth/usercode", post(user_code))
        .route("/api/accounts/deviceauth/token", post(poll))
        .route("/oauth/token", post(exchange))
        .route("/backend-api/wham/accounts/check",get(|| async { Json(json!({"accounts":[{"id":ACCOUNT,"name":"Test workspace","workspace_backend_origin":"https://chatgpt.com","account_routing_override":"NO_CONSTRAINT"}],"default_account_id":ACCOUNT})) }))
        .route("/backend-api/wham/config/bundle",get(||async{Json(json!({}))}))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut rt = kar::runtime("codex").unwrap();
    rt.program = binary.into();
    for cancel in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let profile = home.path().join("codex");
        fs::create_dir(&profile).unwrap();
        fs::write(profile.join("config.toml"),format!("cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{base}/backend-api\"\n[analytics]\nenabled = false\n")).unwrap();
        let mut env = private_env(home.path());
        env.insert("OAR_CODEX_BIN".into(), Some(rt.program.as_os_str().into()));
        env.insert("CODEX_HOME".into(), Some(profile.clone().into_os_string()));
        env.insert(
            "CODEX_APP_SERVER_LOGIN_ISSUER".into(),
            Some(base.clone().into()),
        );
        let options = LoginOptions {
            installation: InstallationOptions {
                cwd: Some(home.path().into()),
                env,
            },
            timeout: Some(Duration::from_secs(15)),
            ..Default::default()
        };
        let mut displayed = false;
        let result = rt
            .login(&options, &mut |event| {
                if let LoginEvent::DeviceCode {
                    user_code,
                    verification_uri,
                } = event
                {
                    assert_eq!(verification_uri, format!("{base}/codex/device"));
                    assert_eq!(user_code, CODE);
                    displayed = true;
                    if cancel {
                        options.cancellation.cancel();
                    } else {
                        state.shown.store(true, Ordering::SeqCst);
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
        assert!(displayed, "no pairing event: {result:?}");
        if cancel {
            assert_eq!(result, LoginResult::Cancelled);
            assert!(!profile.join("auth.json").exists());
        } else {
            let LoginResult::LoggedIn {
                account: Some(account),
            } = result
            else {
                panic!("unexpected login result {result:?}")
            };
            assert_eq!(account.email.as_deref(), Some(EMAIL));
            assert_eq!(account.plan.as_deref(), Some("plus"));
            assert_eq!(account.account_id.as_deref(), Some(ACCOUNT));
            assert!(profile.join("auth.json").is_file());
        }
    }
    assert_eq!(state.exchanges.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
#[ignore = "requires OAR_KIMI_BIN with region-aware device login"]
async fn kimi_native_device_login_status_and_logout() {
    let mut runtime = kar::runtime("kimi").unwrap();
    runtime.program = std::env::var_os("OAR_KIMI_BIN")
        .expect("set OAR_KIMI_BIN")
        .into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let page = format!("{origin}/device?code={CODE}");
    let handoff = json!({"device_code":"local-only-device","user_code":CODE,"verification_uri":format!("{origin}/device"),"verification_uri_complete":page,"expires_in":60,"interval":1});
    let state = Arc::new(Issuer::default());
    let app = Router::new()
        .route("/api/oauth/device_authorization", post(move || async { Json(handoff) }))
        .route("/api/oauth/token", post(|State(s): State<Arc<Issuer>>| async move {
            if !s.shown.load(Ordering::SeqCst) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":"authorization_pending"})));
            }
            s.exchanges.fetch_add(1, Ordering::SeqCst);
            (StatusCode::OK, Json(json!({"access_token":"local-kimi-grant","refresh_token":"local-only-refresh","expires_in":3600,"token_type":"Bearer"})))
        }))
        .route("/models", get(|| async { Json(json!({"data":[{"id":"fixture-model","display_name":"Fixture model","limit":{"context":32768},"protocol":"anthropic"}]})) }))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("kimi");
    fs::create_dir(&profile).unwrap();
    let mut env = private_env(home.path());
    env.insert(
        "OAR_KIMI_BIN".into(),
        Some(runtime.program.as_os_str().into()),
    );
    for key in ["KIMI_CODE_HOME", "KIMI_SHARE_DIR"] {
        env.insert(key.into(), Some(profile.as_os_str().into()));
    }
    for key in [
        "KIMI_CODE_OAUTH_HOST",
        "KIMI_OAUTH_HOST",
        "KIMI_CODE_BASE_URL",
    ] {
        env.insert(key.into(), Some(origin.clone().into()));
    }
    env.insert("KIMI_DISABLE_TELEMETRY".into(), Some("1".into()));
    let scoped = AuthStatusOptions {
        installation: InstallationOptions {
            cwd: Some(home.path().into()),
            env,
        },
        timeout: Some(Duration::from_secs(25)),
    };
    for cancel in [true, false] {
        let options = LoginOptions {
            installation: scoped.installation.clone(),
            timeout: scoped.timeout,
            kimi_region: Some(KimiRegion::Global),
            open_browser: Some(false),
            ..Default::default()
        };
        let mut handoffs = 0;
        let result = runtime
            .login(&options, &mut |event| {
                if let LoginEvent::DeviceCode {
                    user_code,
                    verification_uri,
                } = event
                {
                    assert_eq!(user_code, CODE);
                    assert_eq!(verification_uri, page);
                    handoffs += 1;
                    if cancel {
                        options.cancellation.cancel();
                    } else {
                        state.shown.store(true, Ordering::SeqCst);
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(handoffs, 1, "{result:?}");
        let status = runtime.auth_status(&scoped).await.unwrap();
        if cancel {
            assert_eq!(result, LoginResult::Cancelled);
            assert!(matches!(status, AuthStatus::LoggedOut { .. }), "{status:?}");
        } else {
            assert!(matches!(result, LoginResult::LoggedIn { .. }), "{result:?}");
            assert!(matches!(status, AuthStatus::LoggedIn { .. }), "{status:?}");
            assert!(
                fs::read_to_string(profile.join("config.toml"))
                    .unwrap()
                    .contains("fixture-model")
            );
            assert_eq!(
                runtime.logout(&scoped).await.unwrap(),
                LogoutResult::LoggedOut
            );
            assert!(matches!(
                runtime.auth_status(&scoped).await.unwrap(),
                AuthStatus::LoggedOut { .. }
            ));
            assert_eq!(
                fs::read_dir(profile.join("credentials")).unwrap().count(),
                0
            );
        }
    }
    assert_eq!(state.exchanges.load(Ordering::SeqCst), 1);
    server.abort();
}
