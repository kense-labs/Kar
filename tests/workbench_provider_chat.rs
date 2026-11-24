//! Real Kar CLI and persistence, native protocols replaced with recording peers.
#[path = "workbench_provider_chat/muse_continuation.rs"]
mod muse_continuation;
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod workbench_server;
use workbench_server as server;
#[path = "workbench_provider_chat/registration.rs"]
mod registration;
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
    let mut configured = id == "cursor";
    let mut rejected = false;
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let params = &request["params"];
        let value = match request["method"].as_str().unwrap() {
            "initialize" => json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,
                "mcpCapabilities":{"http":id=="cursor"}}}),
            "session/new" | "session/load" => {
                assert_eq!(
                    params["cwd"].as_str(),
                    std::env::current_dir().unwrap().to_str()
                );
                if registration::acp(&request, &mut rejected) {
                    continue;
                }
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

fn muse(args: &[String]) {
    assert_eq!(args, ["serve", "--trust-workspace"]);
    assert_eq!(std::env::var("MUSE_NO_AUTO_UPDATE").as_deref(), Ok("1"));
    assert!(
        !std::env::vars_os()
            .any(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("META_API_KEY"))
    );
    let mut initialized = false;
    let mut session = Value::Null;
    let mut waiting_turn = Value::Null;
    let mut request_acknowledged = false;
    for line in std::io::stdin().lock().lines() {
        let req: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let p = &req["params"];
        if req.get("method").is_none() {
            assert_eq!(req["id"], 901);
            assert_eq!(req["result"], json!({}));
            request_acknowledged = true;
            continue;
        }
        let result = match req["method"].as_str().unwrap() {
            "initialize" => {
                assert_eq!(p["clientInfo"]["name"], "kar");
                assert_eq!(
                    p["capabilities"]["requestedCapabilities"],
                    json!(["sessionMcp"])
                );
                json!({"schema":{"version":1,"fingerprint":"fixture"},"grantedCapabilities":if registration::mode().as_deref()==Some("muse") {json!(["sessionMcp"])} else {json!([])}})
            }
            "initialized" => {
                initialized = true;
                continue;
            }
            "session/start" | "session/resume" => {
                assert!(initialized);
                registration::muse(&req);
                let id = p["sessionId"].as_str().unwrap();
                assert_eq!(uuid::Uuid::parse_str(id).unwrap().get_version_num(), 7);
                assert_eq!(p["permissionProfile"], ":ask-me");
                if req["method"] == "session/start" {
                    assert_eq!(
                        p["workspaceRoot"].as_str(),
                        std::env::current_dir().unwrap().to_str()
                    );
                    assert_eq!(p["providerId"], "meta");
                    assert_eq!(p["approvalMode"], "promptUnmatched");
                    std::fs::write("native-muse-id", id).unwrap();
                } else {
                    assert_eq!(p["excludeItems"], true);
                    assert_eq!(id, std::fs::read_to_string("native-muse-id").unwrap());
                }
                session = p["sessionId"].clone();
                json!({"session":{"sessionId":session,"modelId":"test-model"}})
            }
            "session/setApprovalMode" => {
                assert_eq!(p["sessionId"], session);
                assert_eq!(p["mode"], "promptUnmatched");
                json!({})
            }
            "turn/start" => {
                let turn = &p["commandId"];
                assert_eq!(
                    uuid::Uuid::parse_str(turn.as_str().unwrap())
                        .unwrap()
                        .get_version_num(),
                    7
                );
                assert_eq!(p["sessionId"], session);
                assert_eq!(p["ifBusy"], "queue");
                assert_eq!(
                    p["workspaceRoots"][0].as_str(),
                    std::env::current_dir().unwrap().to_str()
                );
                let text = format!("muse heard {}", p["input"][0]["text"].as_str().unwrap());
                let notice = |method: &str, mut params: Value| {
                    params["sessionId"] = session.clone();
                    emit(json!({"jsonrpc":"2.0","method":method,"params":params}));
                };
                // Muse may publish turn events before acknowledging the command.
                notice("turn/started", json!({"turnId":turn}));
                notice(
                    "turn/completed",
                    json!({"turnId":"another-turn","terminal":"failed"}),
                );
                let final_item = json!({"itemId":"a","turnId":turn,"kind":"agentMessage","revision":2,"status":"completed","text":text});
                notice(
                    "item/started",
                    json!({"item":{"itemId":"a","turnId":turn,"kind":"agentMessage","revision":1,"status":"inProgress"}}),
                );
                notice("item/completed", json!({"item":final_item}));
                notice("item/completed", json!({"item":final_item}));
                emit(
                    json!({"jsonrpc":"2.0","id":req["id"],"result":{"turnId":turn,"commandId":turn}}),
                );
                waiting_turn = turn.clone();
                emit(
                    json!({"jsonrpc":"2.0","id":901,"method":"approval/request","params":{"sessionId":session,"turnId":turn}}),
                );
                notice(
                    "approval/requested",
                    json!({"approvalId":"file-approval","turnId":turn,
                    "currentRequirementId":{"approvalId":"file-approval","sourceIndex":0},
                    "availableChoices":[{"choiceId":"one","decision":"approved","scope":"once"},{"choiceId":"no","decision":"denied","scope":"once"}]}),
                );
                continue;
            }
            "approval/decide" => {
                assert!(request_acknowledged);
                assert_eq!(p["sessionId"], session);
                assert_eq!(p["choiceId"], "no");
                assert_eq!(p["requirementId"]["approvalId"], "file-approval");
                emit(json!({"jsonrpc":"2.0","id":req["id"],"result":{}}));
                emit(
                    json!({"jsonrpc":"2.0","method":"approval/resolved","params":{"sessionId":session,"turnId":waiting_turn,"approvalId":"file-approval"}}),
                );
                emit(
                    json!({"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":session,"turnId":waiting_turn,"terminal":"completed"}}),
                );
                continue;
            }
            other => panic!("unexpected Muse method {other}"),
        };
        emit(json!({"jsonrpc":"2.0","id":req["id"],"result":result}));
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
        .env("Meta_Api_Key", "fixture-key")
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

fn installed(id: &str) {
    assert!(
        ["kiro", "fx", "muse"].contains(&id),
        "expected kiro, fx or muse"
    );
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("workspace");
    let store = temp.path().join("store");
    std::fs::create_dir(&work).unwrap();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let first = format!("Remember {nonce}. Reply with just that value.");
    let invoke = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_kar"))
            .args(["chat", "--store"])
            .arg(&store)
            .args(args)
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let assistant_recalled = |s: &str| {
        s.lines().any(|line| {
            line.strip_prefix("Assistant> ")
                .is_some_and(|reply| reply.contains(&nonce))
        })
    };
    let one = invoke(&["new", id, &first]);
    assert!(assistant_recalled(&one), "native first reply: {one}");
    let two = invoke(&["resume", "Repeat just the value I asked you to remember."]);
    assert!(assistant_recalled(&two), "native resumed reply: {two}");
    println!("{id}: isolated new and resumed Kar sessions returned the same value.");
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Ok(id) = std::env::var("KAR_CHAT_TEST_PROVIDER") {
        if args.first().map(String::as_str) == Some("--version") {
            println!("{}", if id == "kiro" { "2.27.1" } else { "0.0.14" });
        } else {
            if id == "muse"
                && std::env::var("KAR_MUSE_TEST_SCENARIO").as_deref() == Ok("continuation")
            {
                muse_continuation::peer(&args);
            } else if id == "muse" {
                muse(&args);
            } else {
                acp(&id, &args);
            }
        }
        return;
    }
    if args.first().map(String::as_str) == Some("installed") {
        installed(args.get(1).expect("provider id"));
        return;
    }
    if args.first().map(String::as_str) == Some("muse-continuation") {
        muse_continuation::run();
        return;
    }
    if args.first().map(String::as_str) == Some("model-tools") {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                registration::scenario(),
            )
            .await
            .unwrap();
        });
        return;
    }
    for provider in ["kiro", "fx", "muse"] {
        exercise(provider);
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            registration::scenario(),
        )
        .await
        .unwrap();
    });
}
