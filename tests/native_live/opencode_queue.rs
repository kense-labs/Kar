//! Real ACP admission, native history, and two independent HTTP replicas across a controller kill.
use super::*;
use crate::{chat_replay, sse::SseClient};
use kar::chat::{ChatReplica, InputStatus, Item, Role};
use std::{fs, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;
#[path = "../support/native_group.rs"]
mod native_group;
use native_group::{NativeGroup, wait};
use nix::sys::signal::Signal;

pub(super) async fn archive(program: &Path, opts: &SessionOptions, root: &str, v2: bool) -> Value {
    let route = format!("/api/session/{root}/message?order=asc&limit=32");
    let mut command = tokio::process::Command::new(program);
    command
        .current_dir(&opts.cwd)
        .envs(&opts.env)
        .kill_on_drop(true);
    if v2 {
        command.args(["api", "--standalone", "GET", &route]);
    } else {
        command.args(["export", root]);
    }
    // The v1 exporter can exit with bytes still buffered on a pipe. A
    // regular file captures its full native JSON even for a large prompt.
    let dump = tempfile::NamedTempFile::new_in(opts.cwd.parent().unwrap()).unwrap();
    let child = command
        .stdout(dump.reopen().unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let o = tokio::time::timeout(Duration::from_secs(25), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_reader(std::io::BufReader::new(dump.reopen().unwrap())).unwrap()
}

pub(super) fn version(program: &Path) -> (String, bool) {
    let out = std::process::Command::new(program)
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let version = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .trim_start_matches("opencode ")
        .trim_start_matches('v')
        .to_owned();
    let major = semver::Version::parse(&version).unwrap().major;
    assert!([1, 2].contains(&major));
    (version, major == 2)
}

async fn native_users(
    program: &Path,
    opts: &SessionOptions,
    root: &str,
    v2: bool,
) -> Vec<(String, String)> {
    let doc = archive(program, opts, root, v2).await;
    if v2 {
        return doc["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["type"] == "user")
            .map(|r| {
                (
                    r["id"].as_str().unwrap().into(),
                    r["text"].as_str().unwrap().into(),
                )
            })
            .collect();
    }
    assert_eq!(doc["info"]["id"], root);
    doc["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["info"]["role"] == "user")
        .map(|r| {
            let text = r["parts"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<String>();
            (r["info"]["id"].as_str().unwrap().to_owned(), text)
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires OpenCode 1 or 2 ACP and POSIX process groups"]
async fn two_clients_and_fifo_after_native_receive_before_host_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (mut opts, calls) = fixture(&base);
    opts.model = Some("fixture/fixture".into());
    fs::write(
        opts.cwd.join("queue-read.txt"),
        "kar-open-queue-tool-result",
    )
    .unwrap();
    let program = std::path::PathBuf::from(std::env::var_os("KAR_OPENCODE_BIN").unwrap());
    let (version, v2) = version(&program);
    let dir = base.join("chats");
    let store = ChatStore::open(&dir).unwrap();
    let chat = store
        .create("opencode", Some(&program), opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let root = chat.info().native_id.clone();
    let work = chat.info().workspace.clone();
    let mut a = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let from = a.cursor();
    let (url, http) = chat_replay::serve(&store, id).await;
    let mut left = SseClient::open(&url, &from.to_string(), None).await;
    let mut right = SseClient::open(&url, &from.to_string(), None).await;
    let (mut asaw, mut bsaw) = (vec![], vec![]);
    let seed = chat
        .send(Uuid::new_v4(), "Kar OpenCode queue seed")
        .await
        .unwrap();
    assert_eq!(seed.status, TurnStatus::Completed);
    chat_replay::receive(
        &mut left,
        &mut a,
        &mut asaw,
        store.feed_snapshot(id).unwrap().cursor,
    )
    .await;
    drop(left);
    let tool = chat
        .send(Uuid::new_v4(), "KAR-QUEUE-READ-LOCAL")
        .await
        .unwrap();
    assert_eq!(tool.status, TurnStatus::Completed);
    assert!(tool.items.iter().any(|i|matches!(&i.item, Item::Tool{activity,..} if activity.result.as_ref().and_then(|r|r.text()).is_some_and(|s|s.contains("kar-open-queue-tool-result")))),"did not see the native file read: {:?}",tool.items);
    chat.close().await.unwrap();
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(&base)
        .envs(&opts.env)
        .env("KAR_CHAT_DIR", &dir)
        .env("KAR_OPENCODE_BIN", &program)
        .args(["chat", "--json", "resume", "--chat", &id.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let welcome = tokio::time::timeout(
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
        serde_json::from_str::<Value>(&welcome).unwrap()["chat"]["native_id"],
        root
    );
    let group = NativeGroup::of(controller.id().unwrap());
    group.signal(Signal::SIGSTOP);
    // A blocked pipe prevents the actor from recording the successful write
    // until after the shared journal lock is held.
    let large = format!("kar-queue-held-at-model {}", "x".repeat(256 * 1024));
    let inputs = [
        (Uuid::new_v4(), large.as_str()),
        (Uuid::new_v4(), "kar-oc-queue-next"),
        (Uuid::new_v4(), "kar-oc-queue-last"),
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
        "durable queue head",
    )
    .await;
    let journal = dir.join(format!("{id}.karchat"));
    let lock = fs::File::open(&journal).unwrap();
    lock.lock_shared().unwrap();
    let cut = fs::read(&journal).unwrap();
    let begun = store.feed_snapshot(id).unwrap();
    let t = begun.history.turns.last().unwrap();
    assert_eq!(t.id, first);
    assert!(!t.input_sent && t.native_turn_id.is_none());
    chat_replay::receive(&mut right, &mut b, &mut bsaw, begun.cursor).await;
    drop(right);
    http.abort();
    group.signal(Signal::SIGCONT);
    wait(
        || work.join("requested").is_file(),
        "real ACP prompt at the loopback model",
    )
    .await;
    let before = native_users(&program, &opts, &root, v2).await;
    let matched = before
        .iter()
        .filter(|(_, text)| text == &large)
        .collect::<Vec<_>>();
    assert_eq!(matched.len(), 1, "expected one exact persisted caller text");
    let native_id = &matched[0].0;
    assert_ne!(native_id, &first.to_string());
    assert!(
        fs::read(&journal).unwrap() == cut,
        "host journal crossed the protected receipt boundary"
    );
    controller.kill().await.unwrap();
    assert!(!controller.wait().await.unwrap().success());
    drop(group);
    drop(lock);
    fs::write(work.join("release"), "").unwrap();
    let reopened = ChatStore::open(&dir).unwrap();
    let statuses = || {
        reopened
            .queued_inputs(id)
            .unwrap()
            .iter()
            .map(|q| q.status)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        statuses(),
        [
            InputStatus::Uncertain,
            InputStatus::Pending,
            InputStatus::Pending
        ]
    );
    let resumed = reopened
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, root);
    assert_eq!(resumed.info().workspace, work);
    resumed.drain_queue().await.unwrap();
    for (key, text) in inputs {
        let result = resumed.send(key, text).await.unwrap();
        // With no locally observed native ID, equal user text cannot establish which Kar UUID submitted it.
        let (expected_status, expected_input) = if key == first {
            (TurnStatus::Interrupted, InputStatus::Uncertain)
        } else {
            (TurnStatus::Completed, InputStatus::Sent)
        };
        assert_eq!(result.status, expected_status);
        assert_eq!(
            reopened.enqueue(id, key, text).unwrap().status,
            expected_input
        );
    }
    assert!(
        resumed
            .send(first, "same ID with different text")
            .await
            .is_err()
    );
    resumed.drain_queue().await.unwrap();
    resumed.close().await.unwrap();
    let tip = reopened.feed_snapshot(id).unwrap();
    assert_eq!(
        tip.history.turns.iter().map(|t| t.id).collect::<Vec<_>>(),
        [seed.id, tool.id, first, inputs[1].0, inputs[2].0]
    );
    assert_eq!(tip.history.turns[0], seed);
    assert_eq!(tip.history.turns[1], tool);
    for (i, (_, text)) in inputs.iter().enumerate() {
        assert_eq!(
            tip.history.turns[2 + i]
                .items
                .iter()
                .filter(|i| matches!(&i.item,Item::Message{role:Role::User,text:t,..} if t==text))
                .count(),
            1
        );
    }
    let mut canonical = vec![];
    let mut feed = reopened.follow(id, Some(from)).unwrap();
    while feed.cursor() != tip.cursor {
        canonical.push(serde_json::to_value(feed.recv().await.unwrap()).unwrap());
    }
    let (url, http) = chat_replay::serve(&reopened, id).await;
    assert_ne!(a.cursor(), b.cursor());
    for (replica, saw) in [(&mut a, &mut asaw), (&mut b, &mut bsaw)] {
        let mut sub =
            SseClient::open(&url, &from.to_string(), Some(&replica.cursor().to_string())).await;
        chat_replay::receive(&mut sub, replica, saw, tip.cursor).await;
        assert!(saw == &canonical);
        assert!(replica.turns() == tip.history.turns);
    }
    http.abort();
    let after = native_users(&program, &opts, &root, v2).await;
    let delivered = after
        .iter()
        .filter_map(|(key, text)| {
            inputs
                .iter()
                .position(|(_, s)| s == text)
                .map(|index| (index, key))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        delivered.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(delivered[0].1, native_id);
    let calls = calls.lock().unwrap();
    let dispatched = calls
        .iter()
        .filter_map(|msgs| {
            let last = msgs.as_array()?.last()?;
            if last["role"] != "user" {
                return None;
            }
            let contents = &last["content"];
            // OpenCode 1 groups consecutive user turns into one Anthropic
            // user message when the interrupted turn has no assistant output.
            let latest = contents.as_str().or_else(|| {
                contents
                    .as_array()?
                    .iter()
                    .rev()
                    .find_map(|part| part["text"].as_str())
            })?;
            inputs.iter().position(|(_, text)| text == &latest)
        })
        .collect::<Vec<_>>();
    assert_eq!(dispatched, [0, 1, 2]);
    println!(
        "OpenCode {}: native IDs independently confirmed one user row per input; model calls in queue order {dispatched:?}; both clients matched {} durable events. First receipt remains uncertain.",
        version,
        canonical.len()
    );
}
