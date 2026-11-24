use super::{
    codex_chat_peer::emit,
    sse::SseClient,
    workbench_server::{client, json as ok, serve_with_env},
};
use kar::chat::{ChatFeedSnapshot, ChatReplica};
use reqwest::Client;
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    io::{BufRead, Write},
    time::Duration,
};
use uuid::Uuid;

/// Scripted Claude wire behavior, exercised only by the isolated workbench fixture.
pub(crate) fn peer() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("2.1.330 (Claude Code)");
        return;
    }
    let session = &args
        .windows(2)
        .find(|v| matches!(v[0].as_str(), "--session-id" | "--resume"))
        .unwrap()[1];
    let frame = |mut value: Value| {
        value["session_id"] = session.as_str().into();
        value["uuid"] = Uuid::new_v4().to_string().into();
        emit(value);
    };
    let start = |id: &str, tool: &str, background: bool| {
        frame(
            json!({"type":"system","subtype":"task_started","task_type":"local_bash","task_id":id,"tool_use_id":tool,"is_backgrounded":background}),
        )
    };
    let tool = |id: &str, name: &str, command: &str| {
        frame(
            json!({"type":"assistant","message":{"id":id,"role":"assistant","content":[{"type":"tool_use","name":name,"id":id,"input":{"command":command}}]}}),
        )
    };
    let roster = |ids: &[&str]| {
        let mut list = ids
            .iter()
            .map(|id| json!({"task_type":"local_bash","task_id":id}))
            .collect::<Vec<_>>();
        list.push(json!({"task_type":"local_agent","task_id":"not-shell"}));
        frame(json!({"type":"system","subtype":"background_tasks_changed","tasks":list}));
    };
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).unwrap();
        match v["type"].as_str() {
            Some("control_request") => {
                let result = if v["request"]["subtype"] == "get_status" {
                    json!({"sections":[{"rows":[{"label":"Session ID","value":session}]}]})
                } else {
                    json!({"commands":[],"models":[]})
                };
                emit(
                    json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":result}}),
                );
            }
            Some("user") => {
                let input = v["message"]["content"]
                    .as_str()
                    .or_else(|| v["message"]["content"][0]["text"].as_str())
                    .unwrap();
                let file = std::path::PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap())
                    .join("prompts.jsonl");
                let mut journal = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(file)
                    .unwrap();
                writeln!(journal, "{}", json!({"id":v["uuid"],"text":input})).unwrap();
                let action = if input.starts_with("Background work stopped") {
                    input
                        .split_once("\n\n")
                        .map_or("automatic recovery", |(_, user)| user)
                } else {
                    input
                };
                let home = std::path::PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap());
                if action == "automatic recovery" && home.join("hold-auto").exists() {
                    fs::write(home.join("received-auto"), []).unwrap();
                    let until = std::time::Instant::now() + Duration::from_secs(25);
                    while home.join("hold-auto").exists() {
                        assert!(std::time::Instant::now() < until, "recovery gate timed out");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                match action {
                    "first" => {
                        tool("tool.build", "Bash", "make test");
                        start("build", "tool.build", true);
                        start("watch", "tool.watch", true);
                        tool("tool.watch", "Monitor", "watch status");
                        tool("tool.foreground", "Bash", "make all");
                        start("foreground", "tool.foreground", false);
                    }
                    "reorder" => roster(&["watch", "foreground", "build"]),
                    "reuse" => {
                        roster(&["watch", "foreground"]);
                        start("build", "tool.reused", true);
                        frame(
                            json!({"type":"system","subtype":"task_notification","task_id":"build","tool_use_id":"tool.build","status":"completed"}),
                        );
                        frame(
                            json!({"type":"system","subtype":"task_notification","task_id":"foreground"}),
                        );
                    }
                    "new process" => start("build", "tool.after-restart", true),
                    "exit after result" => start("detached", "tool.exit", true),
                    "start then close" => start("closed", "tool.close", true),
                    "automatic recovery" | "check progress" | "after explicit close" => {}
                    _ => panic!("unexpected prompt {input}"),
                }
                frame(
                    json!({"type":"result","subtype":"success","user_message_uuid":v["uuid"],"is_error":false,"result":"done"}),
                );
                if action == "exit after result" {
                    return;
                }
            }
            _ => {}
        }
    }
}

