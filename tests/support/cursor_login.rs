use super::*;
use axum::{extract::Query, http::header, response::IntoResponse};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

#[derive(Default)]
struct Flow {
    handoff: Mutex<Option<(String, String)>>,
    exchanged: AtomicUsize,
}

async fn poll_login(
    State(flow): State<Arc<Flow>>,
    Query(body): Query<BTreeMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let state = flow.handoff.lock().unwrap();
    let Some((uuid, challenge)) = &*state else {
        return (StatusCode::NOT_FOUND, Json(json!({})));
    };
    assert_eq!(&body["uuid"], uuid);
    let verifier = &body["verifier"];
    assert_eq!(
        &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        challenge
    );
    flow.exchanged.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        Json(json!({"accessToken":"local-cursor-access","refreshToken":"local-cursor-refresh"})),
    )
}

async fn profile() -> impl IntoResponse {
    let mut result = vec![0x10, 7, 0x1a, EMAIL.len() as u8];
    result.extend(EMAIL.as_bytes());
    ([(header::CONTENT_TYPE, "application/proto")], result)
}

#[tokio::test]
#[ignore = "requires KAR_CURSOR_BIN with native file credential store"]
async fn native_cursor_login_and_cancellation() {
    let mut rt = kar::runtime("cursor").unwrap();
    rt.program = std::env::var_os("KAR_CURSOR_BIN")
        .expect("set KAR_CURSOR_BIN")
        .into();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let flow = Arc::new(Flow::default());
    let router = Router::new()
        .route("/auth/poll", get(poll_login))
        .route("/aiserver.v1.DashboardService/GetMe", post(profile))
        .with_state(flow.clone());
    let server = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let credentials = home.path().join(".cursor/auth.json");
    for (cancel, logged_in) in [(true, false), (false, true), (true, true)] {
        *flow.handoff.lock().unwrap() = None;
        let previous = fs::read(&credentials).ok();
        let mut env = private_env(home.path());
        // The native auth transport uses the proxy even for a loopback endpoint.
        for key in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
            env.insert(key.into(), None);
        }
        env.insert("KAR_CURSOR_BIN".into(), Some(rt.program.as_os_str().into()));
        for (key, value) in [
            ("AGENT_CLI_CREDENTIAL_STORE", "file"),
            ("CURSOR_CLI_DPOP_LOGIN", "0"),
            ("CURSOR_API_ENDPOINT", &base),
            ("CURSOR_WEBSITE_URL", &base),
            ("DO_NOT_TRACK", "1"),
        ] {
            env.insert(key.into(), Some(value.into()));
        }
        let opts = LoginOptions {
            installation: InstallationOptions {
                cwd: Some(home.path().into()),
                env,
            },
            open_browser: Some(false),
            timeout: Some(Duration::from_secs(16)),
            ..Default::default()
        };
        let mut links = 0;
        let result = rt
            .login(&opts, &mut |e| {
                if let LoginEvent::AuthUrl { url, .. } = e {
                    links += 1;
                    let url = reqwest::Url::parse(&url).unwrap();
                    assert_eq!(url.origin().ascii_serialization(), base);
                    assert_eq!(url.path(), "/loginDeepControl");
                    let q: BTreeMap<_, _> = url.query_pairs().collect();
                    assert!(uuid::Uuid::parse_str(&q["uuid"]).is_ok());
                    if cancel {
                        opts.cancellation.cancel()
                    } else {
                        *flow.handoff.lock().unwrap() =
                            Some((q["uuid"].to_string(), q["challenge"].to_string()));
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(links, 1, "{result:?}");
        let status = rt
            .auth_status(&AuthStatusOptions {
                installation: opts.installation.clone(),
                timeout: Some(Duration::from_secs(6)),
            })
            .await
            .unwrap();
        if cancel {
            assert_eq!(result, LoginResult::Cancelled);
            assert!(
                fs::read(&credentials).ok() == previous,
                "cancellation changed stored credentials"
            );
        } else {
            assert!(matches!(result, LoginResult::LoggedIn { .. }), "{result:?}");
            assert!(credentials.is_file());
        }
        if logged_in {
            assert!(matches!(status, AuthStatus::LoggedIn { .. }));
        } else {
            assert!(matches!(status, AuthStatus::LoggedOut { .. }));
        }
        assert_eq!(
            flow.exchanged.load(Ordering::SeqCst),
            usize::from(logged_in)
        );
    }
    server.abort();
}
