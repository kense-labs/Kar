//! Installed app-server, isolated configuration, controlled HTTP Responses API provider.
use super::{SessionOptions, Value, json};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use kar::{Event, Outcome, RecordKind};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

const HOLD: &str = "hold-codex-native-queue";
const FIRST: &str = "first-codex-native-queue";
const QUEUED: &str = "after-completion-codex-native-queue";
const AGAIN: &str = "after-restart-of-queue";
const LIMIT: Duration = Duration::from_secs(30);

#[tokio::test]
#[ignore = "requires installed Codex app-server with thread/queue/add and a loopback model"]
async fn native_queue_receipt_idle_interruption_and_restart_of_admission() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    std::fs::create_dir(&home).unwrap();
    let endpoint = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = endpoint.local_addr().unwrap();
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let held = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let server = tokio::spawn({
        let received = received.clone();
        let held = held.clone();
        let release = release.clone();
        async move {
            while let Ok((stream, _)) = endpoint.accept().await {
                let received = received.clone();
                let held = held.clone();
                let release = release.clone();
                tokio::task::spawn_blocking(move || {
                    let stream = stream.into_std().unwrap();
                    stream.set_nonblocking(false).unwrap();
                    let _ =
                        super::respond_observed(stream, None, None, None, false, |_, request| {
                            received.lock().unwrap().push(request.clone());
                            let last = request["input"]
                                .as_array()
                                .and_then(|rows| rows.iter().rev().find(|m| m["role"] == "user"));
                            if last.is_some_and(|r| r.to_string().contains(HOLD)) {
                                held.store(true, Ordering::Release);
                                let deadline = Instant::now() + LIMIT;
                                while !release.load(Ordering::Acquire) && Instant::now() < deadline
                                {
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                            }
                        });
                });
            }
        }
    });
    let config = format!(
        "model = \"gpt-5.1\"\nmodel_provider = \"fixture\"\n[model_providers.fixture]\nname = \"loopback\"\nbase_url = \"http://{address}/v1\"\nwire_api = \"responses\"\nenv_key = \"KAR_FIXTURE_KEY\"\n"
    );
    std::fs::write(home.join("config.toml"), config).unwrap();
    let mut opts = SessionOptions::new(root.path());
    opts.env.insert("CODEX_HOME".into(), home.into_os_string());
    opts.env
        .insert("KAR_FIXTURE_KEY".into(), "local-fixture-key".into());
    let session = kar::runtime("codex").unwrap().open(opts).await.unwrap();
    assert!(session.capabilities().durable_native_queue);
    assert_eq!(
        session.prompt(FIRST).await.unwrap().wait().await.unwrap(),
        Outcome::Completed
    );
    let png = include_bytes!("../fixtures/vision.png");
    let path = root.path().join("vision.png");
    std::fs::write(&path, png).unwrap();
    let image_url = format!("data:image/png;base64,{}", STANDARD.encode(png));
    let image_only = session
        .prompt_image_paths("", std::slice::from_ref(&path))
        .await
        .unwrap();
    let submission = image_only
        .admission()
        .unwrap()
        .request
        .prompt
        .as_ref()
        .unwrap()
        .submission
        .as_ref()
        .unwrap();
    assert!(submission.input.is_empty());
    assert_eq!(submission.images[0].path.as_deref(), path.to_str());
    assert_eq!(image_only.wait().await.unwrap(), Outcome::Completed);
    let has_user_image = |request: &Value, marker: Option<&str>| {
        request["input"]
            .as_array()
            .and_then(|rows| rows.iter().rev().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_array())
            .is_some_and(|content| {
                let image = content.iter().any(|p| {
                    p["type"] == "input_image"
                        && p["image_url"] == image_url
                        && p["detail"] == "high"
                });
                let text = marker.is_none_or(|word| content.iter().any(|p| p["text"] == word));
                image && text && !content.iter().any(|part| part["text"] == "")
            })
    };
    assert!(
        received
            .lock()
            .unwrap()
            .iter()
            .any(|request| has_user_image(request, None))
    );
    let last = session.records().last().unwrap().seq;
    let id = Uuid::new_v4();
    let image = kar::InputImage::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let turn = session
        .queue_images_with_id(id, QUEUED, vec![image])
        .await
        .unwrap();
    let receipt = turn.admission().unwrap().clone();
    assert_eq!(
        receipt.response.native["native"]["queuedSubmission"]["clientUserMessageId"],
        id.to_string()
    );
    assert_eq!(receipt.request.native["type"], "queue");
    assert_eq!(receipt.request.native["inputId"], id.to_string());
    assert_eq!(receipt.request.native["images"], json!([{"path":path}]));
    let result = tokio::time::timeout(LIMIT, turn.report())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.outcome, Outcome::Completed);
    assert!(result.text.contains("rust-wire-ok"));
    let records = session.records();
    assert!(
        records
            .iter()
            .filter(|r| r.seq > last)
            .all(|r| !r.events.contains(&Event::PromptDispatched))
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| r.seq > last
                && r.kind == RecordKind::Request
                && r.direction == kar::Direction::ToRuntime)
            .count(),
        1
    );
    assert!(
        received
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.to_string().contains(QUEUED))
    );
    assert!(
        received
            .lock()
            .unwrap()
            .iter()
            .any(|request| has_user_image(request, Some(QUEUED)))
    );
    assert!(records.iter().all(|row| {
        !serde_json::to_string(row.as_ref())
            .unwrap()
            .contains(&STANDARD.encode(png))
    }));
    let result = session
        .await_input_end(Some(&receipt.request.cursor()), &id.to_string())
        .await
        .unwrap();
    assert!(
        matches!(result,kar::observe::ObservedInputEnd::Turn(ref end) if end.outcome == Outcome::Completed)
    );
    let hold = session.prompt(HOLD).await.unwrap();
    assert_eq!(
        hold.admission().unwrap().response.native["type"],
        "accepted"
    );
    tokio::time::timeout(LIMIT, async {
        while !held.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    session.abort().await.unwrap();
    release.store(true, Ordering::Release);
    assert_eq!(
        tokio::time::timeout(LIMIT, hold.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Aborted
    );
    let rejected = Uuid::new_v4();
    assert!(matches!(
        session.queue_with_id(rejected, "never parked").await,
        Err(kar::Error::Refused(_))
    ));
    let records = session.records();
    let request = records
        .iter()
        .rfind(|r| r.native["inputId"] == rejected.to_string() && r.native["type"] == "queue")
        .unwrap();
    let decision = session
        .control_receipt(request.control_request_id().unwrap())
        .unwrap();
    assert_eq!(decision.response.native["code"], "no_active_turn");
    assert!(
        decision.response.native["reason"]
            .as_str()
            .unwrap()
            .contains("use prompt")
    );
    assert!(
        !records
            .iter()
            .any(|r| r.native["method"] == "thread/queue/add"
                && r.native["params"]["clientUserMessageId"] == rejected.to_string())
    );
    assert_eq!(
        session
            .prompt("resume with prompt")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    let next = session.queue_with_id(Uuid::new_v4(), AGAIN).await.unwrap();
    assert_eq!(
        next.admission().unwrap().response.native["type"],
        json!("accepted")
    );
    assert_eq!(
        tokio::time::timeout(LIMIT, next.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Completed
    );
    session.close().await.unwrap();
    server.abort();
}