async fn send(client: &Client, chat: &str, text: &str) -> Uuid {
    let id = Uuid::new_v4();
    let r = ok(client
        .post(format!("{chat}/messages"))
        .json(&json!({"id":id,"text":text})))
    .await;
    assert_eq!(r["status"], "completed");
    id
}
fn row<'a>(roster: &'a Value, name: &str) -> &'a Value {
    roster["pending"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == name)
        .unwrap()
}
async fn error(client: &Client, url: &str, body: Value, expected: &str) {
    let r = client.post(url).json(&body).send().await.unwrap();
    assert!(!r.status().is_success());
    assert_eq!(r.json::<Value>().await.unwrap()["reason"], expected);
}
#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(90), scenario())
        .await
        .unwrap();
}
async fn scenario() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let state = temp.path().join("state");
    let native = temp.path().join("native");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&native).unwrap();
    let bin = std::env::current_exe().unwrap();
    let env = [
        ("KAR_CLAUDE_BIN", bin.as_os_str()),
        ("KAR_TEST_CLAUDE_BACKGROUND", OsStr::new("1")),
        ("CLAUDE_CONFIG_DIR", native.as_os_str()),
        ("HOME", native.as_os_str()),
    ];
    let (mut server, base) = serve_with_env(&state, &work, &native, &env).await;
    let a = client();
    let b = client();
    let p = ok(a
        .post(format!("{base}/projects"))
        .json(&json!({"name":"background","root":work})))
    .await;
    let chats = format!("/projects/{}/chats", p["id"].as_str().unwrap());
    let id = Uuid::new_v4();
    ok(a.post(format!("{base}{chats}"))
        .json(&json!({"id":id,"runtime":"claude"})))
    .await;
    let path = format!("{chats}/{id}");
    let chat = format!("{base}{path}");
    let tasks = format!("{chat}/background-tasks");
    let mark = format!("{tasks}/await");
    let first = send(&a, &chat, "first").await;
    let r = ok(b.get(&tasks)).await;
    assert_eq!(r["pending"].as_array().unwrap().len(), 2);
    assert_eq!(row(&r, "watch")["background_work"]["kind"], "monitor");
    assert_eq!(row(&r, "watch")["background_work"]["awaited"], true);
    assert_eq!(
        row(&r, "build")["background_work"]["turn_id"],
        first.to_string()
    );
    assert_eq!(row(&r, "build")["background_work"]["awaited"], false);
    assert_eq!(r["background_working"], true);
    let first_start = row(&r, "build")["started_at"].clone();
    let snapshot = ok(a.get(format!("{chat}/snapshot"))).await;
    let seed: ChatFeedSnapshot = serde_json::from_value(snapshot.clone()).unwrap();
    let cursor = seed.cursor.to_string();
    let mut left = ChatReplica::from_snapshot(seed);
    let mut right = ChatReplica::from_snapshot(serde_json::from_value(snapshot).unwrap());
    let mut stream_a = SseClient::open_with_client(&a, &chat, &cursor, None).await;
    let mut stream_b = SseClient::open_with_client(&b, &chat, &cursor, Some(&cursor)).await;
    let off = ok(b
        .post(&mark)
        .json(&json!({"task_ids":["watch"],"waiting":false})))
    .await;
    assert_eq!(off["matched"], json!(["watch"]));
    for (view, feed) in [(&mut left, &mut stream_a), (&mut right, &mut stream_b)] {
        while view.background_working() {
            view.apply(feed.next().await).unwrap();
        }
    }
    assert_eq!(left.cursor(), right.cursor());
    drop(stream_b);
    let tip = right.cursor().to_string();
    let on=ok(a.post(&mark).json(&json!({"task_ids":["missing","build","build","missing"],"started_at":{"build":first_start}}))).await;
    assert_eq!(
        on,
        json!({"matched":["build"],"unmatched":["missing","missing"]})
    );
    stream_b = SseClient::open_with_client(&b, &chat, &cursor, Some(&tip)).await;
    for (view, feed) in [(&mut left, &mut stream_a), (&mut right, &mut stream_b)] {
        while !view.background_working() {
            view.apply(feed.next().await).unwrap();
        }
    }
    assert_eq!(left.cursor(), right.cursor());
    error(&a, &mark, json!({"task_ids":["not-shell"]}), "unmatched").await;
    assert_eq!(
        ok(a.post(&mark)
            .json(&json!({"task_ids":["none","none"],"waiting":false})))
        .await,
        json!({"matched":[],"unmatched":["none","none"]})
    );
    send(&b, &chat, "reorder").await;
    let roster = ok(a.get(&tasks)).await;
    assert_eq!(roster["pending"][0]["id"], "watch");
    assert_eq!(roster["pending"][1]["id"], "foreground");
    assert_eq!(row(&roster, "watch")["background_work"]["awaited"], false);
    assert_eq!(row(&roster, "build")["started_at"], first_start);
    assert_eq!(
        row(&roster, "build")["background_work"]["turn_id"],
        first.to_string()
    );
    assert_eq!(
        row(&roster, "foreground")["background_work"]["command"],
        "make all"
    );
    let before_restart = send(&a, &chat, "reuse").await;
    let r = ok(b.get(&tasks)).await;
    assert_eq!(r["pending"].as_array().unwrap().len(), 2);
    let retained = row(&r, "build");
    let old = retained["started_at"].clone();
    assert_ne!(old, first_start);
    assert_eq!(retained["background_work"]["awaited"], false);
    assert_eq!(
        retained["background_work"]["turn_id"],
        before_restart.to_string()
    );
    error(
        &b,
        &mark,
        json!({"task_ids":["build"],"started_at":{"build":first_start}}),
        "unmatched",
    )
    .await;
    ok(b.post(&mark)
        .json(&json!({"task_ids":["build"],"started_at":{"build":old}})))
    .await;
    drop((stream_a, stream_b));
    server.kill().await.unwrap();
    let (mut server, base) = serve_with_env(&state, &work, &native, &env).await;
    let chat = format!("{base}{path}");
    let tasks = format!("{chat}/background-tasks");
    let mark = format!("{tasks}/await");
    let r = ok(b.get(&tasks)).await;
    assert_eq!(r["pending"], json!([]));
    assert_eq!(r["background_working"], false);
    assert_eq!(r["interrupted_turns"], json!([before_restart]));
    assert!(
        r["lost"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == "build"
                && t["background_work"]["turn_id"] == before_restart.to_string()
                && t["started_at"] == old)
    );
    let prompts = || -> Vec<Value> {
        fs::read_to_string(native.join("prompts.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    // With an explicit wait mark the service reattaches, even without a queue file or user input.
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let h = ok(a.get(format!("{chat}/snapshot"))).await;
            if h["history"]["turns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|turn| {
                    turn["background_note"]["automatic"] == true && turn["status"] == "completed"
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        prompts()
            .iter()
            .filter(|p| p["text"]
                .as_str()
                .unwrap()
                .starts_with("Background work stopped"))
            .count(),
        1
    );
    error(&b, &mark, json!({"task_ids":["build"]}), "unmatched").await;
    let new = send(&b, &chat, "new process").await;
    let r = ok(a.get(&tasks)).await;
    assert_eq!(
        row(&r, "build")["background_work"]["turn_id"],
        new.to_string()
    );
    assert_ne!(row(&r, "build")["started_at"], old);
    assert_eq!(r["interrupted_turns"], json!([before_restart]));
    error(
        &a,
        &mark,
        json!({"task_ids":["build"],"started_at":{"build":old}}),
        "unmatched",
    )
    .await;
    // An ordinary unawaited shell produces a note only with the next actual input.
    server.kill().await.unwrap();
    let (mut server, base) = serve_with_env(&state, &work, &native, &env).await;
    let chat = format!("{base}{path}");
    let admission = Uuid::new_v4();
    let input = json!({"id":admission,"text":"check progress"});
    let recovered = ok(b.post(format!("{chat}/messages")).json(&input)).await;
    assert_eq!(recovered["items"][0]["text"], "check progress");
    assert_eq!(recovered["background_note"]["automatic"], false);
    let count = prompts().len();
    let last = prompts().last().unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        last.contains("Kar restarted")
            && last.contains("build")
            && last.ends_with("check progress")
    );
    assert_eq!(
        ok(a.post(format!("{chat}/messages")).json(&input)).await,
        recovered
    );
    assert_eq!(count, prompts().len());
    // A provider can exit after a successful foreground reply while Kar stays up.
    send(&a, &chat, "exit after result").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let tasks = ok(a.get(format!("{chat}/background-tasks"))).await;
            if tasks["lost"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == "detached" && t["background_exit"] == "provider_exit")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    send(&a, &chat, "check progress").await;
    assert!(
        prompts().last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("provider process exited")
    );
    let turn = send(&a, &chat, "start then close").await;
    ok(a.post(format!("{chat}/background-tasks/await"))
        .json(&json!({"task_ids":["closed"]})))
    .await;
    ok(a.post(format!("{chat}/force-stop"))
        .json(&json!({"turn":turn})))
    .await;
    send(&a, &chat, "after explicit close").await;
    assert_eq!(prompts().last().unwrap()["text"], "after explicit close");
    // Interrupt between admission and the model result. The original stream may
    // have reached the vendor; neither a restart nor a later user input resends it.
    send(&a, &chat, "new process").await;
    ok(a.post(format!("{chat}/background-tasks/await"))
        .json(&json!({"task_ids":["build"]})))
    .await;
    server.kill().await.unwrap();
    fs::write(native.join("hold-auto"), []).unwrap();
    let (mut server, _) = serve_with_env(&state, &work, &native, &env).await;
    tokio::time::timeout(Duration::from_secs(25), async {
        while !native.join("received-auto").exists() {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    server.kill().await.unwrap();
    fs::remove_file(native.join("hold-auto")).unwrap();
    let admitted = prompts().len();
    let (mut server, base) = serve_with_env(&state, &work, &native, &env).await;
    let chat = format!("{base}{path}");
    send(&a, &chat, "check progress").await;
    assert_eq!(prompts().len(), admitted + 1);
    assert_eq!(prompts().last().unwrap()["text"], "check progress");
    server.kill().await.unwrap();
}
