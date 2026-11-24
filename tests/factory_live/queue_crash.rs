//! Freeze the real controller's journal after Begin and before the stdio write
//! finishes. The native process can then accept the command and run independently.
use super::*;
#[path = "../support/chat_replay.rs"]
mod chat_replay;
#[path = "../support/native_group.rs"]
mod native_group;
use crate::sse::SseClient;
use kar::chat::InputStatus;
use native_group::{NativeGroup, wait};
use nix::sys::signal::Signal;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

fn native_rows(path: &Path) -> Vec<Value> {
    fs::read(path)
        .unwrap()
        .split_inclusive(|&b| b == b'\n')
        .filter(|line| line.ends_with(b"\n"))
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[tokio::test]
#[ignore = "requires Factory Droid and POSIX process groups"]
async fn kill_after_admission_before_host_receipt() {
    crash_at_receipt(false).await;
}

#[tokio::test]
#[ignore = "requires Factory Droid and POSIX process groups"]
async fn kill_after_outcome_before_host_receipt() {
    crash_at_receipt(true).await;
}

async fn crash_at_receipt(ended: bool) {
    let f = fixture().await;
    let dir = f._isolated.path().join("chats");
    let chat = f
        .store
        .create("factory", Some(&f.runtime.program), f.opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let root = chat.info().native_id.clone();
    let workspace = chat.info().workspace.clone();
    let mut a = ChatReplica::from_snapshot(f.store.feed_snapshot(id).unwrap());
    let mut b = ChatReplica::from_snapshot(f.store.feed_snapshot(id).unwrap());
    let initial = a.cursor();
    let (url, http) = chat_replay::serve(&f.store, id).await;
    let mut left = SseClient::open(&url, &initial.to_string(), None).await;
    let mut right = SseClient::open(&url, &initial.to_string(), None).await;
    let (mut seen_a, mut seen_b) = (vec![], vec![]);
    let seed = Uuid::new_v4();
    assert_eq!(
        chat.send(seed, "KAR_RECEIPT_SEED").await.unwrap().status,
        TurnStatus::Completed
    );
    let seeded = f.store.feed_snapshot(id).unwrap().cursor;
    chat_replay::receive(&mut left, &mut a, &mut seen_a, seeded).await;
    drop(left);
    chat.close().await.unwrap();
    let mut opts = f.opts.clone();
    opts.resume = Some(root.clone());
    let probe = f.runtime.open(opts).await.unwrap();
    let transcript = probe.native_session_file().await.unwrap().unwrap();
    probe.close().await.unwrap();
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(f._isolated.path())
        .envs(&f.opts.env)
        .env("KAR_CHAT_DIR", &dir)
        .args(["chat", "--json", "resume", "--chat", &id.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(
        Duration::from_secs(30),
        BufReader::new(controller.stdout.take().unwrap())
            .lines()
            .next_line(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["chat"]["native_id"],
        root
    );
    let group = NativeGroup::of(controller.id().unwrap());
    group.signal(Signal::SIGSTOP);
    // Larger than the pipe buffer while the child is stopped.
    let large = format!("KAR_RECEIPT_FIRST {}", "x".repeat(256 * 1024));
    let inputs = [
        (Uuid::new_v4(), large.as_str()),
        (Uuid::new_v4(), "KAR_RECEIPT_SECOND"),
        (Uuid::new_v4(), "KAR_RECEIPT_THIRD"),
    ];
    for (key, text) in inputs {
        assert_eq!(
            f.store.enqueue(id, key, text).unwrap().status,
            InputStatus::Pending
        );
    }
    let first = inputs[0].0;
    wait(
        || {
            f.store
                .history(id)
                .unwrap()
                .turns
                .last()
                .is_some_and(|t| t.id == first)
        },
        "first durable begin",
    )
    .await;
    let file = dir.join(format!("{id}.karchat"));
    let lock = fs::File::open(&file).unwrap();
    lock.lock_shared().unwrap();
    let cut = fs::read(&file).unwrap();
    let store = f.store.clone();
    let begun = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::task::spawn_blocking(move || store.feed_snapshot(id)),
    )
    .await
    .expect("live snapshot blocked behind a shared journal lock")
    .unwrap()
    .unwrap();
    let turn = begun.history.turns.last().unwrap();
    assert_eq!(turn.id, first);
    assert!(!turn.input_sent && turn.native_turn_id.is_none());
    chat_replay::receive(&mut right, &mut b, &mut seen_b, begun.cursor).await;
    drop(right);
    http.abort();
    group.signal(Signal::SIGCONT);
    tokio::time::timeout(Duration::from_secs(20), f.model.held.notified())
        .await
        .expect("Droid did not reach the model");
    if ended {
        f.model.release.send_replace(true);
    }
    let key = first.to_string();
    wait(
        || {
            let rows = native_rows(&transcript);
            rows.iter()
                .any(|r| r["type"] == "message" && r["message"]["role"] == "user" && r["id"] == key)
                && rows
                    .iter()
                    .any(|r| r["type"] == "agent_turn_outcome" && r["turnId"] == key)
                    == ended
        },
        "Droid persisted the matching input at the requested model boundary",
    )
    .await;
    assert!(
        fs::read(&file).unwrap() == cut,
        "host receipt escaped the lock"
    );
    controller.kill().await.unwrap();
    assert!(!controller.wait().await.unwrap().success());
    drop(group);
    drop(lock);
    f.model.release.send_replace(true);
    let reopened = ChatStore::open(&dir).unwrap();
    assert_eq!(
        reopened
            .queued_inputs(id)
            .unwrap()
            .iter()
            .map(|q| q.status)
            .collect::<Vec<_>>(),
        [
            InputStatus::Uncertain,
            InputStatus::Pending,
            InputStatus::Pending
        ]
    );
    let resumed = reopened
        .resume_with(id, |o| o.env = f.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().workspace, workspace);
    assert_eq!(resumed.info().native_id, root);
    resumed.drain_queue().await.unwrap();
    for (key, text) in inputs {
        let t = resumed.send(key, text).await.unwrap();
        assert!(
            t.input_sent,
            "native admission missing for {key} (terminal before crash: {ended})"
        );
        assert_eq!(t.native_turn_id, Some(key.to_string()));
        assert_eq!(
            reopened.enqueue(id, key, text).unwrap().status,
            InputStatus::Sent
        );
        assert_eq!(
            t.status,
            if key == first && !ended {
                TurnStatus::Interrupted
            } else {
                TurnStatus::Completed
            }
        );
    }
    resumed.drain_queue().await.unwrap();
    resumed.close().await.unwrap();
    let tip = reopened.feed_snapshot(id).unwrap();
    assert_eq!(
        tip.history.turns.iter().map(|t| t.id).collect::<Vec<_>>(),
        [seed, first, inputs[1].0, inputs[2].0]
    );
    let mut canonical = vec![];
    let mut stream = reopened.follow(id, Some(initial)).unwrap();
    while stream.cursor() != tip.cursor {
        canonical.push(serde_json::to_value(stream.recv().await.unwrap()).unwrap());
    }
    let (url, http) = chat_replay::serve(&reopened, id).await;
    assert_ne!(a.cursor(), b.cursor());
    for (replica, seen) in [(&mut a, &mut seen_a), (&mut b, &mut seen_b)] {
        let mut feed = SseClient::open(
            &url,
            &initial.to_string(),
            Some(&replica.cursor().to_string()),
        )
        .await;
        chat_replay::receive(&mut feed, replica, seen, tip.cursor).await;
        assert_eq!(seen, &canonical);
        assert_eq!(replica.turns(), tip.history.turns);
    }
    http.abort();
    let calls = f.model.requests.lock().unwrap();
    let markers = calls
        .iter()
        .map(|body| {
            let user = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .unwrap()["content"]
                .to_string();
            ["SEED", "FIRST", "SECOND", "THIRD"]
                .into_iter()
                .find(|k| user.contains(&format!("KAR_RECEIPT_{k}")))
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(markers, ["SEED", "FIRST", "SECOND", "THIRD"]);
    let native = native_rows(&transcript);
    for (key, _) in inputs {
        assert_eq!(
            native
                .iter()
                .filter(|r| r["type"] == "message" && r["id"] == key.to_string())
                .count(),
            1
        );
    }
    println!(
        "Factory terminal before kill={ended}; model saw {markers:?}; both clients matched {} durable events",
        canonical.len()
    );
}
