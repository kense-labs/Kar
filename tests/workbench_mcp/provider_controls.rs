use super::*;
use std::io::BufRead;
pub(super) fn claude_peer() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|s| s == "--version") {
        println!("2.1.330 (Claude Code)");
        return;
    }
    if let Some(config) = args.windows(2).find(|a| a[0] == "--mcp-config") {
        let _ = fs::read_to_string(&config[1]);
    }
    let session = &args
        .windows(2)
        .find(|a| a[0] == "--session-id" || a[0] == "--resume")
        .unwrap()[1];
    let db = std::path::PathBuf::from(std::env::var_os("FIXTURE_NATIVE_DB").unwrap());
    let mut pasted = false;
    let mut connected = false;
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["type"] != "control_request" {
            continue;
        }
        let q = &v["request"];
        let r = match q["subtype"].as_str().unwrap() {
            "get_status" => json!({"sections":[{"rows":[{"label":"Session ID","value":session}]}]}),
            "initialize" => json!({"commands":[]}),
            "mcp_status" => {
                json!({"mcpServers":[{"name":"vault","status":if connected{"connected"}else{"needs-auth"}}]})
            }
            "mcp_authenticate" => {
                if q["serverName"] == "refused" {
                    peer::emit(
                        json!({"type":"control_response","response":{"subtype":"error","request_id":v["request_id"],"error":"private-fixture-claude-error"}}),
                    );
                    continue;
                }
                assert_eq!(q["serverName"], "vault");
                json!({"requiresUserAction":true,"authUrl":"https://id.example.org/authorize?state=private-fixture-state&redirect_uri=http%3A%2F%2Flocalhost%3A4000%2Fcb"})
            }
            "mcp_oauth_callback_url" => {
                assert_eq!(q["serverName"], "vault");
                assert!(
                    q["callbackUrl"]
                        .as_str()
                        .unwrap()
                        .contains("code=private-fixture-code")
                );
                assert!(!pasted);
                pasted = true;
                json!({})
            }
            "mcp_reconnect" => {
                assert!(pasted);
                assert_eq!(q["serverName"], "vault");
                connected = true;
                fs::write(db.join("claude-reconnected"), "ok").unwrap();
                json!({})
            }
            _ => json!({}),
        };
        peer::emit(
            json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":r}}),
        );
    }
}
fn check_private(dir: &std::path::Path) {
    for item in fs::read_dir(dir).unwrap() {
        let item = item.unwrap();
        let meta = item.file_type().unwrap();
        if meta.is_dir() {
            check_private(&item.path());
        }
        if !meta.is_file() {
            continue;
        }
        let bytes = fs::read(item.path()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for secret in [
            "private-fixture-state",
            "private-fixture-code",
            "private-fixture-claude-error",
            "private-fixture-native-error",
            "private-fixture-feedback-reason",
        ] {
            assert!(
                !text.contains(secret),
                "private provider data written to {}",
                item.path().display()
            );
        }
    }
}
pub(super) async fn scenario() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let root = dir.join("work");
    let state = dir.join("state");
    let db = dir.join("native");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&db).unwrap();
    let claude = dir.join("claude-control-fixture");
    fs::hard_link(std::env::current_exe().unwrap(), &claude).unwrap();
    let observer = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = observer.local_addr().unwrap().to_string();
    let vars = [
        ("KAR_TEST_MCP_OBSERVER", addr.as_ref()),
        ("KAR_CLAUDE_BIN", claude.as_os_str()),
    ];
    let (mut kar, url) = server::serve_with_env(&state, &root, &db, &vars).await;
    let c = server::client();
    let p = server::json(
        c.post(format!("{url}/projects"))
            .json(&json!({"name":"OAuth fixture","root":root})),
    )
    .await;
    let chats = format!("{url}/projects/{}/chats", p["id"].as_str().unwrap());
    for provider in ["codex", "claude"] {
        let chat = Uuid::new_v4();
        server::json(c.post(&chats).json(&json!({"id":chat,"runtime":provider}))).await;
        let base = format!("{chats}/{chat}");
        let a = server::json(c.get(format!("{base}/provider-controls"))).await;
        assert_eq!(a["mcp_sign_in"], true);
        assert_eq!(a["upload_diagnostic_feedback"], provider == "codex");
        let stream = &a["stream"];
        let client_key = Uuid::new_v4().simple().to_string();
        let other = Uuid::new_v4().simple().to_string();
        let oauth = format!("{base}/mcp-sign-in");
        let start = |id, server: &str| {
            c.post(&oauth)
                .header("kar-auth-client", &client_key)
                .json(&json!({"id":id,"stream":stream,"server":server}))
        };
        let err = start(Uuid::new_v4(), "refused").send().await.unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(!err.text().await.unwrap().contains("private-fixture"));
        let id = Uuid::new_v4();
        let started = server::json(start(id, "vault")).await;
        assert_eq!(server::json(start(id, "vault")).await, started);
        let u = url::Url::parse(started["authorization_url"].as_str().unwrap()).unwrap();
        let to = |key: &str| {
            u.query_pairs()
                .find(|(k, _)| k == key)
                .unwrap()
                .1
                .into_owned()
        };
        let state = to("state");
        let mut cb = url::Url::parse(&to("redirect_uri")).unwrap();
        cb.query_pairs_mut()
            .append_pair("code", "private-fixture-code")
            .append_pair("state", &state);
        let finish = |key: &str, raw: &str| {
            c.post(format!("{oauth}/{id}"))
                .header("kar-auth-client", key)
                .json(&json!({"callback_url":raw}))
        };
        assert_eq!(
            finish(&other, cb.as_str()).send().await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        let wrong = cb.as_str().replace(&state, "wrong");
        assert_eq!(
            finish(&client_key, &wrong).send().await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            server::json(finish(&client_key, cb.as_str())).await["status"],
            "complete"
        );
        assert_eq!(
            server::json(finish(&client_key, cb.as_str())).await["status"],
            "complete"
        );
        let command = Uuid::new_v4();
        let upload = || {
            c.post(format!("{base}/provider-feedback")).json(&json!({"stream":stream,"command_id":command,"reason":"private-fixture-feedback-reason"}))
        };
        if provider == "codex" {
            for _ in 0..2 {
                assert_eq!(
                    server::json(upload()).await["feedback_id"],
                    "provider-receipt"
                );
            }
            assert_eq!(
                fs::read_to_string(db.join("feedback-count")).unwrap(),
                "upload\n"
            );
        } else {
            assert_eq!(
                upload().send().await.unwrap().status(),
                StatusCode::BAD_REQUEST
            );
        }
    }
    assert!(db.join("claude-reconnected").exists());
    kar.kill().await.unwrap();
    check_private(&state);
}
