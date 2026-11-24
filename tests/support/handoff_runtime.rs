//! Fixtures speak the native stdio protocols. In live mode the same launcher
//! forwards bytes to an installed executable and records only admitted user inputs.
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

fn directory() -> PathBuf {
    std::env::var_os("KAR_HANDOFF_FIXTURE").unwrap().into()
}
fn audit(v: Value) {
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(directory().join("inputs"))
        .unwrap();
    writeln!(f, "{v}").unwrap();
}
fn input<'a>(runtime: &str, v: &'a Value) -> Option<&'a str> {
    let content = match runtime {
        "codex" if v["method"] == "turn/start" => &v["params"]["input"],
        "qwen" | "claude" if v["type"] == "user" => &v["message"]["content"],
        _ => return None,
    };
    content.as_str().or_else(|| content[0]["text"].as_str())
}
fn proxy(runtime: &str, binary: &Path) -> ! {
    let mut command = Command::new(binary);
    if std::env::var("KAR_HOSTED_ACCEPTANCE_PROXY").as_deref() == Ok("1") {
        command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
    }
    let mut child = command
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let writer = child.stdin.take().unwrap();
    let name = runtime.to_owned();
    std::thread::spawn(move || {
        let mut writer = writer;
        let mut stdin = std::io::stdin().lock();
        let mut bytes = Vec::new();
        while {
            bytes.clear();
            stdin.read_until(b'\n', &mut bytes).unwrap_or(0) > 0
        } {
            let packet = serde_json::from_slice::<Value>(&bytes).ok();
            let admitted = packet.as_ref().and_then(|v| input(&name, v));
            if admitted.is_some_and(|text| text.contains("kar-acceptance-edit-notes")) {
                gate(&directory(), "document");
            }
            if writer
                .write_all(&bytes)
                .and_then(|_| writer.flush())
                .is_err()
            {
                break;
            }
            if let (Some(v), Some(text)) = (&packet, admitted) {
                audit(
                    json!({"runtime":name,"text":text,"cwd":std::env::current_dir().unwrap(),"uuid":v["uuid"],"session":v["params"]["threadId"].as_str().or(v["session_id"].as_str())}),
                );
            }
        }
    });
    std::process::exit(child.wait().unwrap().code().unwrap_or(1));
}
fn emit(v: Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{v}").unwrap();
    stdout.flush().unwrap();
}
fn gate(dir: &Path, name: &str) {
    let hold = dir.join(format!("hold-{name}"));
    if hold.is_file() {
        fs::write(dir.join(format!("entered-{name}")), []).unwrap();
        let start = Instant::now();
        while hold.is_file() {
            assert!(
                start.elapsed() < Duration::from_secs(25),
                "timed out at {name}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
fn fixture(runtime: &str) {
    let dir = directory();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!(
            "{}",
            if runtime == "claude" {
                "2.1.134"
            } else if runtime == "qwen" {
                "0.27.4"
            } else {
                "0.143.0"
            }
        );
        return;
    }
    if args.iter().any(|a| a == "--help") {
        println!("{runtime}");
        return;
    }
    let mut sid = args
        .windows(2)
        .find(|a| matches!(a[0].as_str(), "--session-id" | "--resume"))
        .map(|a| a[1].clone())
        .unwrap_or_default();
    let cwd = std::env::current_dir().unwrap();
    let mut stdin = BufReader::new(std::io::stdin().lock());
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let v: Value = serde_json::from_str(&line).unwrap();
        if runtime == "codex" {
            let reply = |r| emit(json!({"id":v["id"],"result":r}));
            match v["method"].as_str() {
                Some("initialize") => reply(json!({})),
                Some("initialized") => {}
                Some("thread/start" | "thread/resume") => {
                    if v["method"] == "thread/start" {
                        sid = Uuid::new_v4().to_string();
                        fs::write(dir.join(&sid), cwd.as_os_str().as_encoded_bytes()).unwrap();
                    } else {
                        sid = v["params"]["threadId"].as_str().unwrap().to_owned();
                        assert_eq!(
                            fs::read(dir.join(&sid)).unwrap(),
                            cwd.as_os_str().as_encoded_bytes()
                        );
                    }
                    let mut info = json!({"thread":{"id":sid,"cwd":cwd,"turns":[]}});
                    if args.iter().any(|a| a == "sandbox_mode=\"workspace-write\"") {
                        info["cwd"] = json!(cwd);
                        info["approvalPolicy"] = json!("never");
                        info["sandbox"] = json!({"type":"workspaceWrite","writableRoots":[],"excludeSlashTmp":true,"excludeTmpdirEnvVar":true,"networkAccess":false});
                    }
                    reply(info);
                }
                Some("thread/fork") => {
                    let source = v["params"]["threadId"].as_str().unwrap();
                    assert_eq!(
                        fs::read(dir.join(source)).unwrap(),
                        cwd.as_os_str().as_encoded_bytes()
                    );
                    assert_eq!(v["params"]["cwd"], serde_json::json!(cwd));
                    let anchor = v["params"]["lastTurnId"].as_str().unwrap();
                    let turns = fs::read_to_string(dir.join(format!("{source}.turns"))).unwrap();
                    let mut prefix = Vec::new();
                    for turn in turns.lines() {
                        prefix.push(json!({"id":turn}));
                        if turn == anchor {
                            break;
                        }
                    }
                    assert_eq!(prefix.last().unwrap()["id"], anchor);
                    sid = Uuid::new_v4().to_string();
                    fs::write(dir.join(&sid), cwd.as_os_str().as_encoded_bytes()).unwrap();
                    fs::write(
                        dir.join(format!("{sid}.turns")),
                        prefix
                            .iter()
                            .map(|t| t["id"].as_str().unwrap())
                            .collect::<Vec<_>>()
                            .join("\n")
                            + "\n",
                    )
                    .unwrap();
                    reply(json!({"thread":{"id":sid,"cwd":cwd,"turns":prefix},"cwd":cwd}));
                }
                Some("turn/start") => {
                    let text = input(runtime, &v).unwrap();
                    audit(json!({"runtime":runtime,"session":sid,"text":text,"cwd":cwd}));
                    let turn = Uuid::new_v4();
                    writeln!(
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join(format!("{sid}.turns")))
                            .unwrap(),
                        "{turn}"
                    )
                    .unwrap();
                    reply(json!({"turn":{"id":turn}}));
                    emit(
                        json!({"method":"turn/started","params":{"threadId":sid,"turn":{"id":turn}}}),
                    );
                    gate(&dir, "turn");
                    if text.contains("kar-acceptance-edit-notes") {
                        let original = fs::read_to_string("notes.md").unwrap();
                        assert!(original.contains("author=original"));
                        gate(&dir, "document");
                        fs::write(
                            "notes.md",
                            original.replace("author=original", "author=assistant"),
                        )
                        .unwrap();
                    }
                    if text == "collect evidence" {
                        let marker = Uuid::new_v4();
                        fs::write(dir.join("side-effect"), marker.to_string()).unwrap();
                        let tool = |method, status| json!({"method":method,"params":{"threadId":sid,"turnId":turn,"item":{"type":"commandExecution","id":"read-proof","command":"fixture-once","status":status,"aggregatedOutput":format!("tool-proof-{marker}"),"exitCode":0}}});
                        emit(tool("item/started", "inProgress"));
                        emit(tool("item/completed", "completed"));
                    }
                    emit(
                        json!({"method":"item/completed","params":{"threadId":sid,"turnId":turn,"item":{"type":"agentMessage","id":format!("answer-{turn}"),"text":"source-answer"}}}),
                    );
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":sid,"turn":{"id":turn,"status":"completed"}}}),
                    );
                }
                Some(method) => panic!("unexpected {method}"),
                None => {}
            }
        } else if v["type"] == "control_request" {
            if v["request"]["subtype"] == "stop_task" {
                assert_eq!(v["request"]["task_id"], "job");
                audit(json!({"runtime":runtime,"session":sid,"stopped":"job"}));
                emit(
                    json!({"type":"system","subtype":"task_notification","task_id":"job","session_id":sid,"status":"stopped"}),
                );
            }
            let initialization = v["request"]["subtype"] == "initialize";
            if initialization {
                assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
                gate(&dir, "open");
                if dir.join("refuse-open").is_file() {
                    emit(
                        json!({"type":"control_response","response":{"subtype":"error","request_id":v["request_id"],"error":"fixture rejected target startup"}}),
                    );
                    continue;
                }
                if args.iter().any(|a| a == "--resume") {
                    assert_eq!(
                        fs::read(dir.join(&sid)).unwrap(),
                        cwd.as_os_str().as_encoded_bytes()
                    );
                } else {
                    fs::write(dir.join(&sid), cwd.as_os_str().as_encoded_bytes()).unwrap();
                }
            }
            emit(
                json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":if initialization {json!({"session_id":sid})} else {json!({})}}}),
            );
        } else if let Some(text) = input(runtime, &v) {
            audit(json!({"runtime":runtime,"session":sid,"text":text,"cwd":cwd,"uuid":v["uuid"]}));
            if runtime == "claude" && text == "start native workflow" {
                emit(
                    json!({"type":"system","subtype":"task_started","task_id":"job","task_type":"local_workflow","session_id":sid,"is_backgrounded":true}),
                );
            }
            let reply = if ["fact-one", "fact-two", "tool-proof-"]
                .iter()
                .all(|fact| text.contains(fact))
            {
                "received both facts and proof"
            } else {
                "continued"
            };
            emit(
                json!({"type":"assistant","session_id":sid,"uuid":Uuid::new_v4(),"message":{"id":Uuid::new_v4(),"role":"assistant","content":[{"type":"text","text":reply}]}}),
            );
            emit(
                json!({"type":"result","session_id":sid,"uuid":Uuid::new_v4(),"user_message_uuid":v["uuid"],"subtype":"success","is_error":false,"result":reply}),
            );
        }
    }
}
pub fn child() -> bool {
    let Some(path) = std::env::args_os().next().map(PathBuf::from) else {
        return false;
    };
    let Some(runtime @ ("codex" | "qwen" | "claude")) = path.file_stem().and_then(|s| s.to_str())
    else {
        return false;
    };
    assert!(std::env::var_os("KAR_HANDOFF_FIXTURE").is_some());
    if let Some(real) =
        std::env::var_os(format!("KAR_HANDOFF_REAL_{}", runtime.to_ascii_uppercase()))
            .filter(|s| !s.is_empty())
    {
        proxy(runtime, Path::new(&real));
    }
    fixture(runtime);
    true
}
