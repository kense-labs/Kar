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
        "qwen" if v["type"] == "user" => &v["message"]["content"],
        _ => return None,
    };
    content.as_str().or_else(|| content[0]["text"].as_str())
}
fn proxy(runtime: &str, binary: &Path) -> ! {
    let mut child = Command::new(binary)
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
            if writer
                .write_all(&bytes)
                .and_then(|_| writer.flush())
                .is_err()
            {
                break;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes)
                && let Some(text) = input(&name, &v)
            {
                audit(json!({"runtime":name,"text":text,"cwd":std::env::current_dir().unwrap()}));
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
            if runtime == "qwen" {
                "0.27.4"
            } else {
                "0.130.0"
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
                    reply(json!({"thread":{"id":sid,"cwd":cwd,"turns":[]}}));
                }
                Some("turn/start") => {
                    let text = input(runtime, &v).unwrap();
                    audit(json!({"runtime":runtime,"session":sid,"text":text,"cwd":cwd}));
                    let turn = Uuid::new_v4();
                    reply(json!({"turn":{"id":turn}}));
                    emit(
                        json!({"method":"turn/started","params":{"threadId":sid,"turn":{"id":turn}}}),
                    );
                    gate(&dir, "turn");
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
    let Some(runtime @ ("codex" | "qwen")) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    assert!(std::env::var_os("KAR_HANDOFF_FIXTURE").is_some());
    if let Some(real) =
        std::env::var_os(format!("KAR_HANDOFF_REAL_{}", runtime.to_ascii_uppercase()))
    {
        proxy(runtime, Path::new(&real));
    }
    fixture(runtime);
    true
}
