//! Each CLI invocation is a separate OS process. This binary also speaks a minimal Codex protocol as its child.
use kar::chat::{ChatStore, Item, Role, TurnStatus};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[path = "chat_restart/codex_admission.rs"]
mod codex_admission;
#[path = "support/codex_chat_peer.rs"]
mod codex_chat_peer;
use codex_chat_peer::{agent, emit};

fn cli(base: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.current_dir(base.join("elsewhere"))
        .env("KAR_CHAT_DIR", base.join("store"))
        .env("FIXTURE_NATIVE_DB", base.join("native"))
        .args(["chat", "--json"]);
    cmd
}
fn success(o: Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}
fn environment_across_controllers() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["work", "elsewhere", "native"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let secret = Uuid::new_v4().to_string();
    let controller = |audit: &str| {
        let mut cmd = cli(&base);
        cmd.env("FIXTURE_ENV_AUDIT", base.join(audit))
            .env("KAR_SESSION_MARKER", &secret)
            .env("KAR_CONTROL_MARKER", "inherited")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    };
    let create = |audit: &str, remove: bool| {
        let mut cmd = controller(audit);
        cmd.args(["new", "codex", "--cwd"])
            .arg(base.join("work"))
            .arg("--bin")
            .arg(std::env::current_exe().unwrap());
        if remove {
            cmd.args(["--unset-env", "KAR_SESSION_MARKER"]);
        }
        cmd.spawn().unwrap()
    };
    let masked = create("removed", true);
    let normal = create("inherited", false);
    let masked = success(masked.wait_with_output().unwrap());
    let normal = success(normal.wait_with_output().unwrap());
    let id = masked["chat"]["id"].as_str().unwrap();
    assert_eq!(masked["chat"]["unset_env"], json!(["KAR_SESSION_MARKER"]));
    assert!(normal["chat"].get("unset_env").is_none());
    let turn = success(
        controller("removed")
            .args(["resume", "anchor", "--chat", id])
            .output()
            .unwrap(),
    );
    let child = success(
        controller("removed")
            .args(["fork", turn["id"].as_str().unwrap(), "--chat", id])
            .output()
            .unwrap(),
    );
    let branch = child["chat"]["id"].as_str().unwrap();
    assert_eq!(child["chat"]["unset_env"], masked["chat"]["unset_env"]);
    success(
        controller("removed")
            .args(["resume", "branch input", "--chat", branch])
            .output()
            .unwrap(),
    );
    success(
        controller("inherited")
            .args([
                "resume",
                "ordinary input",
                "--chat",
                normal["chat"]["id"].as_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    for (file, present) in [("inherited", true), ("removed", false)] {
        let records = fs::read_to_string(base.join(file)).unwrap();
        assert!(
            records.contains("thread/start")
                && records.contains("thread/resume")
                && records.contains("turn/start")
        );
        if !present {
            assert!(records.contains("thread/fork"));
        }
        for row in records.lines() {
            let event: Value = serde_json::from_str(row).unwrap();
            assert_eq!(event["marker"], present, "{row}");
            assert_eq!(event["control"], "inherited");
        }
    }
    for chat in [id, branch, normal["chat"]["id"].as_str().unwrap()] {
        assert!(
            !fs::read_to_string(base.join("store").join(format!("{chat}.karchat")))
                .unwrap()
                .contains(&secret)
        );
    }
    let invalid = controller("invalid")
        .args(["new", "codex", "--unset-env", "X=value"])
        .output()
        .unwrap();
    assert!(!invalid.status.success() && !base.join("invalid").exists());
}

fn restart() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["work", "elsewhere", "native"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let id = Uuid::new_v4().to_string();
    let first = success(
        cli(&base)
            .args(["new", "codex", "same", "--request-id", &id, "--cwd"])
            .arg(base.join("work"))
            .arg("--bin")
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap(),
    );
    assert_eq!(first["id"], id);
    assert_eq!(first["items"].as_array().unwrap().len(), 3);
    assert_eq!(first["items"][1]["text"], "haha");
    assert_eq!(first["items"][2]["text"], "haha");
    let repeated = success(
        cli(&base)
            .args(["resume", "same", "--request-id", &id])
            .output()
            .unwrap(),
    );
    assert_eq!(first, repeated);
    assert!(
        !cli(&base)
            .args(["resume", "different", "--request-id", &id])
            .output()
            .unwrap()
            .status
            .success()
    );
    let second = success(cli(&base).args(["resume", "same"]).output().unwrap());
    assert_ne!(first["id"], second["id"]);
    let history = success(cli(&base).arg("history").output().unwrap());
    assert_eq!(history["turns"].as_array().unwrap().len(), 2);
    assert_eq!(
        history["chat"]["workspace"],
        base.join("work").to_str().unwrap()
    );
    let chat_id = Uuid::parse_str(history["chat"]["id"].as_str().unwrap()).unwrap();
    let store = ChatStore::open(base.join("store")).unwrap();
    let filename = format!("{chat_id}.karchat");
    let journal = fs::read_to_string(base.join("store").join(&filename)).unwrap();
    let header: Value = serde_json::from_str(journal.lines().next().unwrap()).unwrap();
    assert_eq!(header["format"], "kar-chat/1");

    let previous = tempfile::tempdir().unwrap();
    let untagged = json!({"version": 1, "chat": &header["chat"]});
    fs::write(previous.path().join(&filename), format!("{untagged}\n")).unwrap();
    assert!(
        ChatStore::open(previous.path())
            .unwrap()
            .history(chat_id)
            .is_err()
    );

    let crash_id = Uuid::new_v4().to_string();
    let mut child = cli(&base)
        .args(["resume", "crash here", "--request-id", &crash_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let h = store.history(chat_id).unwrap();
        if h.turns.last().is_some_and(|t| {
            t.status==TurnStatus::Running && t.items.iter().any(|i|
            matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="haha"))
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "no durable streamed partial");
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(
        !cli(&base)
            .args(["resume", "concurrent"])
            .output()
            .unwrap()
            .status
            .success()
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        store.history(chat_id).unwrap().turns.last().unwrap().status,
        TurnStatus::Interrupted
    );
    let path = base.join("store").join(format!("{chat_id}.karchat"));
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"incomplete\":")
        .unwrap();
    assert!(
        !cli(&base)
            .args(["resume", "crash here", "--request-id", &crash_id])
            .output()
            .unwrap()
            .status
            .success()
    );
    let partial = store.history(chat_id).unwrap().turns.pop().unwrap();
    assert_eq!(partial.status, TurnStatus::Interrupted);
    let native_turn = partial.native_turn_id.unwrap();
    fs::write(base.join("native/turns"),json!([{"id":native_turn,"status":"completed","items":[{"type":"agentMessage","id":"a","text":"native final"},{"type":"reasoning","id":"r","summary":["native reasoning"]},{"type":"commandExecution","id":"restored-tool","command":"fixture","status":"completed","exitCode":0,"aggregatedOutput":"tool result"}]}]).to_string()).unwrap();
    let recovered = success(
        cli(&base)
            .args(["resume", "crash here", "--request-id", &crash_id])
            .output()
            .unwrap(),
    );
    assert_eq!(recovered["items"].as_array().unwrap().len(), 4);
    assert_eq!(recovered["items"][2]["text"], "native reasoning");
    assert_eq!(recovered["items"][3]["id"], "restored-tool");
    assert_eq!(recovered["items"][3]["activity"]["result"]["exit_code"], 0);
    assert_eq!(recovered["items"][1]["text"], "native final");
    success(
        cli(&base)
            .args(["resume", "after restart"])
            .output()
            .unwrap(),
    );
    let chat = store.history(chat_id).unwrap();
    assert_eq!(chat.turns.len(), 4);
    assert_eq!(chat.turns[2].status, TurnStatus::Completed);
    let data = fs::read_to_string(base.join("native/calls")).unwrap();
    let calls: Vec<Value> = data
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(
        |c| c["id"] == chat.chat.native_id && c["cwd"] == chat.chat.workspace.to_str().unwrap()
    ));
    // A completed malformed row is an error, not a recoverable torn tail.
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"broken\n")
        .unwrap();
    assert!(store.history(chat_id).is_err());
    assert!(
        !cli(&base)
            .args(["resume", "must not run"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(fs::read(&path).unwrap().ends_with(b"broken\n"));
}
fn durable_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["work", "elsewhere", "native"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let run = |args: &[&str]| success(cli(&base).args(args).output().unwrap());
    let seed = success(
        cli(&base)
            .args(["new", "codex", "seed", "--cwd"])
            .arg(base.join("work"))
            .arg("--bin")
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap(),
    );
    let history = run(&["history"]);
    let chat = history["chat"]["id"].as_str().unwrap();
    let args = |text: &str, id: &str| run(&["enqueue", text, "--request-id", id, "--chat", chat]);

    // Enqueue from another process while the interactive controller is already attached and idle.
    let mut host = cli(&base)
        .args(["resume", "--chat", chat])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(host.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["chat"]["id"],
        chat
    );
    let held = Uuid::new_v4().to_string();
    assert_eq!(args("hold without reply", &held)["status"], "pending");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(base.join("native/calls"))
        .unwrap()
        .contains("hold without reply")
    {
        assert!(
            Instant::now() < deadline,
            "attached chat did not deliver external enqueue"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(run(&["queue", "--chat", chat])[0]["status"], "sending");
    let skip = Uuid::new_v4().to_string();
    args("withdraw this", &skip);
    assert_eq!(
        run(&["withdraw", &skip, "--chat", chat])["status"],
        "withdrawn"
    );
    let shared = Uuid::new_v4().to_string();
    let mut producers = vec![];
    for text in [
        "same queue text",
        "other producer",
        "same queue text",
        "same queue text",
    ] {
        let id = if text == "other producer" {
            Uuid::new_v4().to_string()
        } else {
            shared.clone()
        };
        producers.push(
            cli(&base)
                .args(["enqueue", text, "--chat", chat, "--request-id", &id])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for producer in producers {
        assert_eq!(
            success(producer.wait_with_output().unwrap())["status"],
            "pending"
        );
    }
    assert!(
        !cli(&base)
            .args(["enqueue", "conflicting text", "--request-id", &shared])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !cli(&base)
            .args(["withdraw", &held])
            .output()
            .unwrap()
            .status
            .success()
    );
    host.kill().unwrap();
    host.wait().unwrap();
    let saved = run(&["queue", "--chat", chat]);
    assert_eq!(saved[0]["status"], "uncertain");
    // The provider read the packet but gave no admission receipt. An incomplete WAL
    // tail cannot promote this attempt to sent or cause it to be dispatched again.
    let journal = base.join("store").join(format!("{chat}.karchat"));
    let data = fs::read_to_string(&journal).unwrap();
    assert!(!data.lines().any(|line| {
        let row: Value = serde_json::from_str(line).unwrap();
        row["kind"] == "input_sent" && row["id"] == held
    }));
    fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(br#"{"kind":"input_s"#)
        .unwrap();
    assert_eq!(run(&["queue", "--chat", chat])[0]["status"], "uncertain");
    assert_eq!(args("hold without reply", &held)["status"], "uncertain");
    assert_eq!(args("withdraw this", &skip)["status"], "withdrawn");
    args("same queue text", &Uuid::new_v4().to_string());
    assert_eq!(args("seed", seed["id"].as_str().unwrap())["status"], "sent");
    let saved = run(&["queue", "--chat", chat]);
    let pending: Vec<_> = saved
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["status"] == "pending")
        .collect();
    assert_eq!(pending.len(), 3);

    run(&["resume", "after drain", "--chat", chat]);
    assert_eq!(args("hold without reply", &held)["status"], "uncertain");
    assert_eq!(args("same queue text", &shared)["status"], "sent");
    run(&["resume", "--chat", chat]);
    let expected: Vec<_> = ["seed", "hold without reply"]
        .into_iter()
        .chain(pending.iter().map(|e| e["text"].as_str().unwrap()))
        .chain(["after drain"])
        .collect();
    let calls: Vec<Value> = fs::read_to_string(base.join("native/calls"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(
        calls
            .iter()
            .map(|c| c["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(calls.iter().all(|c| c["id"] == history["chat"]["native_id"]
        && c["cwd"] == base.join("work").to_str().unwrap()));
    let history = run(&["history", "--chat", chat]);
    for (input, turn) in pending
        .iter()
        .zip(&history["turns"].as_array().unwrap()[2..])
    {
        assert_eq!(turn["id"], input["id"]);
        assert_eq!(turn["status"], "completed");
        assert!(turn["checkpoint"]["after_tree"].is_string());
    }
    args("parent only", &Uuid::new_v4().to_string());
    let tip = history["turns"].as_array().unwrap().last().unwrap()["id"]
        .as_str()
        .unwrap();
    let child = run(&["fork", tip, "--chat", chat]);
    assert_eq!(
        run(&["queue", "--chat", child["chat"]["id"].as_str().unwrap()]),
        json!([])
    );
    assert_eq!(
        run(&["queue", "--chat", chat])
            .as_array()
            .unwrap()
            .last()
            .unwrap()["status"],
        "pending"
    );
    // Crash once more before the provider even acknowledges session resume.
    let mut attaching = cli(&base)
        .args(["resume", "--chat", chat])
        .env("FIXTURE_PAUSE_RESUME", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !base.join("native/awaiting-resume").exists() {
        assert!(
            Instant::now() < deadline,
            "resume never reached native handshake"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
    attaching.kill().unwrap();
    attaching.wait().unwrap();
    run(&["resume", "--chat", chat]);
    run(&["resume", "--chat", chat]);
    let calls = fs::read_to_string(base.join("native/calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("parent only"))
            .count(),
        1
    );
    delayed_admission(&base, chat.parse().unwrap());
}

// A slow producer holding the queue lock must not let a managed native command outlive
// its workspace lease, even if the normal acknowledgement timeout would have elapsed.
#[tokio::main]
async fn delayed_admission(base: &Path, chat: Uuid) {
    let store_dir = base.join("store");
    let store = ChatStore::open(&store_dir).unwrap();
    let id = Uuid::new_v4();
    store.enqueue(chat, id, "delayed admission").unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(store_dir.join(format!("{chat}.inputs.lock")))
        .unwrap();
    lock.lock().unwrap();
    let chat_session = store
        .resume_with(chat, |opts| {
            opts.handshake_timeout = Duration::from_millis(400);
            opts.env.insert(
                "FIXTURE_NATIVE_DB".into(),
                base.join("native").into_os_string(),
            );
        })
        .await
        .unwrap();
    let checkpoint = store_dir.join(format!("{chat}.git/refs/kar/turns/{id}/before"));
    let release = tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !checkpoint.exists() {
            assert!(
                Instant::now() < deadline,
                "queue head did not reach dispatch"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        drop(lock);
    });
    tokio::time::timeout(Duration::from_secs(10), chat_session.drain_queue())
        .await
        .unwrap()
        .unwrap();
    release.await.unwrap();
    let history = chat_session.history().unwrap();
    let tip = history.turns.last().unwrap();
    assert_eq!((tip.id, tip.status), (id, TurnStatus::Completed));
    assert!(tip.checkpoint.as_ref().unwrap().after_tree.is_some());
    chat_session.close().await.unwrap();
    assert_eq!(
        fs::read_to_string(base.join("native/calls"))
            .unwrap()
            .lines()
            .filter(|s| s.contains("delayed admission"))
            .count(),
        1
    );
}
struct FeedServer(std::process::Child);
impl Drop for FeedServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl FeedServer {
    fn launch(base: &Path, chat: &str) -> (Self, String) {
        let mut server = cli(base)
            .args(["serve", "--chat", chat])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut welcome = String::new();
        std::io::BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut welcome)
            .unwrap();
        let value: Value = serde_json::from_str(&welcome).unwrap();
        (Self(server), value["url"].as_str().unwrap().to_owned())
    }
}
#[path = "support/sse.rs"]
mod sse;
use sse::SseClient;
#[tokio::main]
async fn live_feed() {
    use kar::chat::{ChatFeedSnapshot, ChatReplica};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["work", "elsewhere", "native"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    success(
        cli(&base)
            .args(["new", "codex", "initial", "--cwd"])
            .arg(base.join("work"))
            .arg("--bin")
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap(),
    );
    let store = ChatStore::open(base.join("store")).unwrap();
    let id = store.latest().unwrap();
    let (server, url) = FeedServer::launch(&base, &id.to_string());
    let mut remote = kar::chat::HttpChatFeed::new(&url).unwrap();
    let snapshot: ChatFeedSnapshot = remote.snapshot().await.unwrap();
    let start = snapshot.cursor.to_string();
    let initial_cursor = snapshot.cursor;
    let mut one = ChatReplica::from_snapshot(snapshot);
    let mut two = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut a = SseClient::open(&url, &start, None).await;
    let mut b = SseClient::open(&url, &start, None).await;
    let mut sending = cli(&base)
        .args(["resume", "observe live tool"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    loop {
        let (e, f) = tokio::join!(a.next(), b.next());
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::to_value(&f).unwrap()
        );
        assert!(one.apply(e.clone()).unwrap());
        assert!(!one.apply(e).unwrap());
        assert!(two.apply(f).unwrap());
        if one
            .turns()
            .last()
            .unwrap()
            .items
            .iter()
            .any(|i| matches!(&i.item,Item::Tool{id,..} if id=="stream-tool"))
        {
            break;
        }
    }
    assert_eq!(two.turns().last().unwrap().status, TurnStatus::Running);
    {
        let reader = fs::File::open(base.join("store").join(format!("{id}.karchat"))).unwrap();
        reader.lock_shared().unwrap();
        let snapshot = tokio::time::timeout(Duration::from_secs(3), remote.snapshot())
            .await
            .expect("a second reader blocked a live HTTP snapshot")
            .unwrap();
        assert_eq!(
            snapshot.history.turns.last().unwrap().status,
            TurnStatus::Running
        );
    }
    drop(b);
    fs::write(base.join("native/release-stream-tool"), "").unwrap();
    assert!(sending.wait().unwrap().success());
    let live_tip = store.feed_snapshot(id).unwrap().cursor;
    let mut suffix = vec![];
    while one.cursor() != live_tip {
        let event = a.next().await;
        suffix.push(serde_json::to_value(&event).unwrap());
        one.apply(event).unwrap();
    }
    assert_eq!(one.turns(), store.history(id).unwrap().turns);
    let invocations: Vec<_> = one
        .turns()
        .last()
        .unwrap()
        .items
        .iter()
        .filter_map(|item| match &item.item {
            Item::Tool { id, activity } if id == "stream-tool" => {
                Some((activity.input.clone(), activity.result.as_ref()?.text()?))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        invocations,
        [
            (Some(json!("fixture")), "result".into()),
            (Some(json!("second invocation")), "second result".into())
        ]
    );
    let first_remote = tokio::time::timeout(Duration::from_secs(4), remote.recv(initial_cursor))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_remote.previous, initial_cursor);
    drop(a);
    drop(server);
    let (_server, url) = FeedServer::launch(&base, &id.to_string());
    let mut b = SseClient::open(&url, &start, Some(&two.cursor().to_string())).await;
    let before_gap = two.cursor();
    assert!(matches!(
        two.apply(serde_json::from_value(suffix[1].clone()).unwrap()),
        Err(kar::Error::InvalidCursor)
    ));
    assert_eq!(two.cursor(), before_gap);
    for expected in suffix {
        let e = b.next().await;
        assert_eq!(serde_json::to_value(&e).unwrap(), expected);
        two.apply(e).unwrap();
    }
    let history = store.history(id).unwrap();
    assert_eq!(two.usage(), history.usage);
    assert_eq!(one.usage(), history.usage);
    assert_eq!(history.usage.len(), 2);
    let initial = history.usage[0].usage.total.unwrap();
    let current = &history.usage[1].usage;
    let total = current.total.unwrap();
    let combined = current.with_children.unwrap();
    assert_eq!((initial.input, initial.output), (10, 4));
    assert_eq!((total.input, total.output), (11, 5));
    assert_eq!((combined.input, combined.output), (31, 10));
    assert!(current.by_agent.is_none());
    let children = &history.usage[1].children;
    assert_eq!(children.len(), 2);
    assert_eq!(children["deep"].total.unwrap().input, 6);
    assert_eq!(children["worker"].total.unwrap().input, 14);
    let printed = cli(&base)
        .args(["usage", "--chat", &id.to_string(), "--json"])
        .output()
        .unwrap();
    assert!(printed.status.success());
    assert_eq!(
        serde_json::from_slice::<Vec<kar::UsageSnapshot>>(&printed.stdout).unwrap(),
        history.usage
    );
    let reports: Vec<_> = two
        .turns()
        .last()
        .unwrap()
        .items
        .iter()
        .filter_map(|item| match &item.item {
            Item::Compaction { report } => Some(report),
            _ => None,
        })
        .collect();
    assert_eq!(
        reports.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(),
        ["started", "completed"]
    );
    assert_eq!(reports[0].compaction_id.as_deref(), Some("compact-1"));
    assert_eq!(history.last_compaction.as_ref(), reports.last().copied());
    assert_eq!(history.last_compaction.as_ref(), two.last_compaction());
    let mut killed = cli(&base)
        .args(["resume", "crash here"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    loop {
        two.apply(b.next().await).unwrap();
        let turn = two.turns().last().unwrap();
        if turn.status == TurnStatus::Running
            && turn.items.iter().any(
                |i| matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="haha"),
            )
        {
            break;
        }
    }
    killed.kill().unwrap();
    killed.wait().unwrap();
    while two.turns().last().unwrap().status == TurnStatus::Running {
        two.apply(b.next().await).unwrap();
    }
    assert_eq!(two.turns().last().unwrap().status, TurnStatus::Interrupted);
    let interrupted = two.turns().last().unwrap();
    let interrupted_id = interrupted.id;
    let native = interrupted.native_turn_id.as_deref().unwrap();
    fs::write(base.join("native/turns"), json!([{"id":native,"status":"failed","error":{"codexErrorInfo":"contextWindowExceeded","message":"native context exhausted"},"items":[]}]).to_string()).unwrap();
    success(
        cli(&base)
            .args(["resume", "after feed crash"])
            .output()
            .unwrap(),
    );
    let tip = store.feed_snapshot(id).unwrap().cursor;
    while two.cursor() != tip {
        two.apply(b.next().await).unwrap();
    }
    assert_eq!(two.turns(), store.history(id).unwrap().turns);
    let recovered = two.turns().iter().find(|t| t.id == interrupted_id).unwrap();
    assert_eq!(recovered.status, TurnStatus::Failed);
    assert_eq!(
        recovered.failure.as_ref().unwrap().class,
        kar::FailureClass::InputTooLarge
    );
    // An uncommitted tail has no event id. Resuming truncates it, and the same
    // observer must read the replacement row from the last committed boundary.
    let journal = base.join("store").join(format!("{id}.karchat"));
    OpenOptions::new()
        .append(true)
        .open(journal)
        .unwrap()
        .write_all(b"{\"kind\":")
        .unwrap();
    assert_eq!(store.feed_snapshot(id).unwrap().cursor, tip);
    let mut idle = store.follow(id, Some(tip)).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), idle.recv())
            .await
            .is_err()
    );
    assert_eq!(idle.cursor(), tip);
    success(
        cli(&base)
            .args(["resume", "after partial row"])
            .output()
            .unwrap(),
    );
    let tip = store.feed_snapshot(id).unwrap().cursor;
    while two.cursor() != tip {
        two.apply(b.next().await).unwrap();
    }
    assert_eq!(two.turns(), store.history(id).unwrap().turns);
    let _ = cli(&base)
        .args(["resume", "declared quota"])
        .output()
        .unwrap();
    let tip = store.feed_snapshot(id).unwrap().cursor;
    while two.cursor() != tip {
        two.apply(b.next().await).unwrap();
    }
    let failed = two.turns().last().unwrap();
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(
        failed.failure.as_ref().unwrap().class,
        kar::FailureClass::Quota
    );
    assert_eq!(
        failed.failure.as_ref().unwrap().resets_at_ms,
        Some(1_900_000_000_000)
    );
    assert_eq!(two.turns(), store.history(id).unwrap().turns);
    // A browser reconnects to its original URL, even if that fallback is obsolete.
    let mut a = SseClient::open(&url, "obsolete-format", Some(&one.cursor().to_string())).await;
    while one.cursor() != tip {
        let event = a.next().await;
        assert!(one.apply(event.clone()).unwrap());
        assert!(!one.apply(event).unwrap());
    }
    assert_eq!(one.turns(), two.turns());
    for invalid in [
        format!("{id}:1"),
        format!("{id}:{}", u64::MAX),
        format!("{}:1", Uuid::new_v4()),
    ] {
        let r = reqwest::Client::new()
            .get(format!("{url}/events"))
            .header("last-event-id", invalid)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    }
}
fn legacy_recording() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for d in ["work", "elsewhere", "native"] {
        fs::create_dir(base.join(d)).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let recording = base.join("old.jsonl");
    let raw = Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(&base)
        .env("FIXTURE_NATIVE_DB", base.join("native"))
        .args(["run", "codex", "original text", "--cwd", "work", "--bin"])
        .arg(&exe)
        .arg("--record")
        .arg(&recording)
        .output()
        .unwrap();
    assert!(
        raw.status.success(),
        "{}",
        String::from_utf8_lossy(&raw.stderr)
    );
    let mut old = Vec::new();
    for line in fs::read_to_string(&recording).unwrap().lines() {
        let mut row: Value = serde_json::from_str(line).unwrap();
        row["events"]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["kind"] != "user_message" && e["kind"] != "prompt_dispatched");
        writeln!(old, "{row}").unwrap();
    }
    fs::write(&recording, old).unwrap();
    let import = |cwd: &str| {
        cli(&base)
            .args(["import", "codex"])
            .arg(&recording)
            .arg("--bin")
            .arg(&exe)
            .arg("--cwd")
            .arg(base.join(cwd))
            .output()
            .unwrap()
    };
    assert!(!import("elsewhere").status.success());
    let info = success(import("work"));
    assert!(!import("work").status.success());
    success(cli(&base).args(["resume", "continued"]).output().unwrap());
    let h = success(cli(&base).arg("history").output().unwrap());
    assert_eq!(h["chat"]["native_id"], info["native_id"]);
    assert_eq!(h["turns"].as_array().unwrap().len(), 2);
    assert_eq!(h["turns"][0]["items"][0]["text"], "original text");
    assert_eq!(h["turns"][0]["items"].as_array().unwrap().len(), 3);
}
// Independent observations of the checkout, including ignored files and symlink targets.
fn checkout_bytes(work: &Path) -> std::collections::BTreeMap<PathBuf, (u32, Vec<u8>)> {
    ignore::WalkBuilder::new(work)
        .standard_filters(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .map(|entry| {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = fs::symlink_metadata(path).unwrap();
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode()
            };
            #[cfg(not(unix))]
            let mode = u32::from(meta.is_dir());
            let contents = if meta.is_symlink() {
                fs::read_link(path)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else if meta.is_file() {
                fs::read(path).unwrap()
            } else {
                Vec::new()
            };
            (
                path.strip_prefix(work).unwrap().to_owned(),
                (mode, contents),
            )
        })
        .collect()
}

fn check_paths(files: &Value, expected: &[(&str, &str)]) {
    let mut actual: Vec<_> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["path"].as_str().unwrap(), f["kind"].as_str().unwrap()))
        .collect();
    let mut expected = expected.to_vec();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

fn checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let work = base.join("work");
    for dir in ["work", "native", "elsewhere"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let repo = gix::init(&work).unwrap();
    let mut tree = repo
        .edit_tree(gix::ObjectId::empty_tree(repo.object_hash()))
        .unwrap();
    tree.upsert(
        "staged.txt",
        gix::objs::tree::EntryKind::Blob,
        repo.write_blob(b"staged").unwrap(),
    )
    .unwrap();
    let tree = tree.write().unwrap();
    repo.index_from_tree(&tree)
        .unwrap()
        .write(Default::default())
        .unwrap();
    let staged = fs::read(repo.index_path()).unwrap();
    let head = fs::read(repo.git_dir().join("HEAD")).unwrap();
    fs::write(work.join(".gitignore"), "staged.txt\n*.tmp\n").unwrap();
    fs::write(work.join("staged.txt"), "my unstaged edit").unwrap();
    fs::write(work.join("gone"), "baseline").unwrap();
    fs::write(work.join("mode"), "mode").unwrap();
    let run = |args: &[&str]| {
        cli(&base)
            .args(args)
            .env("PATH", base.join("elsewhere"))
            .output()
            .unwrap()
    };
    let one = success(run(&[
        "new",
        "codex",
        "edit-one",
        "--bin",
        std::env::current_exe().unwrap().to_str().unwrap(),
        "--cwd",
        work.to_str().unwrap(),
    ]));
    let first = one["id"].as_str().unwrap();
    check_paths(
        &success(run(&["changes", first])),
        &[
            ("binary", "added"),
            ("gone", "deleted"),
            ("shape", "added"),
            ("staged.txt", "modified"),
        ],
    );
    fs::write(work.join("manual"), "outside turns").unwrap();
    let two = success(run(&["resume", "edit-two"]));
    let second = two["id"].as_str().unwrap();
    let files = success(run(&["changes", second]));
    assert_eq!(files, two["checkpoint"]["files"]);
    let mut second_changes = vec![
        ("staged.txt", "modified"),
        ("shape", "deleted"),
        ("shape/child", "added"),
        ("binary", "deleted"),
        ("new file", "added"),
    ];
    if cfg!(unix) {
        second_changes.extend([("mode", "modified"), ("link", "added")]);
    }
    check_paths(&files, &second_changes);
    let middle = checkout_bytes(&work);
    let three = success(run(&["resume", "edit-three"]));
    let mut third_changes = vec![
        ("staged.txt", "modified"),
        ("shape/child", "deleted"),
        ("binary", "added"),
        ("new file", "modified"),
    ];
    if cfg!(unix) {
        third_changes.extend([("mode", "modified"), ("link", "deleted")]);
    }
    check_paths(
        &success(run(&["changes", three["id"].as_str().unwrap()])),
        &third_changes,
    );
    assert_eq!(
        three["checkpoint"]["before_tree"],
        two["checkpoint"]["after_tree"]
    );
    assert_ne!(
        three["checkpoint"]["after_tree"],
        two["checkpoint"]["after_tree"]
    );
    assert_eq!(fs::read(work.join("binary")).unwrap(), [0, 128, 255]);
    assert!(
        success(run(&["rollback", second]))["applied"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(checkout_bytes(&work), middle);
    assert_eq!(fs::read(repo.index_path()).unwrap(), staged);
    assert_eq!(fs::read(repo.git_dir().join("HEAD")).unwrap(), head);
    fs::write(work.join("staged.txt"), "my later edit").unwrap();
    let conflict = run(&["rollback", first]);
    assert!(!conflict.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&conflict.stdout).unwrap()["conflicts"][0]["path"],
        "staged.txt"
    );
    assert!(work.join("shape/child").is_file());
    assert!(!work.join("binary").exists());
    fs::write(work.join("staged.txt"), "second").unwrap();
    fs::write(work.join("shape/ignored.tmp"), "do not delete").unwrap();
    assert!(!run(&["rollback", first]).status.success());
    assert!(work.join("shape/ignored.tmp").is_file());
    fs::remove_file(work.join("shape/ignored.tmp")).unwrap();
    assert_eq!(
        success(run(&["rollback", first, "--dry-run"]))["applied"],
        false
    );
    assert!(work.join("shape/child").is_file());
    // Simulate a previous host that exited just after writing one destination file.
    let history = success(run(&["history"]));
    let journal = base.join("store").join(format!(
        "{}.karchat",
        history["chat"]["id"].as_str().unwrap()
    ));
    writeln!(
        OpenOptions::new().append(true).open(journal).unwrap(),
        "{}",
        json!({"kind":"restore_started","target":{"turn":first,"before":false}})
    )
    .unwrap();
    fs::write(work.join("binary"), [0, 255, 6]).unwrap();
    assert!(!run(&["resume", "must wait"]).status.success());
    assert_eq!(success(run(&["rollback", first]))["applied"], true);
    assert_eq!(fs::read(work.join("binary")).unwrap(), [0, 255, 6]);
    assert_eq!(fs::read(work.join("staged.txt")).unwrap(), b"first");
    assert_eq!(fs::read(work.join("shape")).unwrap(), b"leaf");
    assert_eq!(
        fs::read(work.join("ignored.tmp")).unwrap(),
        b"second ignored"
    );
    assert_eq!(fs::read(work.join("manual")).unwrap(), b"outside turns");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(work.join("mode"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );
        assert!(fs::symlink_metadata(work.join("link")).is_err());
    }
    // Continue from the restored workspace, then move back to the preserved sibling branch.
    success(run(&["resume", "branch-edit"]));
    success(run(&["rollback", second]));
    assert!(work.join("shape/child").is_file());
    assert!(!work.join("branch").exists());
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(work.join("link")).unwrap(),
        Path::new("../elsewhere")
    );
    success(run(&["rollback", first, "--before"]));
    assert_eq!(
        fs::read(work.join("staged.txt")).unwrap(),
        b"my unstaged edit"
    );
    assert!(work.join("gone").exists());
    assert_eq!(fs::read(repo.index_path()).unwrap(), staged);
    assert_eq!(fs::read(repo.git_dir().join("HEAD")).unwrap(), head);
    assert_eq!(
        success(run(&["history"]))["turns"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let shadow = gix::open(
        base.join("store")
            .join(format!("{}.git", history["chat"]["id"].as_str().unwrap())),
    )
    .unwrap();
    assert_eq!(
        shadow
            .find_reference(format!("refs/kar/turns/{second}/after").as_str())
            .unwrap()
            .id()
            .to_string(),
        two["checkpoint"]["after_tree"]
    );
    let next = success(run(&["resume", "edit-one"]));
    fs::write(work.join("staged.txt"), "edited between turns").unwrap();
    let last = success(run(&["resume", "edit-two"]));
    let older = run(&["rollback", next["id"].as_str().unwrap()]);
    assert!(!older.status.success());
    let preview: Value = serde_json::from_slice(&older.stdout).unwrap();
    assert_eq!(preview["conflicts"][0]["path"], "staged.txt");
    assert_eq!(fs::read(work.join("staged.txt")).unwrap(), b"second");
    success(run(&["rollback", last["id"].as_str().unwrap(), "--before"]));
    assert_eq!(
        fs::read(work.join("staged.txt")).unwrap(),
        b"edited between turns"
    );
}
fn linked_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = gix::init(base.join("main")).unwrap();
    let linked = base.join("linked");
    let workspace = linked.join("component");
    let admin = repo.git_dir().join("worktrees/task");
    for path in [
        &workspace,
        &admin,
        &base.join("elsewhere"),
        &base.join("native"),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(admin.join("commondir"), "../..\n").unwrap();
    fs::write(admin.join("HEAD"), "ref: refs/heads/task\n").unwrap();
    fs::write(
        admin.join("gitdir"),
        format!("{}\n", linked.join(".git").display()),
    )
    .unwrap();
    fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .unwrap();
    fs::write(linked.join(".gitignore"), "*.tmp\n").unwrap();
    fs::write(workspace.join("gone"), "last copy").unwrap();
    fs::write(linked.join("sibling"), "sibling").unwrap();
    let run = |args: &[&str]| {
        success(
            cli(&base)
                .env("KAR_CHAT_DIR", workspace.join("state"))
                .args(args)
                .output()
                .unwrap(),
        )
    };
    let turn = run(&[
        "new",
        "codex",
        "edit-one",
        "--bin",
        std::env::current_exe().unwrap().to_str().unwrap(),
        "--cwd",
        workspace.to_str().unwrap(),
    ]);
    let files = turn["checkpoint"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 4, "{files:?}");
    run(&["rollback", turn["id"].as_str().unwrap(), "--before"]);
    assert_eq!(fs::read(workspace.join("gone")).unwrap(), b"last copy");
    assert_eq!(fs::read(linked.join("sibling")).unwrap(), b"sibling");
    assert_eq!(
        fs::read(workspace.join("ignored.tmp")).unwrap(),
        b"first ignored"
    );
}
fn coordinated_fork() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for path in ["work", "native", "elsewhere"] {
        fs::create_dir(base.join(path)).unwrap();
    }
    let work = base.join("work");
    let database = base.join("native");
    fs::write(work.join("gone"), "baseline").unwrap();
    fs::write(work.join("mode"), "mode").unwrap();
    fs::write(work.join(".gitignore"), "*.tmp\n").unwrap();
    let first = success(
        cli(&base)
            .args(["new", "codex", "edit-one", "--bin"])
            .arg(std::env::current_exe().unwrap())
            .arg("--cwd")
            .arg(&work)
            .output()
            .unwrap(),
    );
    let first = first["id"].as_str().unwrap();
    let second = success(cli(&base).args(["resume", "edit-two"]).output().unwrap());
    let parent = success(cli(&base).arg("history").output().unwrap());
    let id = parent["chat"]["id"].as_str().unwrap();
    let fork = || {
        let mut cmd = cli(&base);
        cmd.args(["fork", first, "--chat", id]);
        cmd
    };
    fs::write(work.join("local-only"), "mine").unwrap();
    let conflict = fork().output().unwrap();
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("local-only"));
    assert!(!database.join("forks").exists());
    fs::remove_file(work.join("local-only")).unwrap();
    assert!(
        !fork()
            .env("FIXTURE_FORK_REFUSED", "1")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read(work.join("staged.txt")).unwrap(), b"second");
    assert_eq!(
        success(cli(&base).arg("list").output().unwrap())
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Kill the host after file restoration but before the child becomes visible.
    let other = success(
        cli(&base)
            .args(["new", "codex", "--cwd"])
            .arg(&work)
            .arg("--bin")
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap(),
    );
    let mut process = fork()
        .env("FIXTURE_PAUSE_BRANCH", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !database.join("awaiting-publication").exists() {
        assert!(
            process.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "fork did not reach publication"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    process.kill().unwrap();
    process.wait().unwrap();
    assert_eq!(fs::read(work.join("staged.txt")).unwrap(), b"first");
    assert_eq!(
        success(cli(&base).arg("list").output().unwrap())
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let waiting = success(cli(&base).args(["history", "--chat", id]).output().unwrap());
    assert_eq!(waiting["pending_fork"]["turn"], first);
    let blocked = cli(&base)
        .args([
            "resume",
            "wrong workspace",
            "--chat",
            other["chat"]["id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !blocked.status.success()
            && String::from_utf8_lossy(&blocked.stderr).contains("pending fork")
    );
    assert!(
        !cli(&base)
            .args(["resume", "wrong context", "--chat", id])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !cli(&base)
            .args(["rollback", second["id"].as_str().unwrap(), "--chat", id])
            .output()
            .unwrap()
            .status
            .success()
    );
    let branch = success(fork().output().unwrap());
    assert_eq!(
        branch["chat"]["native_id"],
        fs::read_to_string(database.join("awaiting-publication")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(database.join("forks"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(branch["turns"].as_array().unwrap().len(), 1);
    assert_eq!(branch["turns"][0]["id"], first);
    assert_eq!(
        success(cli(&base).args(["history", "--chat", id]).output().unwrap())["turns"],
        parent["turns"]
    );
    success(cli(&base).args(["resume", "branch-edit"]).output().unwrap());
    // Roll back an inherited checkpoint after a new branch-specific checkpoint.
    success(cli(&base).args(["rollback", first]).output().unwrap());
    assert!(!work.join("branch").exists());
}
#[tokio::main]
async fn reopen_after_disconnected_writer_and_history_probe() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let database = base.join("native");
    let work = base.join("work");
    fs::create_dir(&database).unwrap();
    fs::create_dir(&work).unwrap();
    let store_dir = base.join("store");
    let store = ChatStore::open(&store_dir).unwrap();
    let mut opts = kar::SessionOptions::new(&work);
    opts.env
        .insert("FIXTURE_NATIVE_DB".into(), database.clone().into());
    let parent = store
        .create(
            "codex",
            Some(&std::env::current_exe().unwrap()),
            opts.clone(),
        )
        .await
        .unwrap();
    let id = parent.info().id;
    let native = parent.info().native_id.clone();
    let first = parent.send(Uuid::new_v4(), "source turn").await.unwrap();
    fs::write(database.join("hold-native-fork"), "").unwrap();
    {
        // Drop the caller once the native fork is doing work. The store still
        // owns publication of that fork and its writes to the source journal.
        let mut request = Box::pin(parent.fork_at(first.id));
        tokio::select! {
            _ = &mut request => panic!("fork returned before publication"),
            result = tokio::time::timeout(Duration::from_secs(5),async {
                while !database.join("entered-native-fork").exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }) => result.unwrap(),
        }
    }
    let mut close = tokio::spawn(parent.close());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut close)
            .await
            .is_err()
    );
    fs::remove_file(database.join("hold-native-fork")).unwrap();
    tokio::time::timeout(Duration::from_secs(8), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    // Hold the two read locks of a history observer. A new controller must
    // wait for the journal snapshot rather than reporting a competing writer.
    let journal = fs::File::open(store_dir.join(format!("{id}.karchat"))).unwrap();
    let lease = fs::File::open(store_dir.join(format!("{id}.lock"))).unwrap();
    journal.lock_shared().unwrap();
    lease.lock_shared().unwrap();
    let mut reopen = tokio::spawn(async move { store.resume_with(id, |o| o.env = opts.env).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut reopen)
            .await
            .is_err()
    );
    lease.unlock().unwrap();
    journal.unlock().unwrap();
    let chat = tokio::time::timeout(Duration::from_secs(5), reopen)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(chat.info().native_id, native);
    assert_eq!(chat.history().unwrap().turns[0].id, first.id);
    chat.close().await.unwrap();
}
#[tokio::main]
async fn fork_from_open_session() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let work = base.join("work");
    let database = base.join("native");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&database).unwrap();
    let store = ChatStore::open(base.join("store")).unwrap();
    let mut options = kar::SessionOptions::new(&work);
    options
        .env
        .insert("FIXTURE_NATIVE_DB".into(), database.into_os_string());
    let parent = store
        .create("codex", Some(&std::env::current_exe().unwrap()), options)
        .await
        .unwrap();
    let first = parent.send(Uuid::new_v4(), "first").await.unwrap();
    parent.send(Uuid::new_v4(), "branch-edit").await.unwrap();
    let stable = Uuid::new_v4();
    let result = parent
        .fork_at_with_id(first.id, stable, None, |opts| {
            opts.remove_env("KAR_WORKBENCH_TOKEN").unwrap();
        })
        .await
        .unwrap();
    assert_eq!(result.chat.id, stable);
    assert_eq!(result.turns.len(), 1);
    assert!(result.chat.unset_env.contains("KAR_WORKBENCH_TOKEN"));
    let replay = parent
        .fork_at_with_id(first.id, stable, None, |_| {
            panic!("published child should not be relaunched")
        })
        .await
        .unwrap();
    assert_eq!(replay.chat.native_id, result.chat.native_id);
    assert_eq!(parent.history().unwrap().turns.len(), 2);
    let child = parent.fork_at(first.id).await.unwrap();
    assert!(!work.join("branch").exists());
    assert_eq!(child.history().unwrap().turns, vec![first]);
    fs::write(work.join("changed-after-fork"), "new local edit").unwrap();
    let rejected = child.send(Uuid::new_v4(), "wrong files").await.unwrap_err();
    assert!(rejected.to_string().contains("changed-after-fork"));
    assert_eq!(child.history().unwrap().turns.len(), 1);
    fs::remove_file(work.join("changed-after-fork")).unwrap();
    child.send(Uuid::new_v4(), "child only").await.unwrap();
    assert_eq!(parent.history().unwrap().turns.len(), 2);
    assert_ne!(parent.info().native_id, child.info().native_id);
    child.close().await.unwrap();
    let first = parent
        .send(Uuid::new_v4(), "open deferred tool")
        .await
        .unwrap();
    let snap = store.feed_snapshot(parent.info().id).unwrap();
    let mut feed = store.follow(parent.info().id, Some(snap.cursor)).unwrap();
    let mut replica = kar::chat::ChatReplica::from_snapshot(snap);
    let second = parent
        .send(Uuid::new_v4(), "finish deferred tool")
        .await
        .unwrap();
    assert!(
        !second
            .items
            .iter()
            .any(|i| matches!(i.item, Item::Tool { .. }))
    );
    let tip = store.feed_snapshot(parent.info().id).unwrap();
    while replica.cursor() != tip.cursor {
        replica.apply(feed.recv().await.unwrap()).unwrap();
    }
    assert_eq!(replica.turns(), tip.history.turns);
    let prior = replica.turns().iter().find(|t| t.id == first.id).unwrap();
    let activity = prior
        .items
        .iter()
        .find_map(|i| match &i.item {
            Item::Tool { activity, .. } => Some(activity),
            _ => None,
        })
        .unwrap();
    assert_eq!(activity.preview, "late output");
    assert!(activity.finished);
    assert_eq!(activity.result.as_ref().unwrap().exit_code, Some(9));
    assert!(prior.tool_groups("codex").iter().any(|group| matches!(
        group,
        kar::observe::ToolGroupSegment::Tools {
            failed: 1,
            running: None,
            ..
        }
    )));
    parent.close().await.unwrap();
}
#[tokio::main]
async fn hold_idle_form(base: PathBuf) {
    let store = ChatStore::open(base.join("store")).unwrap();
    let mut opts = kar::SessionOptions::new(base.join("work"));
    opts.approvals = kar::ApprovalMode::Host;
    opts.env.insert(
        "FIXTURE_NATIVE_DB".into(),
        base.join("native").into_os_string(),
    );
    opts.env.insert("FIXTURE_IDLE_FORM".into(), "1".into());
    let chat = store
        .create("codex", Some(&std::env::current_exe().unwrap()), opts)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while chat.history().unwrap().pending_requests.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    fs::write(base.join("ready"), chat.info().id.to_string()).unwrap();
    std::future::pending::<()>().await;
}
#[tokio::main]
async fn abandoned_idle_question() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for name in ["work", "native"] {
        fs::create_dir(base.join(name)).unwrap();
    }
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .arg("hold-idle-form")
        .arg(&base)
        .spawn()
        .unwrap();
    let store = ChatStore::open(base.join("store")).unwrap();
    let id = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(id) = fs::read_to_string(base.join("ready"))
                && let Ok(id) = id.parse::<Uuid>()
            {
                break id;
            }
            assert!(
                owner.try_wait().unwrap().is_none(),
                "form controller exited before opening its request"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let snap = store.feed_snapshot(id).unwrap();
    assert!(snap.history.turns.is_empty());
    let req = &snap.history.pending_requests[0];
    assert_eq!(
        serde_json::to_value(req).unwrap()["controls"]["form"]["fields"][0]["label"],
        "Choose a color"
    );
    let mut a = store.follow(id, Some(snap.cursor)).unwrap();
    let mut b = store.follow(id, Some(snap.cursor)).unwrap();
    let mut replica = kar::chat::ChatReplica::from_snapshot(snap);
    owner.kill().unwrap();
    owner.wait().unwrap();
    let (a, b) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(a.recv(), b.recv())
    })
    .await
    .unwrap();
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.cursor, b.cursor);
    assert!(replica.apply(a).unwrap());
    assert!(!replica.apply(b).unwrap());
    assert!(replica.pending_requests().is_empty());
    let now = store.feed_snapshot(id).unwrap();
    assert_eq!(now.cursor, replica.cursor());
    assert!(now.history.pending_requests.is_empty() && now.history.turns.is_empty());
}
fn opencode_peer() {
    let root = "open-root";
    let child = "open-child";
    let leaf = "open-leaf";
    let status = |parent: &str, child: &str, value: &str| {
        emit(json!({"method":"opencode/session/child_update","params":{
        "rootSessionId":root,"parentSessionId":parent,"childSessionId":child,"type":"status","status":value,"title":child}}))
    };
    let text = |content: &str| json!({"sessionUpdate":"agent_message_chunk","messageId":"m","content":{"type":"text","text":content}});
    let nested = |parent: &str, child: &str, update: Value| {
        emit(json!({"method":"opencode/session/child_update","params":{
        "rootSessionId":root,"parentSessionId":parent,"childSessionId":child,"type":"update","update":update}}))
    };
    let standard = |update: Value| {
        emit(json!({"method":"session/update","params":{"sessionId":root,"update":update}}))
    };
    let mut dup = text("COPY");
    dup["_meta"] = json!({"opencode/child-session":{"id":child,"parentID":root}});
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let reply = |body: Value| emit(json!({"id":v["id"],"result":body}));
        match v["method"].as_str().unwrap() {
            "initialize" => {
                assert_eq!(
                    v["params"]["clientCapabilities"]["_meta"]["opencode/child-session-updates"],
                    true
                );
                reply(
                    json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{}}}}),
                );
            }
            "session/list" => reply(
                json!({"sessions":[{"sessionId":root,"cwd":std::env::current_dir().unwrap()}]}),
            ),
            "session/new" | "session/load" => reply(json!({"sessionId":root})),
            "session/prompt" => {
                let input = &v["params"]["prompt"][0]["text"];
                if input == "spawn" {
                    standard(dup.clone());
                    standard(
                        json!({"sessionUpdate":"tool_call","toolCallId":"spawn","title":"subagent","status":"in_progress"}),
                    );
                    status(root, child, "created");
                    nested(root, child, text("pre"));
                    standard(
                        json!({"sessionUpdate":"tool_call_update","toolCallId":"spawn","status":"completed","rawOutput":{"metadata":{"sessionID":child}}}),
                    );
                    nested(
                        root,
                        child,
                        json!({"sessionUpdate":"tool_call","toolCallId":"spawn-leaf","title":"subagent","rawOutput":{"metadata":{"sessionID":leaf}}}),
                    );
                    status(child, leaf, "created");
                    standard(text("first root"));
                } else if input == "reuse" {
                    standard(
                        json!({"sessionUpdate":"tool_call","toolCallId":"reuse","title":"subagent","status":"pending"}),
                    );
                    standard(
                        json!({"sessionUpdate":"tool_call_update","toolCallId":"reuse","rawInput":{"sessionID":child},"status":"in_progress"}),
                    );
                    status(root, child, "running");
                    nested(root, child, text("follow-up"));
                    standard(
                        json!({"sessionUpdate":"tool_call_update","toolCallId":"reuse","status":"completed","rawOutput":{"metadata":{"sessionID":child}}}),
                    );
                    status(root, child, "completed");
                    standard(text("third root"));
                } else {
                    status(root, child, "running");
                    nested(root, child, text("post"));
                    standard(dup.clone());
                    nested(child, leaf, text("deep"));
                    nested(
                        child,
                        leaf,
                        json!({"sessionUpdate":"tool_call","toolCallId":"leaf-call","title":"read","status":"in_progress"}),
                    );
                    nested(
                        child,
                        leaf,
                        json!({"sessionUpdate":"tool_call_update","toolCallId":"leaf-call","status":"completed","rawOutput":"result"}),
                    );
                    status(child, leaf, "completed");
                    status(root, child, "completed");
                    standard(text("second root"));
                }
                reply(json!({"stopReason":"end_turn"}));
            }
            other => panic!("unexpected method {other}"),
        }
    }
}

#[tokio::main]
async fn background_child_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir(&cwd).unwrap();
    let store = ChatStore::open(tmp.path().join("store")).unwrap();
    let setup = |opts: &mut kar::SessionOptions| {
        opts.retained_bytes = Some(1);
        opts.env.insert("KAR_TEST_OPENCODE".into(), "1".into());
        opts.env
            .insert("XDG_DATA_HOME".into(), tmp.path().join("data").into());
    };
    let mut opts = kar::SessionOptions::new(&cwd);
    setup(&mut opts);
    let session = store
        .create("opencode", Some(&std::env::current_exe().unwrap()), opts)
        .await
        .unwrap();
    let id = session.info().id;
    let first = session.send(Uuid::new_v4(), "spawn").await.unwrap();
    let graph = session.snapshot().graph;
    assert_eq!(graph.edges.len(), 2);
    assert_eq!(first.child_sessions["open-leaf"], "open-child");
    session.close().await.unwrap();
    let snapshot = store.feed_snapshot(id).unwrap();
    let mut updates = store.follow(id, Some(snapshot.cursor)).unwrap();
    let mut replica = kar::chat::ChatReplica::from_snapshot(snapshot);
    let session = store.resume_with(id, setup).await.unwrap();
    let second = session.send(Uuid::new_v4(), "another").await.unwrap();
    let third = session.send(Uuid::new_v4(), "reuse").await.unwrap();
    assert_eq!(third.child_sessions["open-child"], "open-root");
    assert!(
        third
            .items
            .iter()
            .any(|i| i.source.session_id.as_deref() == Some("open-child")
                && matches!(&i.item, Item::Message {text,complete:true,..} if text == "follow-up"))
    );
    let tip = store.feed_snapshot(id).unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while replica.cursor() != tip.cursor {
            let event = updates.recv().await.unwrap();
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(replica.turns(), tip.history.turns);
    assert_eq!(second.items.len(), 2);
    let previous = &replica.turns()[0];
    assert_eq!(previous.id, first.id);
    let texts = |child: &str| {
        previous
            .items
            .iter()
            .filter(|i| i.source.session_id.as_deref() == Some(child))
            .filter_map(|i| match &i.item {
                Item::Message { text, complete, .. } => Some((text.clone(), *complete)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(texts("open-child"), [("prepost".into(), true)]);
    assert_eq!(texts("open-leaf"), [("deep".into(), true)]);
    assert!(previous.items.iter().any(|i|matches!(&i.item,Item::Tool{id,activity} if id=="leaf-call" && activity.finished && activity.result.is_some())));
    assert!(
        replica
            .tasks()
            .iter()
            .all(|t| t.status == kar::TaskStatus::Completed)
    );
    session.close().await.unwrap();
}

#[tokio::main]
async fn codex_steer_receipt_survives_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    for dir in ["work", "native"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let db = base.join("store");
    let store = ChatStore::open(&db).unwrap();
    let mut options = kar::SessionOptions::new(base.join("work"));
    options
        .env
        .insert("FIXTURE_NATIVE_DB".into(), base.join("native").into());
    let chat = store
        .create("codex", Some(&std::env::current_exe().unwrap()), options)
        .await
        .unwrap();
    let (owner, refused, landed) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let echoed = Uuid::new_v4();
    let mut events = chat.subscribe(None).unwrap();
    let drive = async {
        while let Some(record) = events.next().await {
            if record.native["method"] == "turn/started" {
                break;
            }
        }
        assert!(chat.steer_with_id(refused, "refused steer").await.is_err());
        chat.steer_with_id(echoed, "echo before reply")
            .await
            .unwrap();
        assert!(
            chat.steer_with_id(echoed, "echo before reply")
                .await
                .is_err()
        );
        chat.steer_with_id(landed, "accepted steer").await.unwrap();
    };
    let (turn, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(chat.send(owner, "hold for scoped steer"), drive)
    })
    .await
    .unwrap();
    assert_eq!(turn.unwrap().id, owner);
    let chat_id = chat.info().id;
    chat.close().await.unwrap();
    let history = ChatStore::open(db).unwrap().history(chat_id).unwrap();
    assert_eq!(history.turns.len(), 1);
    let turn = &history.turns[0];
    for (input, text, status) in [
        (landed, "accepted steer", kar::SteerStatus::Received),
        (echoed, "echo before reply", kar::SteerStatus::Accepted),
        (refused, "refused steer", kar::SteerStatus::Refused),
    ] {
        assert_eq!(turn.steered_inputs.get(&input), Some(&status));
        let rows: Vec<_> = turn.items.iter().filter(|row| matches!(&row.item, Item::Message {role: Role::User,text:value,..} if value == text)).collect();
        assert_eq!(rows.len(), 1);
        assert!(
            matches!(&rows[0].item, Item::Message {provider_id:Some(id),..} if id == &input.to_string())
        );
    }
}

#[tokio::main]
async fn manual_compaction_replay() {
    use kar::chat::ChatReplica;
    use kar::{CompactionFailure, CompactionStatus, CompactionUnconfirmed};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["work", "native", "elsewhere"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let store = ChatStore::open(base.join("store")).unwrap();
    let mut opts = kar::SessionOptions::new(base.join("work"));
    let db = base.join("native");
    opts.env
        .insert("FIXTURE_NATIVE_DB".into(), db.clone().into());
    let chat = store
        .create("codex", Some(&std::env::current_exe().unwrap()), opts)
        .await
        .unwrap();
    let id = chat.info().id;
    let start = store.feed_snapshot(id).unwrap();
    let mut client = ChatReplica::from_snapshot(start);
    fs::write(db.join("compact-mode"), "after-item").unwrap();
    fs::write(db.join("compact-hold"), "").unwrap();
    let op = chat.start_compaction().await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while !matches!(op.latest().status, CompactionStatus::Accepted) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        op.wait(Some(Duration::ZERO)).await.status,
        CompactionStatus::Unconfirmed {
            code: CompactionUnconfirmed::Timeout
        }
    ));
    let cancelled = kar::CompactionOptions::default();
    cancelled.signal.cancel();
    assert!(matches!(
        op.wait_with(&cancelled).await.status,
        CompactionStatus::Unconfirmed {
            code: CompactionUnconfirmed::Signal
        }
    ));
    assert!(chat.snapshot().status.is_running());
    assert!(matches!(
        chat.start_compaction().await,
        Err(kar::Error::Busy)
    ));
    assert!(matches!(
        chat.send(Uuid::new_v4(), "reject while compacting").await,
        Err(kar::Error::Busy)
    ));
    assert!(store.history(id).unwrap().turns.is_empty());
    let input = Uuid::new_v4();
    chat.enqueue(input, "after compact").unwrap();
    assert!(!db.join("calls").exists());
    let mut other = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    assert!(other.manual_compaction().unwrap().status.is_pending());
    fs::remove_file(db.join("compact-hold")).unwrap();
    let report = op.wait(Some(Duration::from_secs(5))).await;
    assert!(matches!(report.status, CompactionStatus::Completed { .. }));
    assert_eq!(
        report.response.as_ref().unwrap().stream_id,
        report.request.stream_id
    );
    chat.drain_queue().await.unwrap();
    let tip = store.feed_snapshot(id).unwrap();
    for replica in [&mut client, &mut other] {
        let mut feed = store.follow(id, Some(replica.cursor())).unwrap();
        while replica.cursor() != tip.cursor {
            let event = tokio::time::timeout(Duration::from_secs(5), feed.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        assert_eq!(replica.manual_compaction(), Some(&report));
        assert_eq!(replica.turns().len(), 1);
        assert_eq!(replica.turns()[0].id, input);
    }
    for mode in ["missing", "abort", "refuse", "late-ack"] {
        fs::write(db.join("compact-mode"), mode).unwrap();
        let result = chat
            .start_compaction()
            .await
            .unwrap()
            .wait(Some(Duration::from_secs(5)))
            .await;
        match mode {
            "missing" => assert!(matches!(
                result.status,
                CompactionStatus::Failed {
                    code: CompactionFailure::NotCompacted,
                    ..
                }
            )),
            "abort" => assert!(matches!(result.status, CompactionStatus::Aborted { .. })),
            "refuse" => assert!(matches!(result.status, CompactionStatus::Rejected { .. })),
            _ => assert!(matches!(result.status, CompactionStatus::Completed { .. })),
        }
    }
    chat.close().await.unwrap();
    fs::remove_file(db.join("compact-mode")).unwrap();
    fs::write(db.join("compact-hold"), "").unwrap();
    let mut owner = cli(&base)
        .args(["compact", "--chat", &id.to_string()])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let snapshot = store.feed_snapshot(id).unwrap();
            if snapshot
                .history
                .manual_compaction
                .as_ref()
                .is_some_and(|r| matches!(r.status, CompactionStatus::Accepted))
            {
                break snapshot;
            }
            assert!(owner.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut a = store.follow(id, Some(snapshot.cursor)).unwrap();
    let mut b = store.follow(id, Some(snapshot.cursor)).unwrap();
    owner.kill().unwrap();
    owner.wait().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (left, right) = tokio::join!(a.recv(), b.recv());
            let (left, right) = (left.unwrap(), right.unwrap());
            assert_eq!(
                serde_json::to_value(&left).unwrap(),
                serde_json::to_value(right).unwrap()
            );
            if let kar::chat::ChatUpdate::ManualCompaction { report } = left.update {
                assert!(matches!(
                    report.status,
                    CompactionStatus::Unconfirmed {
                        code: CompactionUnconfirmed::ControllerExited
                    }
                ));
                break;
            }
        }
    })
    .await
    .unwrap();
    fs::remove_file(db.join("compact-hold")).unwrap();
    assert!(matches!(
        store.history(id).unwrap().manual_compaction.unwrap().status,
        CompactionStatus::Unconfirmed { .. }
    ));
    let output = success(
        cli(&base)
            .args(["compact", "--chat", &id.to_string()])
            .output()
            .unwrap(),
    );
    assert_eq!(output["status"]["kind"], "completed");
}

#[path = "support/cursor_children.rs"]
mod cursor_children;
#[path = "support/cursor_interactions.rs"]
mod cursor_interactions;
#[path = "support/workbench_fork.rs"]
mod workbench_fork;
#[path = "support/workbench_task_folders.rs"]
mod workbench_task_folders;
#[path = "support/workbench_projects.rs"]
mod workbench_projects;
#[path = "support/workbench_queue.rs"]
mod workbench_queue;
#[path = "support/workbench_requests.rs"]
mod workbench_requests;
#[path = "support/workbench_server.rs"]
mod workbench_server;

#[path = "support/worktree_chats.rs"]
mod worktree_chats;
#[path = "support/workbench_devices.rs"]
mod workbench_devices;
#[path = "support/workbench_cloud_live.rs"]
mod workbench_cloud_live;
#[path = "support/workbench_observability.rs"]
mod workbench_observability;
fn main() {
    let mode = std::env::args().nth(1);
    if matches!(mode.as_deref(), Some("workbench-simlock-live" | "workbench-expo-live")) {
        workbench_cloud_live::run(mode.as_deref() == Some("workbench-expo-live"));
        return;
    }
    if mode.as_deref() == Some("workbench-observability") {
        workbench_observability::run();
        return;
    }
    if matches!(mode.as_deref(),Some("workbench-devices"|"workbench-devices-live")){
        workbench_devices::run(mode.as_deref()==Some("workbench-devices-live"));
        return;
    }
    if std::env::var_os("KAR_DEVICE_TEST_DIR").is_some() && matches!(mode.as_deref(),Some("simctl"|"devices"|"-s"|"-T"|"-list-avds"|"--find"|"version"|"-version")){
        workbench_devices::peer(&std::env::args().skip(1).collect::<Vec<_>>());return;
    }
    if std::env::args().nth(1).as_deref() == Some("workbench-requests") {
        workbench_requests::run();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("workbench-fork") {
        workbench_fork::run();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("workbench-queue") {
        workbench_queue::run();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("fork-with-id") {
        worktree_chats::fork_with_id();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("workbench-task-folders") {
        workbench_task_folders::run();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("workbench-projects") {
        workbench_projects::run();
        workbench_task_folders::run();
        return;
    }
    if std::env::var_os("KAR_TEST_CURSOR_ACP").is_some() {
        cursor_children::peer();
        return;
    }
    if std::env::var_os("KAR_TEST_OPENCODE").is_some() {
        if std::env::args().nth(1).as_deref() == Some("--version") {
            println!("opencode 2.3.2");
        } else {
            opencode_peer();
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("hold-idle-form") {
        hold_idle_form(std::env::args_os().nth(2).unwrap().into());
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("codex 0.143.0");
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("app-server") {
        agent();
        return;
    }
    environment_across_controllers();
    restart();
    durable_inputs();
    codex_admission::reopen();
    live_feed();
    codex_steer_receipt_survives_reopen();
    manual_compaction_replay();
    background_child_replay();
    cursor_children::run();
    cursor_interactions::run();
    abandoned_idle_question();
    legacy_recording();
    checkpoints();
    linked_checkout();
    worktree_chats::run();
    coordinated_fork();
    fork_from_open_session();
    reopen_after_disconnected_writer_and_history_probe();
    println!(
        "multi-process recovery, message identity, replay, single writer, crash and torn-tail recovery passed"
    );
}
