//! Conformance against the pinned upstream runtime’s KAP protocol fixture.
#![cfg(unix)]
use kar::{Event, Outcome, SessionOptions};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

fn peer(path: &Path) -> std::path::PathBuf {
    let source = std::env::var_os("KAR_UPSTREAM_SOURCE")
        .expect("checkout containing the upstream runtime acceptance baseline");
    for file in ["fake-kimi-kap.mjs", "fake-kimi-kap-ws.mjs"] {
        let contents = std::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .arg("show")
            .arg(format!(
                "2b15859a13dcc50e8b66cbaee5a007832aa00e08:tests/fixtures/{file}"
            ))
            .output()
            .unwrap();
        assert!(
            contents.status.success(),
            "missing fixed upstream fixture {file}"
        );
        fs::write(path.join(file), contents.stdout).unwrap();
    }
    let bin = path.join("kimi");
    fs::write(
        &bin,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.7.2-botiverse.1'; exit 0; fi
exec node "$(dirname "$0")/fake-kimi-kap.mjs" "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

fn isolated(work: &Path) -> SessionOptions {
    let mut opts = SessionOptions::new(work);
    for key in [
        "HOME",
        "USERPROFILE",
        "KIMI_CODE_HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
    ] {
        opts.env.insert(key.into(), work.as_os_str().into());
    }
    opts
}
async fn wait_for_start(events: &mut kar::Subscription) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !events
            .next()
            .await
            .unwrap()
            .events
            .contains(&Event::TurnStarted)
        {}
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires Node and KAR_UPSTREAM_SOURCE with the fixed v0.130.1 protocol peer"]
async fn aborted_or_absent_followup_releases_held_input() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = kar::runtime("kimi").unwrap();
    rt.program = peer(dir.path());
    for (scenario, steer, parent_outcome) in [
        (
            "gap_cancel_starts_steer",
            "HOLD_STEER_GAP",
            Outcome::Completed,
        ),
        ("cancel_false", "HOLD_STEER_GAP", Outcome::Completed),
        ("normal", "SERVER_INTERRUPT_PARENT", Outcome::Aborted),
    ] {
        let work = dir.path().join(scenario);
        fs::create_dir(&work).unwrap();
        let mut opts = isolated(&work);
        let log = work.join("received.jsonl");
        opts.env
            .insert("FAKE_KIMI_LOG".into(), log.as_os_str().into());
        opts.env
            .insert("FAKE_KIMI_SCENARIO".into(), scenario.into());
        let session = rt.open(opts).await.unwrap();
        assert_eq!(session.protocol(), kar::Protocol::KimiKap);
        let mut events = session.subscribe(None).unwrap();
        let parent = session.prompt("HOLD_FOR_CONTROLS").await.unwrap();
        wait_for_start(&mut events).await;
        let (queued_id, queued) = session.queue("DRAIN_AFTER_GAP_ABORT").await.unwrap();
        session.steer(steer).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), parent.wait())
                .await
                .unwrap()
                .unwrap(),
            parent_outcome
        );
        if parent_outcome == Outcome::Completed {
            let cancel = session.abort().await;
            if scenario == "cancel_false" {
                assert!(
                    matches!(cancel,Err(kar::Error::Refused(reason)) if reason=="no active turn")
                );
            } else {
                cancel.unwrap();
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(5), queued.report())
            .await
            .expect(scenario)
            .unwrap();
        assert_eq!(result.outcome, Outcome::Completed);
        assert_eq!(result.text, "reply:DRAIN_AFTER_GAP_ABORT");
        assert!(
            matches!(session.abort().await,Err(kar::Error::Refused(reason)) if reason=="no active turn")
        );
        assert!(matches!(
            session.steer("too late").await,
            Err(kar::Error::Refused(_))
        ));
        let log = fs::read_to_string(log).unwrap();
        let requests: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let prompts: Vec<_> = requests
            .iter()
            .filter(|v| v["url"].as_str().is_some_and(|p| p.ends_with("/prompts")))
            .collect();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1]["body"]["prompt_id"], queued_id.to_string());
        assert_eq!(
            requests
                .iter()
                .filter(|v| v["url"].as_str().is_some_and(|p| p.ends_with(":cancel")))
                .count(),
            usize::from(parent_outcome == Outcome::Completed)
        );
        session.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires Node and KAR_UPSTREAM_SOURCE with the fixed v0.130.1 protocol peer"]
