//! Real Kar CLI and persistence, native protocols replaced with recording peers.
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    path::Path,
    process::Command,
};

fn emit(value: Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn acp(id: &str, args: &[String]) {
    let expected = if id == "kiro" {
        &["acp", "--agent-engine=v3", "--auth-method=cli"][..]
    } else {
        &["acp"][..]
    };
    assert_eq!(args, expected);
    let session = format!("native-{id}");
    let mut configured = false;
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let params = &request["params"];
        let value = match request["method"].as_str().unwrap() {
            "initialize" => json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true}}),
            "session/new" | "session/load" => {
                assert_eq!(
                    params["cwd"].as_str(),
                    std::env::current_dir().unwrap().to_str()
                );
                if id == "fx" {
                    assert_eq!(params["_meta"]["fx"]["profileMcpServers"], true);
                }
                if request["method"] == "session/load" {
                    assert_eq!(params["sessionId"], session);
                }
                json!({"sessionId":session,"configOptions":[]})
            }
            "session/set_config_option" => {
                let (key, value) = if id == "kiro" {
                    ("autopilot", "off")
                } else {
                    ("mode", "ask")
                };
                assert_eq!(params["sessionId"], session);
                assert_eq!(
                    (params["configId"].as_str(), params["value"].as_str()),
                    (Some(key), Some(value))
                );
                configured = true;
                json!({"configOptions":[{"id":key,"currentValue":value}]})
            }
            "session/prompt" => {
                assert!(configured);
                let message = params["prompt"][0]["text"].as_str().unwrap();
                emit(
                    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("{id} heard {message}")}}}}),
                );
                json!({"stopReason":"end_turn"})
            }
            other => panic!("unexpected {id} method {other}"),
        };
        emit(json!({"jsonrpc":"2.0","id":request["id"],"result":value}));
    }
}

fn cli(id: &str, args: &[&str], root: &Path, store: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["chat", "--store"])
        .arg(store)
        .args(args)
        .current_dir(root)
        .env(
            format!("KAR_{}_BIN", id.to_uppercase()),
            std::env::current_exe().unwrap(),
        )
        .env("KAR_CHAT_TEST_PROVIDER", id)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "Kar CLI returned {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    format!(
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn exercise(id: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    let store = temp.path().join("state");
    std::fs::create_dir(&root).unwrap();
    let created = cli(id, &["new", id, "first"], &root, &store);
    assert!(created.contains(&format!("{id} heard first")), "{created}");
    let resumed = cli(id, &["resume", "second"], &root, &store);
    assert!(resumed.contains(&format!("{id} heard second")), "{resumed}");
    let history = cli(id, &["history", "--json"], &root, &store);
    assert!(history.contains("first") && history.contains("second"));
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Ok(id) = std::env::var("KAR_CHAT_TEST_PROVIDER") {
        if args.first().map(String::as_str) == Some("--version") {
            println!("{}", if id == "kiro" { "2.27.1" } else { "0.0.14" });
        } else {
            acp(&id, &args);
        }
        return;
    }
    for provider in ["kiro", "fx"] {
        exercise(provider);
    }
}
