//! macOS native-process acceptance for a turn whose runtime stops reading stdin.
//! The separate Cargo target owns one agent process group at a time.
#![cfg(target_os = "macos")]
use axum::{
    Router,
    extract::State,
    http::{StatusCode, Uri},
    routing::post,
};
use kar::{Direction, Event, Outcome, SessionOptions, runtime};
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::{Pid, getpgid},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify};
static OWNER: Mutex<()> = Mutex::const_new(());

async fn hold(State(seen): State<Arc<Notify>>, uri: Uri) -> StatusCode {
    if uri.path().ends_with("/messages") || uri.path().ends_with("/responses") {
        seen.notify_one();
        return std::future::pending().await;
    }
    StatusCode::NOT_FOUND
}

async fn stuck(provider: &str) {
    let _guard = OWNER.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&work).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Notify::new());
    let app = Router::new().fallback(post(hold)).with_state(seen.clone());
    let model = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut opts = SessionOptions::new(&work);
    opts.env.insert("HOME".into(), home.clone().into());
    match provider {
        "codex" => {
            opts.env.insert("CODEX_HOME".into(), home.clone().into());
            opts.env
                .insert("KAR_FIXTURE_KEY".into(), "loopback-only".into());
            std::fs::write(home.join("config.toml"),format!("model = \"gpt-5.1\"\nmodel_provider = \"fixture\"\n[model_providers.fixture]\nname = \"local\"\nbase_url = \"{endpoint}/v1\"\nenv_key = \"KAR_FIXTURE_KEY\"\nwire_api = \"responses\"\n")).unwrap();
        }
        "claude" => {
            opts.env.insert("CLAUDE_CONFIG_DIR".into(), home.into());
            for (k, v) in [
                ("ANTHROPIC_BASE_URL", endpoint.as_str()),
                ("ANTHROPIC_API_KEY", "loopback-only"),
                ("ANTHROPIC_AUTH_TOKEN", ""),
                ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ] {
                opts.env.insert(k.into(), v.into());
            }
            opts.model = Some("sonnet".into());
        }
        _ => unreachable!(),
    }
    let session = runtime(provider).unwrap().open(opts).await.unwrap();
    let turn = session
        .prompt("Wait for the model response.")
        .await
        .unwrap();
    let admission = turn.admission().expect("public prompt decision");
    assert_eq!(admission.response.native["type"], "accepted");
    tokio::time::timeout(Duration::from_secs(45), seen.notified())
        .await
        .unwrap();
    // A stopped process continues owning its sockets and stdout; only actual
    // process-group teardown can finish this case. Limit the signal to our child.
    let list = std::process::Command::new("pgrep")
        .args(["-P", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(list.status.success());
    let raw = String::from_utf8(list.stdout).unwrap();
    let pid = Pid::from_raw(raw.trim().parse().expect("exactly one owned native child"));
    assert_eq!(getpgid(Some(pid)).unwrap(), pid);
    killpg(pid, Signal::SIGSTOP).unwrap();
    tokio::time::timeout(Duration::from_secs(17), session.abort())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(turn.wait().await.unwrap(), Outcome::Aborted);
    let mut replay = session.subscribe(None).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while replay.next().await.is_some() {}
    })
    .await
    .unwrap();
    assert_eq!(killpg(pid, None), Err(Errno::ESRCH));
    let records = session.records();
    let requests: Vec<_> = records
        .iter()
        .filter(|r| r.direction == Direction::ToRuntime && r.native["type"] == "abort")
        .collect();
    assert_eq!(requests.len(), 1);
    let id = requests[0].native["id"].as_str().unwrap();
    let decisions: Vec<_> = records
        .iter()
        .filter(|r| r.direction == Direction::Host && r.control_response_to() == Some(id))
        .collect();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].native["type"], "accepted");
    assert_eq!(
        records
            .iter()
            .filter(|r| r.direction == Direction::ToRuntime
                && (r.native["method"] == "turn/interrupt"
                    || r.native
                        .pointer("/request/subtype")
                        .is_some_and(|v| v == "interrupt")))
            .count(),
        1
    );
    for _ in 0..2 {
        let mut history = session.subscribe(None).unwrap();
        let mut ends = Vec::new();
        let mut exits = 0;
        while let Some(frame) = history.next().await {
            for event in &frame.events {
                match event {
                    Event::TurnEnded { outcome } if frame.agent_path.is_empty() => {
                        ends.push(outcome.clone())
                    }
                    Event::Exited { .. } => exits += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(ends, vec![Outcome::Aborted]);
        assert_eq!(exits, 1);
    }
    model.abort();
    let _ = model.await;
}

#[tokio::test]
#[ignore = "requires native Codex with a loopback Responses endpoint"]
async fn codex_unanswered_interrupt() {
    stuck("codex").await;
}
#[tokio::test]
#[ignore = "requires native Claude with a loopback Messages endpoint"]
async fn claude_unanswered_interrupt() {
    stuck("claude").await;
}
