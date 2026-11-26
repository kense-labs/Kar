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
    if std::env::var_os("KAR_TEST_CURSOR_BACKGROUND").is_some() {
        cursor_peer();
        return;
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("2.1.330 (Claude Code)");
        return;
    }
    if let Some(config) = args.windows(2).find(|v| v[0] == "--mcp-config") {
        let _ = fs::read(&config[1]);
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

    #[cfg(target_os = "macos")]
    cursor_scenario().await;
}

#[cfg(not(target_os = "macos"))]
fn cursor_peer() {}

#[cfg(target_os = "macos")]
fn cursor_peer() {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help") {
        println!("Usage: agent acp");
        return;
    }
    if args.iter().any(|a| a == "--version") {
        println!("2026.11.22-e9042b2");
        return;
    }
    let cursor_dir = PathBuf::from(std::env::var_os("CURSOR_DATA_DIR").unwrap());
    let record_dir = PathBuf::from(std::env::var_os("KAR_TEST_CURSOR_DIR").unwrap());
    let mut session = String::new();

    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).unwrap();
        let reply = |result: Value| emit(json!({"jsonrpc":"2.0","id":v["id"],"result":result}));
        match v["method"].as_str() {
            Some("initialize") => reply(json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "sessionCapabilities": {"list": {}, "subagents": {}}
                }
            })),
            Some("cursor/list_available_models") => {
                emit(json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "error": {"code": -32601, "message": "Method not found"}
                }));
            }
            Some("session/new") => {
                session = format!("cursor-{}", Uuid::new_v4());
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(record_dir.join("sessions.txt"))
                    .unwrap();
                writeln!(f, "{session}").unwrap();
                reply(json!({"sessionId": session}));
            }
            Some("session/list") => {
                let cwd = std::env::current_dir().unwrap();
                let list: Vec<Value> = fs::read_to_string(record_dir.join("sessions.txt"))
                    .unwrap_or_default()
                    .lines()
                    .filter(|s| !s.is_empty())
                    .map(|s| json!({"sessionId": s, "cwd": cwd}))
                    .collect();
                reply(json!({"sessions": list}));
            }
            Some("session/load") => {
                session = v["params"]["sessionId"].as_str().unwrap().to_owned();
                reply(json!({"sessionId": session, "configOptions": []}));
            }
            Some("session/set_config_option") => {
                let key = v["params"]["configId"].as_str().unwrap_or("mode");
                let val = v["params"]["value"].as_str().unwrap_or("ask");
                reply(json!({"configOptions": [{"id": key, "currentValue": val}]}));
            }
            Some("session/set_mode") => {
                reply(json!({}));
            }
            Some("session/prompt") => {
                let text = v["params"]["prompt"][0]["text"].as_str().unwrap();
                if let Some(rest) = text.strip_prefix("spawn:") {
                    let (shell_id, tag) = rest.split_once(':').unwrap();

                    let call_id = format!("call-{tag}");

                    emit(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session,
                            "update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": call_id,
                                "title": "Shell",
                                "kind": "execute",
                                "status": "in_progress",
                                "rawInput": {"command": "trap '' HUP; sleep 30"}
                            }
                        }
                    }));

                    let mut child = Command::new("sh")
                        .args(["-c", "trap '' HUP; sleep 30"])
                        .process_group(0)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    let pid = child.id();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(60));
                        let _ = child.wait();
                    });

                    // Write terminal metadata on Session 1 launches; Session 2 intentionally omits
                    // PID in text while Session 1's 1.txt is on disk, verifying Session 2 rejects
                    // Session 1's PID and discovers its own detached child group leader!
                    if tag != "s2_first" {
                        let terminals = cursor_dir.join("projects/demo/terminals");
                        fs::create_dir_all(&terminals).unwrap();
                        fs::write(
                            terminals.join(format!("{shell_id}.txt")),
                            format!(
                                "---\npid: {pid}\nstatus: running\nstarted_at: 2026-11-24T22:00:00Z\ncommand: trap '' HUP; sleep 30\n---\n"
                            ),
                        )
                        .unwrap();
                    }

                    let mut pids = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(record_dir.join("pids.jsonl"))
                        .unwrap();
                    writeln!(
                        pids,
                        "{}",
                        json!({"tag": tag, "session": session, "shell_id": shell_id, "pid": pid})
                    )
                    .unwrap();

                    let receipt_text = if tag == "s2_first" {
                        format!("Command was sent to the background.\nShell ID: {shell_id}")
                    } else {
                        format!("Background command started.\nShell ID: {shell_id}\nPID: {pid}")
                    };
                    emit(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session,
                            "update": {
                                "sessionUpdate": "tool_call_update",
                                "toolCallId": call_id,
                                "status": "completed",
                                "content": [{
                                    "type": "content",
                                    "content": {"type": "text", "text": receipt_text}
                                }]
                            }
                        }
                    }));
                    emit(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": "background shell launched"}
                            }
                        }
                    }));
                }
                reply(json!({"stopReason": "end_turn"}));
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "macos")]
fn os_process_alive(pid: u32) -> bool {
    use libproc::{proc_pid::pidinfo, task_info::TaskAllInfo};
    pidinfo::<TaskAllInfo>(pid as i32, 0).is_ok_and(|info| info.pbsd.pbi_status != nix::libc::SZOMB)
}

