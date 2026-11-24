use kar::{Outcome, Protocol, Runtime, SessionOptions};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[tokio::test]
#[ignore = "requires Qoder worker 1.1.78; uses its native response replay in an empty home"]
async fn real_worker_reopens_saved_conversation() {
    let binary = std::env::var_os("KAR_QODER_WORKER_BIN").expect("set KAR_QODER_WORKER_BIN");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cwd = tmp.path().join("project");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&cwd).unwrap();
    let settings = tmp.path().join("settings.json");
    fs::write(
        &settings,
        json!({"security":{"auth":{"useExternal":true}}}).to_string(),
    )
    .unwrap();
    let replay = tmp.path().join("replies.jsonl");
    let runtime = Runtime::new(
        "qoder",
        PathBuf::from(binary),
        &[
            "--settings",
            settings.to_str().unwrap(),
            "--fake-responses",
            replay.to_str().unwrap(),
        ],
        Protocol::Qoder,
    );
    let mut opts = isolated_options(&cwd, &home);
    let mut id = None;
    for (prompt, reply) in [
        ("worker-first-message", "worker-first-answer"),
        ("worker-after-restart", "worker-resumed-answer"),
    ] {
        let row = json!({"method":"generateContentStream","response":[{"candidates":[{"content":{"parts":[{"text":reply}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2}}]});
        fs::write(&replay, format!("{row}\n")).unwrap();
        opts.resume = id.clone();
        let session = runtime.open(opts.clone()).await.unwrap();
        if let Some(expected) = &id {
            assert_eq!(session.id(), expected);
        }
        id = Some(session.id().to_owned());
        let report = tokio::time::timeout(
            Duration::from_secs(50),
            session.prompt(prompt).await.unwrap().report(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(report.outcome, Outcome::Completed);
        assert_eq!(report.text, reply);
        session.close().await.unwrap();
    }
    let file = format!("{}.jsonl", id.unwrap());
    let path = ignore::WalkBuilder::new(home.join(".qoder/projects"))
        .hidden(false)
        .build()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_str() == Some(file.as_str()))
        .expect("Qoder saved its transcript")
        .into_path();
    let history = fs::read_to_string(path).unwrap();
    for text in [
        "worker-first-message",
        "worker-first-answer",
        "worker-after-restart",
        "worker-resumed-answer",
    ] {
        assert!(
            history.contains(text),
            "missing {text} in the resumed native history"
        );
    }
}

fn isolated_options(cwd: &Path, home: &Path) -> SessionOptions {
    let mut opts = SessionOptions::new(cwd);
    for name in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME"] {
        opts.env.insert(name.into(), home.as_os_str().into());
    }
    for name in [
        "QODER_PERSONAL_ACCESS_TOKEN",
        "QODER_SERVICE_ACCOUNT_KEY",
        "QODER_JOB_TOKEN",
        "QODER_API_KEY",
        "QODER_CONFIG_DIR",
        "QODER_CLI_HOME",
        "QODER_AGENT_SDK_DAEMON",
    ] {
        opts.env.insert(name.into(), "".into());
    }
    opts.env
        .insert("QODER_FAKE_STREAM_DELAY_MS".into(), "1".into());
    opts.handshake_timeout = Duration::from_secs(75);
    opts
}

#[tokio::test]
#[ignore = "requires Qoder daemon 1.1.83 in an empty home"]
async fn real_daemon_acknowledges_session_before_requesting_login() {
    let binary = std::env::var_os("KAR_QODER_DAEMON_BIN").expect("set KAR_QODER_DAEMON_BIN");
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    let home = tmp.path().join("home");
    fs::create_dir(&cwd).unwrap();
    fs::create_dir(&home).unwrap();
    let runtime = Runtime::new("qoder", PathBuf::from(binary), &[], Protocol::Qoder);
    // This worker control runs only after daemon version, capabilities and returned session ID were verified.
    let opened = runtime.open(isolated_options(&cwd, &home)).await;
    assert!(
        matches!(opened, Err(kar::Error::Refused(message)) if message == "Qoder refused initialize")
    );
}
