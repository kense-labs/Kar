//! Real Qwen stream-json and JSONL, under a test-only home. Keep its exact SDK
//! input UUID distinct from the native record UUID and the native prompt counter.
use super::*;
#[path = "../support/chat_replay.rs"]
mod chat_replay;
#[path = "../support/native_group.rs"]
mod native_group;
use crate::sse::SseClient;
use kar::chat::{ChatReplica, InputStatus, TurnStatus};
use native_group::{NativeGroup, wait};
use nix::sys::signal::Signal;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

fn journal(f: &Fixture, sid: &str) -> PathBuf {
    let root = f.dir.path().join("home/.qwen/projects");
    let files: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path().join("chats").join(format!("{sid}.jsonl")))
        .filter(|p| p.is_file())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "expected one Qwen session file in the isolated home"
    );
    files.into_iter().next().unwrap()
}

fn rows(path: &Path) -> Vec<Value> {
    fs::read(path)
        .unwrap()
        .split_inclusive(|&c| c == b'\n')
        .filter(|l| l.ends_with(b"\n"))
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect()
}

fn user_prompts(path: &Path) -> Vec<(String, String, String)> {
    rows(path)
        .into_iter()
        .filter(|r| r["type"] == "user" && r["provenance"] == "real_user")
        .map(|r| {
            (
                r["uuid"].as_str().unwrap().to_owned(),
                r["promptId"].as_str().unwrap().to_owned(),
                r["message"]["parts"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect()
}

fn last_marker(body: &Value) -> Option<&'static str> {
    let content = body["messages"].as_array()?.last()?;
    if content["role"] != "user" {
        return None;
    }
    let text = content["content"].to_string();
    [
        "QWEN_CUT_SEED",
        "QWEN_CUT_HELD",
        "QWEN_CUT_NEXT",
        "QWEN_CUT_LAST",
        "QWEN_NATIVE_RETRY",
    ]
    .into_iter()
    // The provider coalesces successive user messages when the first input
    // has no assistant reply. Only the newest marker submitted this request.
    .filter_map(|m| text.rfind(m).map(|at| (at, m)))
    .max_by_key(|(at, _)| *at)
    .map(|(_, m)| m)
}

#[tokio::test]
#[ignore = "requires real Qwen Code; SDK UUID and native deduplication across resume"]
async fn native_retry_of_same_sdk_uuid() {
    let f = fixture().await;
    let rt = runtime("qwen").unwrap();
    let id = Uuid::new_v4();
    let prompt = "QWEN_NATIVE_RETRY";
    let mut opts = f.opts.clone();
    let mut sid = None;
    for _ in 0..2 {
        opts.resume = sid.clone();
        let s = rt.open(opts.clone()).await.unwrap();
        if let Some(root) = &sid {
            assert_eq!(s.id(), root);
        }
        sid = Some(s.id().to_owned());
        let turn = s.prompt_with_id(id, prompt).await.unwrap();
        assert_eq!(turn.wait().await.unwrap(), Outcome::Completed);
        let sent: Vec<_> = s
            .records()
            .into_iter()
            .filter(|r| r.direction == kar::Direction::ToRuntime && r.native["type"] == "user")
            .collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].native["uuid"],
            id.to_string(),
            "Qwen SDK wire must receive the caller's UUID"
        );
        s.close().await.unwrap();
    }
    let root = sid.unwrap();
    let path = journal(&f, &root);
    assert!(!fs::read_to_string(&path).unwrap().contains(&id.to_string()));
    let native = user_prompts(&path);
    assert_eq!(native.len(), 2);
    assert!(native.iter().all(|(_, _, text)| text == prompt));
    assert_ne!(native[0].0, native[1].0);
    assert_eq!(
        [&native[0].1, &native[1].1],
        [&format!("{root}########1"), &format!("{root}########2")]
    );
    let calls = f
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(last_marker)
        .collect::<Vec<_>>();
    assert_eq!(calls, [prompt, prompt]);
    println!(
        "Two SDK writes with one UUID across native resume produced two distinct native prompts and two model calls; disk contained no caller UUID."
    );
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    BeforeReceipt,
    AfterReceipt,
    NativeEnded,
}

#[tokio::test]
#[ignore = "requires Qwen Code, POSIX process groups and a local HTTP model"]
async fn kill_after_native_input_before_host_receipt() {
    crash(Cut::BeforeReceipt).await;
}

#[tokio::test]
#[ignore = "requires Qwen Code and POSIX process groups"]
async fn kill_after_host_receipt() {
    crash(Cut::AfterReceipt).await;
}

#[tokio::test]
#[ignore = "requires Qwen Code and POSIX process groups"]
async fn kill_after_native_completion_before_host_receipt() {
    crash(Cut::NativeEnded).await;
}

async fn crash(cut: Cut) {
    let f = fixture().await;
    let dir = f.dir.path().join("chats");
    let store = ChatStore::open(&dir).unwrap();
    let rt = runtime("qwen").unwrap();
    let chat = store
        .create("qwen", Some(&rt.program), f.opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let root = chat.info().native_id.clone();
    let workspace = chat.info().workspace.clone();
    let seed = Uuid::new_v4();
    let mut a = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let initial = a.cursor();
    let (url, http) = chat_replay::serve(&store, id).await;
    let mut left = SseClient::open(&url, &initial.to_string(), None).await;
    let mut right = SseClient::open(&url, &initial.to_string(), None).await;
    let (mut seen_a, mut seen_b) = (vec![], vec![]);
    assert_eq!(
        chat.send(seed, "QWEN_CUT_SEED").await.unwrap().status,
        TurnStatus::Completed
    );
    let seeded = store.feed_snapshot(id).unwrap().cursor;
    chat_replay::receive(&mut left, &mut a, &mut seen_a, seeded).await;
    drop(left);
    chat.close().await.unwrap();
    let file = journal(&f, &root);
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(f.dir.path())
        .envs(&f.opts.env)
        .env("KAR_CHAT_DIR", &dir)
        .args(["chat", "--json", "resume", "--chat", &id.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(
        Duration::from_secs(25),
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
    // Stop the writer between durable Begin and the successful stdio write.
    let held = format!("QWEN_CUT_HELD {}", "x".repeat(256 * 1024));
    let inputs = [
        (Uuid::new_v4(), held.as_str()),
        (Uuid::new_v4(), "QWEN_CUT_NEXT"),
        (Uuid::new_v4(), "QWEN_CUT_LAST"),
    ];
    for (key, text) in inputs {
        assert_eq!(
            store.enqueue(id, key, text).unwrap().status,
            InputStatus::Pending
        );
    }
    let first = inputs[0].0;
    wait(
        || {
            store
                .history(id)
                .unwrap()
                .turns
                .last()
                .is_some_and(|t| t.id == first)
        },
        "durable input begin",
    )
    .await;
    let log = dir.join(format!("{id}.karchat"));
    let lock = fs::File::open(&log).unwrap();
    lock.lock_shared().unwrap();
    let before = fs::metadata(&log).unwrap().len();
    let begun = store.feed_snapshot(id).unwrap();
    let t = begun.history.turns.last().unwrap();
    assert_eq!(t.id, first);
    assert!(!t.input_sent && t.native_turn_id.is_none());
    chat_replay::receive(&mut right, &mut b, &mut seen_b, begun.cursor).await;
    drop(right);
    http.abort();
    group.signal(Signal::SIGCONT);
    wait(
        || {
            f.requests
                .lock()
                .unwrap()
                .iter()
                .any(|call| last_marker(call) == Some("QWEN_CUT_HELD"))
                && user_prompts(&file).iter().any(|(_, _, s)| s == &held)
        },
        "Qwen model and transcript received the input",
    )
    .await;
    if cut == Cut::NativeEnded {
        f.release.send_replace(true);
        wait(
            || {
                let all = rows(&file);
                all.iter()
                    .filter(|r| {
                        r["type"] == "assistant"
                            && r["message"]["parts"][0]["text"] == "qwen-rust-reply"
                    })
                    .count()
                    == 2
            },
            "durable native answer",
        )
        .await;
    }
    assert_eq!(fs::metadata(&log).unwrap().len(), before);
    let lock = if cut == Cut::AfterReceipt {
        drop(lock);
        wait(
            || store.queued_inputs(id).unwrap()[0].status == InputStatus::Sent,
            "Kar receipt",
        )
        .await;
        None
    } else {
        Some(lock)
    };
    controller.kill().await.unwrap();
    assert!(!controller.wait().await.unwrap().success());
    drop(group);
    drop(lock);
    f.release.send_replace(true);
    let reopened = ChatStore::open(&dir).unwrap();
    let first_status = if cut == Cut::AfterReceipt {
        InputStatus::Sent
    } else {
        InputStatus::Uncertain
    };
    assert_eq!(
        reopened
            .queued_inputs(id)
            .unwrap()
            .iter()
            .map(|e| e.status)
            .collect::<Vec<_>>(),
        [first_status, InputStatus::Pending, InputStatus::Pending]
    );
    let (url, http) = chat_replay::serve(&reopened, id).await;
    assert_ne!(a.cursor(), b.cursor());
    let first_cursor = initial.to_string();
    let saved_a = a.cursor().to_string();
    let saved_b = b.cursor().to_string();
    let (mut left, mut right) = tokio::join!(
        SseClient::open(&url, &first_cursor, Some(&saved_a)),
        SseClient::open(&url, &first_cursor, Some(&saved_b)),
    );
    let recovered = reopened.feed_snapshot(id).unwrap().cursor;
    tokio::join!(
        chat_replay::receive(&mut left, &mut a, &mut seen_a, recovered),
        chat_replay::receive(&mut right, &mut b, &mut seen_b, recovered),
    );
    let resumed = reopened
        .resume_with(id, |o| o.env = f.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, root);
    assert_eq!(resumed.info().workspace, workspace);
    resumed.drain_queue().await.unwrap();
    for (key, text) in inputs {
        assert_eq!(
            reopened.enqueue(id, key, text).unwrap().status,
            if key == first {
                first_status
            } else {
                InputStatus::Sent
            }
        );
        assert_eq!(
            resumed.send(key, text).await.unwrap().status,
            if key == first {
                TurnStatus::Interrupted
            } else {
                TurnStatus::Completed
            }
        );
    }
    resumed.drain_queue().await.unwrap();
    resumed.close().await.unwrap();
    let history = reopened.feed_snapshot(id).unwrap();
    let tip = history.cursor;
    assert_eq!(
        history
            .history
            .turns
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [seed, first, inputs[1].0, inputs[2].0]
    );
    let mut durable = vec![];
    let mut stream = reopened.follow(id, Some(initial)).unwrap();
    while stream.cursor() != tip {
        durable.push(serde_json::to_value(stream.recv().await.unwrap()).unwrap());
    }
    assert_ne!(recovered, tip);
    tokio::join!(
        chat_replay::receive(&mut left, &mut a, &mut seen_a, tip),
        chat_replay::receive(&mut right, &mut b, &mut seen_b, tip),
    );
    for (replica, seen) in [(&a, &seen_a), (&b, &seen_b)] {
        assert!(seen == &durable);
        assert_eq!(replica.turns(), history.history.turns);
    }
    http.abort();
    f.http.abort();
    let native = user_prompts(&file);
    let content = native
        .iter()
        .map(|(_, _, t)| t.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    let markers = [
        "QWEN_CUT_SEED",
        "QWEN_CUT_HELD",
        "QWEN_CUT_NEXT",
        "QWEN_CUT_LAST",
    ];
    assert_eq!(content, markers);
    let calls = f.requests.lock().unwrap();
    assert_eq!(
        calls.iter().filter_map(last_marker).collect::<Vec<_>>(),
        markers
    );
    assert!(
        calls.last().unwrap()["messages"]
            .to_string()
            .contains("QWEN_CUT_SEED")
    );
    println!(
        "{cut:?}: host {first_status:?}, Qwen and model each received four inputs once in order, both clients replayed {} durable events",
        durable.len()
    );
}
