//! Stop the actual Web process while Kar opens its write-ahead record. Holding
//! a shared journal lock then keeps the receipt out of Kar until the CLI dies.
use super::*;
use kar::chat::InputStatus;
#[path = "../support/native_group.rs"]
mod native_group;
use native_group::{NativeGroup, wait};
use nix::sys::signal::Signal;
use std::{fs, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};

// Only complete JSONL lines can serve as independent proof at this cut.
fn native_turn(path: &Path, request: Uuid) -> Option<(u64, Option<Value>)> {
    let rows = fs::read(path).unwrap();
    let key = request.to_string();
    let mut active = None;
    let mut claimed = None;
    for line in rows
        .split_inclusive(|&b| b == b'\n')
        .filter(|line| line.ends_with(b"\n"))
    {
        let e: Value = serde_json::from_slice(line).unwrap();
        let d = &e["data"];
        match e["type"].as_str() {
            Some("turn/start") => active = d["turn"].as_u64(),
            Some("user/message")
                if e["surfaceOp"] == "append"
                    && d["source"]["kind"] == "user"
                    && d["source"]["rpcId"] == key =>
            {
                claimed = active
            }
            Some("turn/end") if claimed.is_some() && claimed == d["turn"].as_u64() => {
                return Some((claimed.unwrap(), Some(d["reason"].clone())));
            }
            _ => {}
        }
    }
    claimed.map(|turn| (turn, None))
}

#[tokio::test]
#[ignore = "requires a real Web DSH and POSIX process groups"]
async fn kill_after_native_admission_before_durable_receipt() {
    crash_at_receipt(false).await;
}

#[tokio::test]
#[ignore = "requires a real Web DSH and POSIX process groups"]
async fn kill_after_native_claim_without_terminal_or_durable_receipt() {
    crash_at_receipt(true).await;
}