async fn failed_web_spawn_releases_the_profile_and_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = kar::runtime("kimi").unwrap();
    rt.program = peer(dir.path());
    let launcher = fs::read(&rt.program).unwrap();
    let mut opts = SessionOptions::new(dir.path());
    for key in ["HOME", "USERPROFILE", "KIMI_CODE_HOME"] {
        opts.env.insert(key.into(), dir.path().as_os_str().into());
    }
    opts.handshake_timeout = Duration::from_secs(30);
    // The version probe succeeds. The following OS spawn fails before it can own pipes.
    fs::write(
        &rt.program,
        "#!/bin/sh\nchmod 600 \"$0\"\necho '2.7.2-botiverse.1'\n",
    )
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(4), rt.open(opts.clone()))
        .await
        .unwrap();
    assert!(
        matches!(result,Err(kar::Error::Io(e)) if e.kind()==std::io::ErrorKind::PermissionDenied)
    );
    // The process can also exit successfully without publishing a KAP endpoint.
    fs::write(
        &rt.program,
        "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo '2.7.2-botiverse.1'; fi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&rt.program, fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(4), rt.open(opts.clone()))
        .await
        .unwrap();
    assert!(matches!(result,Err(kar::Error::Closed(s)) if s.contains("before announcing")));
    fs::write(&rt.program, launcher).unwrap();
    let session = tokio::time::timeout(Duration::from_secs(4), rt.open(opts))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.protocol(), kar::Protocol::KimiKap);
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires Node and KAR_UPSTREAM_SOURCE with the fixed v0.130.1 protocol peer"]
async fn accepted_cancel_without_native_end_is_bounded_across_retries() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = kar::runtime("kimi").unwrap();
    rt.program = peer(dir.path());
    let mut opts = isolated(dir.path());
    opts.env
        .insert("FAKE_KIMI_SCENARIO".into(), "abort_hang".into());
    let session = rt.open(opts).await.unwrap();
    let mut events = session.subscribe(None).unwrap();
    let parent = session.prompt("HOLD_FOR_CONTROLS").await.unwrap();
    wait_for_start(&mut events).await;
    session.abort().await.unwrap();
    let report = parent.report();
    tokio::pin!(report);
    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        let mut tick = tokio::time::interval(Duration::from_millis(600));
        loop {
            tokio::select! {
                result=&mut report=>break result,
                _=tick.tick()=>{ let _=session.abort().await; }
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(outcome,Err(kar::Error::Closed(s)) if s.contains("cancellation exceeded its deadline"))
    );
    let mut requested = std::collections::HashSet::new();
    let mut replied = std::collections::HashSet::new();
    let mut exited = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(r) = events.next().await {
            if r.native["method"] == "POST /sessions/{id}:cancel" {
                requested.insert(r.native["id"].as_str().unwrap().to_owned());
            }
            if r.native.pointer("/result/cancelled") == Some(&true.into()) {
                replied.insert(r.native["id"].as_str().unwrap().to_owned());
            }
            assert!(
                !r.events
                    .iter()
                    .any(|e| matches!(e, Event::TurnEnded { .. }))
            );
            exited |= r.events.iter().any(|e| matches!(e, Event::Exited { .. }));
        }
    })
    .await
    .unwrap();
    assert!(exited && requested.len() > 1);
    assert_eq!(requested, replied);
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires Node and KAR_UPSTREAM_SOURCE with the fixed v0.130.1 protocol peer"]
async fn late_cancel_response_stays_with_the_cancelled_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = kar::runtime("kimi").unwrap();
    rt.program = peer(dir.path());
    let mut opts = isolated(dir.path());
    opts.env
        .insert("FAKE_KIMI_SCENARIO".into(), "late_cancel_reply".into());
    let session = rt.open(opts).await.unwrap();
    let mut events = session.subscribe(None).unwrap();
    let cancelled = session.prompt("HOLD_FOR_CONTROLS").await.unwrap();
    wait_for_start(&mut events).await;
    session.abort().await.unwrap();
    assert_eq!(cancelled.wait().await.unwrap(), Outcome::Aborted);
    let parent = session.prompt("HOLD_FOR_CONTROLS").await.unwrap();
    wait_for_start(&mut events).await;
    let (_, queued) = session.queue("QUEUED_AFTER_STEER").await.unwrap();
    session.steer("STEER_FIRST HOLD_STEER").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while events
            .next()
            .await
            .unwrap()
            .native
            .pointer("/turn/steered_from_prompt_id")
            .and_then(|id| id.as_str())
            .is_none()
        {}
    })
    .await
    .unwrap();
    session.steer("STEER_SECOND").await.unwrap();
    assert_eq!(parent.wait().await.unwrap(), Outcome::Completed);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), queued.report())
            .await
            .unwrap()
            .unwrap()
            .text,
        "reply:QUEUED_AFTER_STEER"
    );
    session.close().await.unwrap();
}
