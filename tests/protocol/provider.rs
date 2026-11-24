use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::any,
};
use base64::Engine;
use kar::{
    Error, InstallationOptions, Result,
    account::LoginCancellation,
    providers::{Method, Prompt, ProviderAuthOptions, ProviderEvent, ProviderInteraction},
};
use serde_json::{Value, json};
use std::{fs, io, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::net::TcpListener;

#[path = "opencode_inventory.rs"]
mod inventory;

const SECRET: &str = "private-fixture-provider-value";
fn case() -> String {
    std::env::var("OAR_PROVIDER_TEST").unwrap()
}
async fn serve(req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let pwd = std::env::var("OPENCODE_PASSWORD").unwrap();
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("opencode:{pwd}"))
    );
    if parts.headers["authorization"] != expected {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let how = case();
    if how.starts_with("skills_") {
        return inventory::serve(&how, &parts);
    }
    let method = parts.method.as_str();
    let path = parts.uri.path();
    let data = to_bytes(body, 1024 * 128).await.unwrap();
    let val: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
    let integration = || {
        let connections = if Path::new("paired").exists() {
            json!([{"id":"fresh","type":"credential","method":"oauth","label":"Provider"},{"id":"old","type":"credential"}])
        } else {
            json!([{"id":"old","type":"credential"}])
        };
        json!({"id":"example","name":"Example","methods":[{"type":"oauth","id":"other","label":"Other"},{"type":"oauth","id":"device","label":"Device","form":[
            {"key":"region","type":"string","required":true,"options":[{"value":"custom","label":"Other"}]},
            {"key":"workspace","type":"number","required":true,"when":[{"key":"region","op":"eq","value":"custom"}]},
            {"key":"unused","type":"string","required":true,"when":[{"key":"region","op":"neq","value":"custom"}]}
        ]}],"connections":connections})
    };
    let v = match (method, path) {
        ("GET", "/api/plugin") => json!({"data":[{}]}),
        ("GET", "/api/integration") => json!({"data":[integration()]}),
        ("GET", "/api/integration/example") => json!({"data":integration()}),
        ("GET", "/api/integration/another") => {
            json!({"data":{"id":"another","methods":[],"connections":[]}})
        }
        ("POST", "/api/integration/example/connect/oauth") => {
            fs::write("authorizing", "").unwrap();
            assert_eq!(
                val,
                json!({"methodID":"device","answer":{"region":"custom","workspace":42}})
            );
            let url = if how == "refuse" || how == "paste" {
                "https://example.test/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A1717%2Fcb"
            } else {
                "https://example.test/device"
            };
            json!({"data":{"url":url,"mode":if how=="paste"||how=="race"{"code"}else{"auto"},"attemptID":"try-one","instructions":if how=="paste" || how=="race" {"Paste code"}else{"Open this and enter code: LOCAL-PAIR"},"time":{"created":10,"expires":60010}}})
        }
        ("GET", "/api/integration/example/connect/oauth/try-one") => match how.as_str() {
            "reject" => {
                return (StatusCode::BAD_REQUEST, Json(json!({"message":SECRET}))).into_response();
            }
            "expired" => json!({"data":{"status":"expired"}}),
            "timeout" | "cancel" | "drop" | "refuse" => json!({"data":{"status":"pending"}}),
            "paste" if !Path::new("pasted").exists() => json!({"data":{"status":"pending"}}),
            "race" if !Path::new("prompted").exists() => json!({"data":{"status":"pending"}}),
            _ => {
                fs::write("paired", "").unwrap();
                json!({"data":{"status":"complete"}})
            }
        },
        ("POST", "/api/integration/example/connect/oauth/try-one/complete") => {
            assert_eq!(val, json!({"code":SECRET}));
            fs::write("pasted", "").unwrap();
            return StatusCode::NO_CONTENT.into_response();
        }
        ("DELETE", "/api/integration/example/connect/oauth/try-one") => {
            fs::write("cancelled", "").unwrap();
            return StatusCode::NO_CONTENT.into_response();
        }
        ("DELETE", "/api/credential/old") => {
            fs::write("removed", "").unwrap();
            return StatusCode::NO_CONTENT.into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(v).into_response()
}
pub(super) async fn peer(args: &[String]) {
    if args == ["--help"] {
        return println!("opencode <subcommand>");
    }
    if args == ["--version"] {
        return println!("opencode v{}", inventory::version(&case()));
    }
    if args == ["debug", "skill"] {
        return inventory::legacy(&case());
    }
    assert_eq!(
        args,
        ["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"]
    );
    assert!(std::env::var_os("OPENCODE_SERVER_USERNAME").is_none());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    fs::write("address", addr.to_string()).unwrap();
    println!("{}", json!({"url":format!("http://{addr}")}));
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
        let _ = tx.send(());
    });
    axum::serve(listener, Router::new().fallback(any(serve)))
        .with_graceful_shutdown(async {
            let _ = rx.await;
        })
        .await
        .unwrap();
}
struct Host<'a> {
    dir: &'a Path,
    case: &'a str,
    cancel: LoginCancellation,
    events: usize,
    manual: usize,
}
impl ProviderInteraction for Host<'_> {
    fn event(&mut self, e: ProviderEvent) -> Result<()> {
        self.events += 1;
        match e {
            ProviderEvent::DeviceCode {
                user_code,
                expires_in_seconds,
                ..
            } => {
                assert_eq!(user_code, "LOCAL-PAIR");
                assert_eq!(expires_in_seconds, Some(60));
            }
            ProviderEvent::AuthUrl { url, .. } => assert!(url.starts_with("https://example.test/")),
            _ => panic!(),
        }
        if self.case == "cancel" {
            self.cancel.cancel();
        }
        if self.case == "interact" {
            return Err(Error::Refused(SECRET.into()));
        }
        Ok(())
    }
    async fn prompt(&mut self, p: Prompt) -> Result<String> {
        Ok(match p {
            Prompt::Select { options, .. } if options[0].id == "other" => "device".into(),
            Prompt::Select { options, .. } => options[0].id.clone(),
            Prompt::Text { message, .. } => {
                assert_eq!(message, "workspace");
                "42".into()
            }
            Prompt::ManualCode { .. } => {
                self.manual += 1;
                if self.case == "race" {
                    fs::write(self.dir.join("prompted"), "").unwrap();
                    return std::future::pending().await;
                }
                format!(" {SECRET} ")
            }
            _ => panic!(),
        })
    }
}
fn options(h: &Path, case: &str) -> ProviderAuthOptions {
    let mut env: std::collections::BTreeMap<_, _> =
        std::env::vars_os().map(|(k, _)| (k, None)).collect();
    for k in ["HOME", "USERPROFILE"] {
        env.insert(k.into(), Some(h.as_os_str().into()));
    }
    env.insert("OAR_PROVIDER_TEST".into(), Some(case.into()));
    env.insert(
        "OAR_OPENCODE_BIN".into(),
        Some(std::env::current_exe().unwrap().into_os_string()),
    );
    ProviderAuthOptions {
        installation: InstallationOptions {
            cwd: Some(h.into()),
            env,
        },
        idle_timeout: Duration::from_millis(100),
        timeout: Duration::from_secs(5),
        login_timeout: if case == "timeout" {
            Duration::from_millis(300)
        } else {
            Duration::from_secs(5)
        },
        refuse_loopback: true,
    }
}
pub(super) async fn exercise() {
    inventory::exercise().await;
    killed_host().await;
    for case in [
        "device", "paste", "race", "cancel", "timeout", "reject", "expired", "refuse", "interact",
        "drop",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path();
        let auth = Arc::new(
            kar::runtime("opencode")
                .unwrap()
                .opencode_provider_auth(options(h, case))
                .await
                .unwrap(),
        );
        let cancel = LoginCancellation::default();
        let mut host = Host {
            dir: h,
            case,
            cancel: cancel.clone(),
            events: 0,
            manual: 0,
        };
        if case == "drop" {
            let task = auth.login("example", Method::Oauth, &mut host, &cancel);
            {
                tokio::pin!(task);
                let concurrent = async {
                    while !h.join("authorizing").exists() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    auth.logout("another").await.unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(60), auth.logout("example"))
                            .await
                            .is_err()
                    );
                };
                tokio::select! {r=&mut task=>panic!("unexpected result {r:?}"),r=tokio::time::timeout(Duration::from_secs(2),concurrent)=>r.unwrap()}
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                while !h.join("cancelled").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        } else {
            let r = auth
                .login("example", Method::Oauth, &mut host, &cancel)
                .await;
            if matches!(case, "device" | "paste" | "race") {
                assert_eq!(r.unwrap().method, Some(Method::Oauth));
                assert!(h.join("removed").exists());
                assert_eq!(host.manual, usize::from(case != "device"));
            } else {
                use kar::providers::ProviderErrorKind as Kind;
                let Error::Provider(err) = r.unwrap_err() else {
                    panic!("missing typed {case} failure")
                };
                let expected = match case {
                    "timeout" => Kind::TimedOut,
                    "cancel" => Kind::Cancelled,
                    "expired" => Kind::Expired,
                    "interact" => Kind::InteractionFailed,
                    "refuse" => Kind::LoopbackRequired,
                    _ => Kind::Rejected,
                };
                assert_eq!(err.kind, expected);
                assert!(!err.to_string().contains(SECRET));
                if case == "reject" {
                    assert_eq!(err.status, Some(400));
                }
                assert!(
                    h.join("cancelled").exists(),
                    "{case} cancelled native attempt before returning"
                );
                assert!(!h.join("removed").exists());
            }
            assert_eq!(host.events, usize::from(case != "refuse"), "{case}");
        }
        if case == "device" {
            let addr: SocketAddr = fs::read_to_string(h.join("address"))
                .unwrap()
                .parse()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while tokio::net::TcpStream::connect(addr).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert!(auth.status("example").await.unwrap().configured);
        }
        auth.close().await;
    }
}

async fn killed_host() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let opts = options(dir.path(), "drop");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.current_dir(dir.path()).env_clear();
    for (k, v) in opts.installation.env {
        if let Some(v) = v {
            cmd.env(k, v);
        }
    }
    let mut host = cmd
        .args(["providers", "opencode", "login", "example", "oauth"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut out = BufReader::new(host.stdout.take().unwrap()).lines();
    let prompt = tokio::time::timeout(Duration::from_secs(6), out.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&prompt).unwrap()["prompt"]["kind"],
        "select"
    );
    let addr: SocketAddr = fs::read_to_string(dir.path().join("address"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_ok());
    host.kill().await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while tokio::net::TcpStream::connect(addr).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
