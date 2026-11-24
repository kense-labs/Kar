//! Real Claude Code, a temporary profile and a TLS proxy that answers every request locally.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    fs, io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{TlsAcceptor, rustls};

#[derive(Default)]
struct Backend {
    paths: Mutex<Vec<String>>,
    altered: AtomicBool,
    spent: AtomicBool,
}
const ORG: &str = "00000000-0000-4000-8000-000000000002";
const USER: &str = "00000000-0000-4000-8000-000000000001";
fn later(hours: i64) -> String {
    (jiff::Timestamp::now() + jiff::SignedDuration::from_hours(hours)).to_string()
}
impl Backend {
    fn respond(&self, path: &str) -> Value {
        self.paths.lock().unwrap().push(path.to_owned());
        if path.ends_with("/profile") {
            json!({"account":{"uuid":USER,"email":"fixture@example.invalid","display_name":"Test user"},"organization":{"uuid":ORG,"organization_type":"claude_pro","rate_limit_tier":"default_claude_pro","cc_onboarding_flags":{}}})
        } else if path.contains("/api/eval") {
            let mut f = json!({"enabled":true,"version":1});
            if self.altered.load(Ordering::SeqCst) {
                f["confirmTitle"] = "Changed remote title for {kind}".into();
                f["confirmYes"] = "Changed remote selection".into();
            }
            json!({"features":{"tengu_cedar_ember":{"defaultValue":f}}})
        } else if path.ends_with("/usage") {
            let later_day = later(24);
            let spent = self.spent.load(Ordering::SeqCst);
            json!({"five_hour":{"utilization":0,"resets_at":later_day},"seven_day":{"utilization":if spent{0}else{100},"resets_at":later_day},
            "cedar_ember":{"eligible":true,"at_limit":!spent,"exhausted":if spent{json!([])}else{json!(["seven_day"])},"weekly_resets_at":later_day,"next_grant_id":"local-weekly","grants":[{"id":"local-weekly","label":"Weekly reset","resets_total":2,"resets_left":if spent{1}else{2},"starts_at":later(-1),"ends_at":later_day,"clears":["seven_day"],"paused":false,"usable_now":!spent,"use_requires_limit":true,"percent_used":{"seven_day":100},"blocking":[],"type":"weekly","gives_back_weekly":false}]}})
        } else if path.contains("reset_rate_limits") {
            self.spent.store(true, Ordering::SeqCst);
            json!({"result":"reset","cleared":["seven_day"],"resets_left":1,"weekly_resets_at":later(24)})
        } else if path.ends_with("/v1/messages") {
            json!({"id":"synthetic","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
        } else {
            json!({})
        }
    }
}
async fn headers<S: AsyncRead + Unpin>(s: &mut BufReader<S>) -> io::Result<(String, usize)> {
    let mut line = String::new();
    s.read_line(&mut line).await?;
    let mut len = 0;
    let mut total = 0;
    loop {
        let mut l = String::new();
        let n = s.read_line(&mut l).await?;
        total += n;
        if n == 0 || l == "\r\n" {
            break;
        }
        if total > 65536 {
            return Err(io::Error::other("oversized fixture request"));
        }
        if let Some((k, v)) = l.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    Ok((line, len))
}
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(s: S, backend: &Backend) -> io::Result<()> {
    let mut s = BufReader::new(s);
    let (req, n) = headers(&mut s).await?;
    if n > 0 {
        tokio::io::copy(&mut (&mut s).take(n as u64), &mut tokio::io::sink()).await?;
    }
    let path = req
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");
    let body = backend.respond(path).to_string();
    s.write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await?;
    s.shutdown().await
}
async fn backend(root: &Path, backend: Arc<Backend>) -> String {
    let names = [
        "api.anthropic.com",
        "claude.ai",
        "statsig.anthropic.com",
        "console.anthropic.com",
        "platform.claude.com",
        "claude.com",
        "api.claude.com",
        "statsig.com",
        "api.statsig.com",
    ];
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(names.map(String::from).to_vec()).unwrap();
    fs::write(root.join("test-ca.pem"), cert.pem()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    )
    .unwrap();
    let accept = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let back = backend.clone();
            let tls = accept.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                let (line, _) = headers(&mut stream).await?;
                if !line.starts_with("CONNECT ") {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
                        )
                        .await?;
                    return Ok::<(), io::Error>(());
                }
                stream
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                answer(tls.accept(stream).await?, &back).await
            });
        }
    });
    format!("http://{addr}")
}
#[tokio::test]
#[ignore = "requires a native Claude Code with /limit-reset, all services use synthetic data locally"]
async fn real_dialog_and_changed_remote_labels() {
    let cli = std::env::var_os("KAR_CLAUDE_BIN").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let config = h.join("config");
    fs::create_dir(&config).unwrap();
    let b = Arc::new(Backend::default());
    let url = backend(h, b.clone()).await;
    let profile = || json!({"hasCompletedOnboarding":true,"lastOnboardingVersion":"2.1.330","theme":"dark","oauthAccount":{"accountUuid":USER,"emailAddress":"fixture@example.invalid","organizationUuid":ORG,"displayName":"Local Fixture"},"cachedGrowthBookFeatures":{"tengu_cedar_ember":{"enabled":true,"version":1}}});
    fs::write(config.join(".claude.json"), profile().to_string()).unwrap();
    fs::write(config.join(".credentials.json"),json!({"claudeAiOauth":{"accessToken":"synthetic-reset-test","refreshToken":"synthetic-refresh","expiresAt":jiff::Timestamp::now().as_millisecond()+3600000,"scopes":["user:profile","user:inference"],"subscriptionType":"pro","rateLimitTier":"default_claude_pro"}}).to_string()).unwrap();
    async fn call(
        cli: &std::ffi::OsStr,
        h: &Path,
        url: &str,
        action: &str,
        args: &[&str],
    ) -> Value {
        let o = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", h)
            .env("CLAUDE_CONFIG_DIR", h.join("config"))
            .env("HTTPS_PROXY", url)
            .env("HTTP_PROXY", url)
            .env("ALL_PROXY", url)
            .env("NODE_EXTRA_CA_CERTS", h.join("test-ca.pem"))
            .env("DISABLE_AUTOUPDATER", "1")
            .env("DISABLE_ERROR_REPORTING", "1")
            .env("LANG", "en_US.UTF-8")
            .args(["account", "claude", action, "--bin"])
            .arg(cli)
            .arg("--cwd")
            .arg(h)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    }
    let report = call(&cli, h, &url, "inspect-limit-resets", &[]).await;
    let routes = || b.paths.lock().unwrap().clone();
    assert_eq!(report["kind"], "inspected", "{report} / {:?}", routes());
    let weekly = &report["resets"][1];
    assert_eq!(
        (&weekly["state"], &weekly["resetsLeft"]),
        (&json!("available"), &json!(2)),
        "{report} / {:?}",
        routes()
    );
    assert_eq!(report["resets"][0]["state"], "not_checked");
    assert_eq!(report["resets"][2]["state"], "none", "{report}");
    assert_eq!(report["resets"][3]["state"], "none", "{report}");
    assert!(routes().iter().any(|p| p.ends_with("/usage")));
    assert!(!b.spent.load(Ordering::SeqCst));
    b.altered.store(true, Ordering::SeqCst);
    // Remove the cached payload so the second native invocation picks up the modified labels.
    let mut next = profile();
    next.as_object_mut()
        .unwrap()
        .remove("cachedGrowthBookFeatures");
    fs::write(config.join(".claude.json"), next.to_string()).unwrap();
    let args = [
        "--idempotency-key",
        "changed-dialog",
        "--reset-id",
        "weekly",
        "--five-hour-limit-consent",
    ];
    let refused = call(&cli, h, &url, "redeem-limit-reset", &args).await;
    assert_eq!(refused["kind"], "unknown", "{refused} / {:?}", routes());
    assert!(!b.spent.load(Ordering::SeqCst));
    let before = routes();
    assert_eq!(
        call(&cli, h, &url, "redeem-limit-reset", &args).await,
        refused
    );
    assert_eq!(routes(), before);
    b.altered.store(false, Ordering::SeqCst);
    fs::write(config.join(".claude.json"), profile().to_string()).unwrap();
    let spend = [
        "--idempotency-key",
        "original-dialog",
        "--reset-id",
        "weekly",
    ];
    let completed = call(&cli, h, &url, "redeem-limit-reset", &spend).await;
    assert_eq!(
        routes()
            .iter()
            .filter(|p| p.contains("reset_rate_limits"))
            .count(),
        1,
        "{completed} / {:?}",
        routes()
    );
    assert_eq!(completed["kind"], "reset", "{completed}");
    let after = routes();
    assert_eq!(
        call(&cli, h, &url, "redeem-limit-reset", &spend).await,
        completed
    );
    assert_eq!(routes(), after);
}