async fn crash_at_receipt(mid_turn: bool) {
    let tb = testbed(Protocol::Dsh).await;
    tb.data.hold_model_reply.send_replace(mid_turn);
    let home = tb.dir.path().join("home/.dsh");
    fs::create_dir_all(&home).unwrap();
    let profile = json!([{"id":"session-persistence-jsonl","config":{"root":home.join("sessions"),"compression":"none"}}]);
    fs::write(home.join("cordis.patch.yml"), profile.to_string()).unwrap();
    let session = tb
        .store
        .create("dsh", Some(&tb.runtime.program), tb.opts.clone())
        .await
        .unwrap();
    let id = session.info().id;
    let native_id = session.info().native_id.clone();
    let workspace = session.info().workspace.clone();
    let seed = Uuid::new_v4();
    session.send(seed, "KAR_QUEUE_CRASH seed").await.unwrap();
    session.close().await.unwrap();
    let mut opts = tb.opts.clone();
    opts.resume = Some(native_id.clone());
    let probe = tb.runtime.open(opts).await.unwrap();
    let native_file = probe.native_session_file().await.unwrap().unwrap();
    probe.close().await.unwrap();

    let dir = tb.dir.path().join("store");
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(tb.dir.path())
        .envs(&tb.opts.env)
        .env("KAR_CHAT_DIR", &dir)
        .args(["chat", "--json", "resume", "--chat", &id.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let line = tokio::time::timeout(
        Duration::from_secs(30),
        BufReader::new(controller.stdout.take().unwrap())
            .lines()
            .next_line(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let opened: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(opened["chat"]["native_id"], native_id);
    let group = NativeGroup::of(controller.id().unwrap());
    group.signal(Signal::SIGSTOP);
    let inputs = [
        (Uuid::new_v4(), "KAR_QUEUE_CRASH first"),
        (Uuid::new_v4(), "KAR_QUEUE_CRASH second"),
        (Uuid::new_v4(), "KAR_QUEUE_CRASH third"),
    ];
    for (key, text) in inputs {
        assert_eq!(
            tb.store.enqueue(id, key, text).unwrap().status,
            InputStatus::Pending
        );
    }
    let (first, _) = inputs[0];
    wait(
        || {
            tb.store
                .history(id)
                .unwrap()
                .turns
                .iter()
                .any(|t| t.id == first)
        },
        "durable first begin",
    )
    .await;
    let journal = dir.join(format!("{id}.karchat"));
    let lock = fs::File::open(&journal).unwrap();
    lock.lock_shared().unwrap();
    let frozen = fs::read(&journal).unwrap();
    let turn = tb.store.history(id).unwrap().turns.pop().unwrap();
    assert_eq!(turn.id, first);
    assert!(!turn.input_sent && turn.native_turn_id.is_none());
    group.signal(Signal::SIGCONT);
    wait(
        || {
            native_turn(&native_file, first).is_some_and(|(_, end)| end.is_none() == mid_turn)
                && tb.data.requests.lock().unwrap().len() == 2
        },
        "DSH persisted the UUID and reached the selected model boundary",
    )
    .await;
    assert_eq!(fs::read(&journal).unwrap(), frozen);
    controller.kill().await.unwrap();
    assert!(!controller.wait().await.unwrap().success());
    drop(group);
    drop(lock);
    tb.data.hold_model_reply.send_replace(false);

    let receipts = tb.store.queued_inputs(id).unwrap();
    assert_eq!(
        receipts.iter().map(|i| i.status).collect::<Vec<_>>(),
        [
            InputStatus::Uncertain,
            InputStatus::Pending,
            InputStatus::Pending
        ]
    );
    let reopened = tb
        .store
        .resume_with(id, |options| options.env = tb.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(reopened.info().native_id, native_id);
    assert_eq!(reopened.info().workspace, workspace);
    reopened.drain_queue().await.unwrap();
    for (key, text) in inputs {
        assert_eq!(
            tb.store.enqueue(id, key, text).unwrap().status,
            InputStatus::Sent
        );
        let turn = reopened.send(key, text).await.unwrap();
        assert!(turn.input_sent);
        let (number, reason) = native_turn(&native_file, key).expect("native input UUID missing");
        assert_eq!(turn.native_turn_id, Some(number.to_string()));
        if key == first && mid_turn {
            let reason = reason.expect("native recovery must explain the interrupted input");
            assert_eq!(turn.status, TurnStatus::Failed, "{reason}");
            let failure = turn
                .failure
                .as_ref()
                .expect("missing native recovery reason");
            assert_eq!(reason["kind"], "interrupted");
            assert_eq!(failure.class, kar::FailureClass::RuntimeExited);
            let detail = reason
                .pointer("/error/message")
                .or_else(|| reason.get("kind"))
                .and_then(Value::as_str)
                .unwrap();
            assert_eq!(failure.reason, detail);
            println!(
                "DSH restart reported native turn {number} with reason {}",
                reason["kind"]
            );
        } else {
            assert_eq!(reason.unwrap()["kind"], "completed");
            assert_eq!(turn.status, TurnStatus::Completed);
        }
    }
    reopened.drain_queue().await.unwrap();
    reopened.close().await.unwrap();
    let rows = fs::read_to_string(native_file).unwrap();
    let claims: Vec<_> = rows
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|e| {
            e["type"] == "user/message"
                && e["surfaceOp"] == "append"
                && e["data"]["source"]["kind"] == "user"
        })
        .map(|e| e["data"]["source"]["rpcId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        claims,
        [seed, inputs[0].0, inputs[1].0, inputs[2].0].map(|id| id.to_string())
    );
    let inputs_seen = tb
        .data
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let last = r["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .unwrap();
            let content = &last["content"];
            // DSH may coalesce consecutive user blocks when the interrupted
            // input has no assistant reply. The newest caller block starts this request.
            content
                .as_str()
                .into_iter()
                .chain(
                    content
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|p| p["text"].as_str()),
                )
                .rfind(|text| text.starts_with("KAR_QUEUE_CRASH "))
                .expect("missing caller marker")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        inputs_seen,
        [
            "KAR_QUEUE_CRASH seed",
            inputs[0].1,
            inputs[1].1,
            inputs[2].1
        ]
    );
    println!(
        "DSH persisted the first UUID before Kar recorded receipt; replayed 0 model requests, two pending UUIDs delivered once in order"
    );
}