#[cfg(target_os = "macos")]
async fn cursor_scenario() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let state = temp.path().join("state");
    let native = temp.path().join("native");
    let cursor_data = temp.path().join("cursor-data");
    let record_dir = temp.path().join("cursor-records");
    for d in [&work, &native, &cursor_data, &record_dir] {
        fs::create_dir(d).unwrap();
    }

    let bin = std::env::current_exe().unwrap();
    let env = [
        ("KAR_CURSOR_BIN", bin.as_os_str()),
        ("KAR_TEST_CLAUDE_BACKGROUND", OsStr::new("1")),
        ("KAR_TEST_CURSOR_BACKGROUND", OsStr::new("1")),
        ("CURSOR_DATA_DIR", cursor_data.as_os_str()),
        ("KAR_TEST_CURSOR_DIR", record_dir.as_os_str()),
    ];
    let (mut server, base) = serve_with_env(&state, &work, &native, &env).await;
    let a = client();
    let b = client();
    let p = ok(a
        .post(format!("{base}/projects"))
        .json(&json!({"name": "cursor-background", "root": work})))
    .await;
    let chats = format!("{base}/projects/{}/chats", p["id"].as_str().unwrap());

    let c1 = Uuid::new_v4();
    let c2 = Uuid::new_v4();
    ok(a.post(&chats).json(&json!({"id": c1, "runtime": "cursor"}))).await;
    ok(b.post(&chats).json(&json!({"id": c2, "runtime": "cursor"}))).await;
    let chat1 = format!("{chats}/{c1}");
    let chat2 = format!("{chats}/{c2}");

    // 1. Both Cursor sessions spawn a real detached `sh -c 'trap "" HUP; sleep 30'` process group
    // and finish their foreground turns.
    let turn1 = send(&a, &chat1, "spawn:1:s1_first").await;
    let turn2 = send(&b, &chat2, "spawn:1:s2_first").await;

    let find_pid = |tag: &str| -> u32 {
        fs::read_to_string(record_dir.join("pids.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["tag"] == tag)
            .and_then(|v| v["pid"].as_u64())
            .unwrap() as u32
    };
    let pid1 = find_pid("s1_first");
    let pid2 = find_pid("s2_first");
    assert_ne!(pid1, pid2);
    assert!(os_process_alive(pid1));
    assert!(os_process_alive(pid2));

    // 2. Verify GET .../background-tasks shows each session's running command after foreground turn ended.
    let tasks1 = ok(a.get(format!("{chat1}/background-tasks"))).await;
    let tasks2 = ok(b.get(format!("{chat2}/background-tasks"))).await;
    assert_eq!(
        tasks1["pending"].as_array().unwrap().len(),
        1,
        "tasks1={tasks1} tasks2={tasks2}"
    );
    assert_eq!(
        tasks2["pending"].as_array().unwrap().len(),
        1,
        "tasks1={tasks1} tasks2={tasks2}"
    );
    let row1 = row(&tasks1, "1");
    let row2 = row(&tasks2, "1");
    assert_eq!(row1["background_work"]["turn_id"], turn1.to_string());
    assert_eq!(row2["background_work"]["turn_id"], turn2.to_string());
    assert_eq!(row1["background_work"]["command"], "trap '' HUP; sleep 30");
    assert_eq!(row2["background_work"]["command"], "trap '' HUP; sleep 30");
    let start1 = row1["started_at"].clone();
    let start2 = row2["started_at"].clone();

    // Mark wait on Session 1's background shell.
    let waited = ok(a
        .post(format!("{chat1}/background-tasks/await"))
        .json(&json!({"task_ids": ["1"], "started_at": {"1": start1}})))
    .await;
    assert_eq!(waited["matched"], json!(["1"]));
    assert_eq!(
        ok(a.get(format!("{chat1}/background-tasks"))).await["background_working"],
        true
    );

    // Stop Session 1's background shell with started_at -> kills Session 1's real OS process,
    // while Session 2's real OS process remains untouched and running!
    let stop_resp = a
        .post(format!("{chat1}/background-tasks/1/stop"))
        .json(&json!({"started_at": start1}))
        .send()
        .await
        .unwrap();
    assert_eq!(stop_resp.status(), reqwest::StatusCode::NO_CONTENT);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let t1 = ok(a.get(format!("{chat1}/background-tasks"))).await;
            if !os_process_alive(pid1) && t1["pending"].as_array().unwrap().is_empty() {
                assert_eq!(t1["background_working"], false);
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();

    assert!(
        os_process_alive(pid2),
        "stopping Session 1's shell must not affect Session 2's shell"
    );
    assert_eq!(
        ok(b.get(format!("{chat2}/background-tasks"))).await["pending"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // 3. Reusing the same shell_id ("1") in a second turn of Session 1 creates a new started_at
    // occurrence, preserves the first stopped occurrence in tasks, and rejects stopping with old started_at!
    let turn1_second = send(&a, &chat1, "spawn:1:s1_reused").await;
    let pid1_reused = find_pid("s1_reused");
    assert!(os_process_alive(pid1_reused));

    let tasks1_reused = ok(a.get(format!("{chat1}/background-tasks"))).await;
    assert_eq!(tasks1_reused["pending"].as_array().unwrap().len(), 1);
    let reused_row = row(&tasks1_reused, "1");
    let start1_reused = reused_row["started_at"].clone();
    assert_ne!(start1_reused, start1);
    assert_eq!(
        reused_row["background_work"]["turn_id"],
        turn1_second.to_string()
    );
    assert_eq!(reused_row["background_work"]["awaited"], false);

    let snap1 = ok(a.get(format!("{chat1}/snapshot"))).await;
    let all_shells: Vec<&Value> = snap1["history"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["id"] == "1")
        .collect();
    assert_eq!(all_shells.len(), 2);
    assert_eq!(all_shells[0]["started_at"], start1);
    assert_eq!(all_shells[0]["status"], "stopped");
    assert_eq!(all_shells[1]["started_at"], start1_reused);
    assert_eq!(all_shells[1]["status"], "running");

    // Stopping the new occurrence with the stale first_start cursor is rejected!
    let stale_stop = a
        .post(format!("{chat1}/background-tasks/1/stop"))
        .json(&json!({"started_at": start1}))
        .send()
        .await
        .unwrap();
    assert!(!stale_stop.status().is_success());
    assert!(os_process_alive(pid1_reused));

    // Clean up both live background shells (Session 1 reused + Session 2).
    for (url, cursor, pid) in [
        (
            format!("{chat1}/background-tasks/1/stop"),
            start1_reused,
            pid1_reused,
        ),
        (format!("{chat2}/background-tasks/1/stop"), start2, pid2),
    ] {
        let resp = a
            .post(url)
            .json(&json!({"started_at": cursor}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        tokio::time::timeout(Duration::from_secs(5), async {
            while os_process_alive(pid) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    }

    server.kill().await.unwrap();
}
