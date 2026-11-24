//! Installed agent binaries, isolated homes and loopback model endpoints.
#[path = "native_live/claude_history.rs"]
mod claude_history;
#[path = "native_live/cursor.rs"]
mod cursor;
#[path = "native_live/pi_history.rs"]
mod pi_history;
#[path = "native_live/resume_prompts.rs"]
mod resume_prompts;
#[path = "support/sse.rs"]
mod sse;
use kar::{Event, Outcome, SessionOptions, runtime};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};

// A durable cursor is the byte boundary of a fully synced chat journal record.
fn interrupted_copy(
    source: &std::path::Path,
    destination: &std::path::Path,
    id: uuid::Uuid,
    cut: kar::chat::ChatCursor,
) -> kar::chat::ChatStore {
    let store = kar::chat::ChatStore::open(destination).unwrap();
    let filename = format!("{id}.oarchat");
    let bytes = std::fs::read(source.join(&filename)).unwrap();
    let offset: usize = cut.to_string().split_once(':').unwrap().1.parse().unwrap();
    std::fs::write(destination.join(filename), &bytes[..offset]).unwrap();
    std::fs::File::create(destination.join(format!("{id}.lock"))).unwrap();
    store
}

fn output_schema() -> Value {
    json!({"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false})
}
fn respond(stream: TcpStream) -> std::io::Result<()> {
    respond_with_tool(stream, None, None)
}
fn respond_with_tool(
    stream: TcpStream,
    target: Option<&std::path::Path>,
    capture: Option<&std::sync::Mutex<Vec<Value>>>,
) -> std::io::Result<()> {
    respond_in_workspace(stream, target, capture, None, false)
}
fn respond_in_workspace(
    stream: TcpStream,
    target: Option<&std::path::Path>,
    capture: Option<&std::sync::Mutex<Vec<Value>>>,
    workspace: Option<&std::path::Path>,
    summarizing: bool,
) -> std::io::Result<()> {
    respond_observed(stream, target, capture, workspace, summarizing, |_, _| {})
}
fn respond_observed(
    mut stream: TcpStream,
    target: Option<&std::path::Path>,
    capture: Option<&std::sync::Mutex<Vec<Value>>>,
    workspace: Option<&std::path::Path>,
    summarizing: bool,
    observe: impl FnOnce(&str, &Value),
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(4)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
    let mut length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            length = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let mut reply_text = "rust-wire-ok";
    let request: Value = serde_json::from_slice(&body).unwrap_or_default();
    observe(&path, &request);
    let last = request
        .get("messages")
        .or_else(|| request.get("input"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.iter().rev().find(|m| m["role"] == "user"))
        // OpenCode's title request quotes the original prompt but exposes no tools.
        .filter(|m| {
            !m["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|p| {
                    p["text"]
                        .as_str()
                        .is_some_and(|text| text.ends_with("Reply with only the title."))
                })
            })
        })
        .map(Value::to_string)
        .unwrap_or_default();
    if last.contains("oar-fixture-decline-credential") {
        let response = json!({"type":"error","error":{"type":"authentication_error","message":"fixture credential was rejected"}}).to_string();
        write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\nx-should-retry: false\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )?;
        return stream.flush();
    }
    if last.contains("oar-fixture-provider-throttle") {
        let body = json!({"type":"error","error":{"type":"rate_limit_error","message":"The local fixture refused this request."}}).to_string();
        write!(
            stream,
            "HTTP/1.1 429 Too Many Requests\r\nx-should-retry: false\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
        return stream.flush();
    }
    if let Some(work) = workspace {
        if last.contains("OAR-CRASH-AFTER-READ") && last.contains("tool_result") {
            let gate = work.parent().unwrap();
            std::fs::write(gate.join("model-waiting"), "")?;
            let limit = std::time::Instant::now() + Duration::from_secs(30);
            while !gate.join("model-finish").is_file() {
                if std::time::Instant::now() >= limit {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let gate = work.parent().unwrap().join("native-goal-gate");
        if gate.is_dir() && path.contains("responses") {
            std::fs::write(gate.join("requested"), "")?;
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            while !gate.join("release").exists() {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "goal fixture release",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::write(work.join("goal-progress"), "tracked")?;
        }
        if last.contains("violet-side-question") {
            reply_text = "violet-answer";
        } else if last.contains("blue-continuation") {
            std::fs::write(work.join("version"), "blue")?;
            reply_text = "blue-answer-only";
        } else if last.contains("green-branch") {
            assert_eq!(std::fs::read_to_string(work.join("version"))?, "orange");
            std::fs::write(work.join("version"), "green")?;
        } else if last.contains("orange-original") {
            std::fs::write(work.join("version"), "orange")?;
        }
    }
    if path.starts_with("/v1/messages/count_tokens") {
        let body = r#"{"input_tokens":12}"#;
        return write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
    }
    if path.starts_with("/v1/messages") {
        let request: Value = serde_json::from_slice(&body)?;
        // Claude uses the returned model as a resume fact. Echo the actual model
        // in this fixture so subsequent effort readbacks refer to the same model.
        let reported_model = request["model"].as_str().unwrap_or("claude-sonnet-4-5");
        if let Some(log) = capture {
            let mut calls = log.lock().unwrap();
            let first = calls.is_empty();
            calls.push(request["messages"].clone());
            if first && last.contains("oar-retry-once") {
                let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"503 Service Unavailable"}}"#;
                return write!(
                    stream,
                    "HTTP/1.1 503 Service Unavailable\r\nx-should-retry: false\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        }
        let schema_turn = last.contains("schema-probe");
        let isolated_question = last.contains("isolated-side-question");
        if schema_turn || isolated_question {
            let system = request["system"].to_string();
            assert!(
                system.contains("HOST-BASE-RS") && system.contains("HOST-APPEND-RS"),
                "Claude native instructions"
            );
            if isolated_question {
                assert!(
                    request["tools"].as_array().is_none_or(Vec::is_empty),
                    "disposable question must have no tools"
                );
                reply_text = "isolated-answer";
            } else {
                let tools = request["tools"].as_array().unwrap();
                assert!(
                    tools.iter().any(|t| t["name"] == "Read")
                        && !tools.iter().any(|t| t["name"] == "Bash"),
                    "Claude native tool exclusions"
                );
            }
            let prompt = request["messages"].to_string();
            assert!(
                prompt.contains("KEEP-PROJECT-RULE") && !prompt.contains("EXCLUDE-USER-RULE"),
                "Claude native instruction sources"
            );
        }
        if last.contains("opencode-settings-probe") {
            let actual = request["system"].to_string();
            if !actual.contains("OPEN-BASE {env:OAR_LITERAL_PROBE}")
                || !actual.contains("OPEN-APPEND-RS")
            {
                reply_text = "missing-opencode-settings";
            }
        }
        if !summarizing && last.contains("pi-settings-probe") {
            let system = request["system"].to_string();
            assert!(
                system.contains("PI-SYSTEM-RS") && system.contains("PI-HOST-RS"),
                "Pi system flags"
            );
            let selected = if last.contains("untrusted") {
                "PI-GLOBAL-RS"
            } else {
                "PI-PROJECT-RS"
            };
            let excluded = if last.contains("untrusted") {
                "PI-PROJECT-RS"
            } else {
                "PI-GLOBAL-RS"
            };
            assert!(
                system.contains(selected)
                    && !system.contains(excluded)
                    && system.find(selected) < system.find("PI-HOST-RS"),
                "Pi append selection"
            );
            let tools = request["tools"].as_array().unwrap();
            assert!(
                tools.iter().any(|t| t["name"] == "read")
                    && !tools.iter().any(|t| t["name"] == "bash"),
                "Pi native tool exclusions"
            );
        }
        if summarizing {
            reply_text = "## Objective\nContinue the fixture conversation.\n## Work State\nThe earlier task finished successfully.";
        }
        let spawn_probe =
            !summarizing && last.contains("OAR-SPAWN-BACKGROUND") && !last.contains("tool_result");
        let conversation = request["messages"].to_string();
        let steer_probe =
            workspace.is_some() && !last.is_empty() && conversation.contains("OAR-LIVE-STEER-TOOL");
        if let Some(work) = workspace
            && last.contains("OAR-LIVE-STEER-")
            && !last.contains("OAR-LIVE-FOLLOWUP")
        {
            std::fs::write(work.join("requested"), "")?;
            let until = std::time::Instant::now() + Duration::from_secs(12);
            while !work.join("release").exists() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if conversation.contains("OAR-LIVE-FOLLOWUP") {
            reply_text = "saw the follow-up";
        }
        if !summarizing && last.contains("OAR-CHILD-REPLY") && !last.contains("tool_result") {
            // The parent should already have an end_turn before this child responds.
            std::thread::sleep(Duration::from_millis(600));
            reply_text = "open-child-finished";
        }
        let followup_write = workspace.is_some()
            && conversation.contains("OAR-LIVE-STEER-TEXT")
            && last.contains("OAR-LIVE-SECOND")
            && !last.contains("tool_result");
        let fork_read = workspace.is_some() && last.contains("OAR-FORK-READ-SHARED");
        if target.is_some()
            || schema_turn
            || spawn_probe
            || steer_probe
            || followup_write
            || fork_read
        {
            let answered = request["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .any(|part| part["type"] == "tool_result");
            let background = last.contains("oar-background-task");
            let asking = last.contains("oar-form-question");
            if !answered || background || asking || schema_turn || spawn_probe {
                let (name, id, input) = if fork_read {
                    (
                        "read",
                        "toolu_fork_shared",
                        json!({"path":workspace.unwrap().parent().unwrap().join("shared/grant.txt")}),
                    )
                } else if spawn_probe {
                    (
                        "subagent",
                        "call_child",
                        json!({"agent":"general","description":"Check background events","prompt":"OAR-CHILD-REPLY"}),
                    )
                } else if followup_write {
                    (
                        "Write",
                        "toolu_followup_write",
                        json!({"file_path":workspace.unwrap().join("followup.txt"),"content":"from the independent turn"}),
                    )
                } else if steer_probe {
                    let work = workspace.unwrap();
                    if request["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t["name"] == "Read")
                    {
                        (
                            "Read",
                            "toolu_steer_read",
                            json!({"file_path":work.join("fixture.txt")}),
                        )
                    } else {
                        (
                            "read",
                            "toolu_steer_read",
                            json!({"filePath":work.join("fixture.txt")}),
                        )
                    }
                } else if schema_turn {
                    let tool = request["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|tool| tool["name"] == "StructuredOutput")
                        .expect("native structured output tool");
                    assert_eq!(
                        tool["input_schema"]["properties"],
                        output_schema()["properties"]
                    );
                    ("StructuredOutput", "toolu_schema", json!({"answer":4}))
                } else if asking {
                    (
                        "AskUserQuestion",
                        "toolu_form",
                        json!({"questions":[
                            {"question":"Which color?","header":"Color","multiSelect":false,"options":[{"label":"Blue","description":"Blue color"},{"label":"Green","description":"Green color"}]},
                            {"question":"Which checks?","header":"Checks","multiSelect":true,"options":[{"label":"Unit","description":"Unit suite"},{"label":"Replay","description":"Replay suite"}]}
                        ]}),
                    )
                } else if last.contains("oar-claude-recover") {
                    ("Read", "toolu_recover", json!({"file_path":target}))
                } else if last.contains("oar-pi-recover") {
                    ("read", "pi_read_1", json!({"path":target}))
                } else if background {
                    (
                        "Bash",
                        "toolu_bg",
                        json!({"command":"sleep 20","description":"Wait for task control","run_in_background":true}),
                    )
                } else {
                    (
                        "Write",
                        "toolu_approval",
                        json!({"file_path":target,"content":"fixture approved"}),
                    )
                };
                let encoded = input.to_string();
                let split = encoded
                    .find("fixture approved")
                    .map_or(encoded.len() / 2, |i| i + "fixture ".len());
                let split = (0..=split)
                    .rev()
                    .find(|i| encoded.is_char_boundary(*i))
                    .unwrap();
                let events = [
                    json!({"type":"message_start","message":{"id":"msg_tool","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":&encoded[..split]}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":&encoded[split..]}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":3}}),
                    json!({"type":"message_stop"}),
                ];
                return send_events(stream, &events);
            }
        }
        let mut events = vec![
            json!({"type":"message_start","message":{"id":"msg_probe","type":"message","role":"assistant","model":reported_model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":0}}}),
        ];
        let reasoning = conversation.contains("oar-claude-recover");
        let text_index = usize::from(reasoning);
        if reasoning {
            events.extend([
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"native reasoning block"}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"fixture-signature"}}),
                json!({"type":"content_block_stop","index":0}),
            ]);
        }
        events.extend([
            json!({"type":"content_block_start","index":text_index,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":text_index,"delta":{"type":"text_delta","text":reply_text}}),
            json!({"type":"content_block_stop","index":text_index}),
        ]);
        if reply_text != "violet-answer"
            && String::from_utf8_lossy(&body).contains("orange-original")
        {
            events.extend([
                json!({"type":"content_block_start","index":text_index+1,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":text_index+1,"delta":{"type":"text_delta","text":"second-block"}}),
                json!({"type":"content_block_stop","index":text_index+1}),
            ]);
        }
        events.extend([
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ]);
        return send_events(stream, &events);
    }
    if !path.ends_with("/responses") {
        return write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }
    if let Some(log) = capture {
        let request: Value = serde_json::from_slice(&body)?;
        log.lock().unwrap().push(request["input"].clone());
    }
    if last.contains("schema-probe") {
        assert_eq!(
            request.pointer("/text/format/schema"),
            Some(&output_schema())
        );
        let instructions = request["instructions"].to_string();
        let input = request["input"].as_array().unwrap();
        let roles = |marker: &str| {
            input
                .iter()
                .filter(|item| item.to_string().contains(marker))
                .map(|item| item["role"].clone())
                .collect::<Vec<_>>()
        };
        capture.unwrap().lock().unwrap().push(json!({
            "probe": last, "base_instruction": instructions.contains("HOST-BASE-RS"), "base_roles": roles("HOST-BASE-RS"),
            "existing_roles": roles("EXISTING-CODEX-RULE"), "append_roles": roles("HOST-APPEND-RS"),
            "global_append": instructions.contains("HOST-APPEND-RS"), "global_existing": instructions.contains("EXISTING-CODEX-RULE"),
            "mcp_names":request["tools"].as_array().unwrap().iter().filter_map(|t|t["name"].as_str()).filter(|s|s.starts_with("mcp__")).collect::<Vec<_>>()
        }));
        reply_text = r#"{"answer":4}"#;
    }
    let input = request["input"].as_array();
    let already_spawned = input
        .into_iter()
        .flatten()
        .any(|v| v["call_id"] == "usage_spawn");
    if last.contains("OAR-METER-PARENT") && !already_spawned {
        let item = json!({"type":"function_call","id":"fc_meter","call_id":"usage_spawn","namespace":"collaboration","name":"spawn_agent",
            "arguments":json!({"message":"OAR-METER-CHILD","task_name":"meter","fork_turns":"none"}).to_string(),"status":"completed"});
        let response = json!({"id":"resp_spawn","object":"response","status":"completed","model":"gpt-5.1","output":[item],"usage":{"input_tokens":2,"output_tokens":2,"total_tokens":4}});
        return send_events(
            stream,
            &[
                json!({"type":"response.created","response":{"id":"resp_spawn","status":"in_progress","output":[]}}),
                json!({"type":"response.output_item.added","output_index":0,"item":item}),
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.completed","response":response}),
            ],
        );
    }
    let child_measurement = last.contains("OAR-METER-CHILD")
        || (!already_spawned
            && !last.contains("OAR-METER-PARENT")
            && request["input"].to_string().contains("OAR-METER-CHILD"));
    let (spent_input, spent_output) = if child_measurement { (7, 3) } else { (2, 2) };
    let item = json!({"type":"message","id":"msg_fixture","role":"assistant","status":"completed","content":[{"type":"output_text","text":reply_text,"annotations":[]}]});
    let response = json!({"id":"resp_fixture","object":"response","status":"completed","model":"gpt-5.1","output":[item],"usage":{"input_tokens":spent_input,"output_tokens":spent_output,"total_tokens":spent_input+spent_output}});
    let events = [
        json!({"type":"response.created","response":{"id":"resp_fixture","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_fixture","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_fixture","output_index":0,"content_index":0,"delta":reply_text}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":response}),
    ];
    send_events(stream, &events)
}
fn send_events(mut stream: TcpStream, events: &[Value]) -> std::io::Result<()> {
    let body = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect::<String>();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
#[ignore = "requires an installed Codex CLI; uses only a loopback model fixture"]
async fn actual_codex_turn_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex-home");
    std::fs::create_dir(&home).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let _ = respond(conn);
            });
        }
    });
    let config = format!(
        r#"model = "gpt-5.1"
model_provider = "fixture"
[model_providers.fixture]
name = "local test fixture"
base_url = "http://127.0.0.1:{port}/v1"
env_key = "OAR_FIXTURE_KEY"
wire_api = "responses"
[features]
multi_agent = true
multi_agent_v2 = true
"#
    );
    std::fs::write(home.join("config.toml"), config).unwrap();
    let mut opts = SessionOptions::new(dir.path());
    opts.env.insert("CODEX_HOME".into(), home.into_os_string());
    opts.env
        .insert("OAR_FIXTURE_KEY".into(), "only-a-fixture".into());
    let provider = runtime("codex").unwrap();
    missing_target(&provider, &opts).await;
    let session = provider.open(opts.clone()).await.unwrap();
    let turn = session.prompt("Reply with one word.").await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(40), turn.wait())
        .await
        .expect("native turn timed out")
        .unwrap();
    assert!(
        session
            .native_session_file()
            .await
            .unwrap()
            .unwrap()
            .is_file()
    );
    let records = session.records();
    assert_eq!(
        outcome,
        Outcome::Completed,
        "{:?}",
        records
            .iter()
            .filter_map(|r| r
                .native
                .pointer("/params/turn/error/message")
                .and_then(Value::as_str))
            .collect::<Vec<_>>()
    );
    assert!(
        records
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e,Event::TextDelta{text} if text.contains("rust-wire-ok")))
    );
    let id = session.id().to_string();
    let stream = session.stream_id();
    let usage = session.usage().total.expect("actual Codex token report");
    assert_eq!((usage.input, usage.output), (2, 2));
    let anchor = records
        .iter()
        .find(|r| r.native["method"] == "turn/completed")
        .and_then(|r| r.span_id.clone())
        .unwrap();
    assert_eq!(
        session
            .prompt("Only the source should remember the second marker.")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    session.close().await.unwrap();
    let fork = provider
        .fork(&id, Some(&anchor), opts.clone())
        .await
        .unwrap();
    assert_ne!(fork.id(), id);
    let history = &fork.info()["thread"]["turns"];
    assert_eq!(history.as_array().unwrap().last().unwrap()["id"], anchor);
    assert!(!history.to_string().contains("second marker"));
    let fork_id = fork.id().to_owned();
    assert_eq!(
        fork.prompt("Only the branch should remember the branch marker.")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    fork.close().await.unwrap();
    let mut branch = opts.clone();
    branch.resume = Some(fork_id);
    let reopened = provider.open(branch).await.unwrap();
    assert!(
        reopened.info()["thread"]["turns"]
            .to_string()
            .contains("branch marker")
    );
    reopened.close().await.unwrap();
    opts.resume = Some(id.clone());
    let resumed = provider.open(opts).await.unwrap();
    assert_eq!(
        resumed
            .prompt("measure only the resumed turn")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    let measured = resumed.usage().total.expect("native Codex resume baseline");
    assert_eq!((measured.input, measured.output), (2, 2));
    let source = resumed.info()["thread"]["turns"].to_string();
    assert!(source.contains("second marker") && !source.contains("branch marker"));
    assert_eq!(resumed.id(), id);
    assert_ne!(resumed.stream_id(), stream);
    let cursor = resumed.records().last().unwrap().cursor();
    let mut updates = resumed.subscribe(Some(&cursor)).unwrap();
    let operation = resumed.start_compaction().await.unwrap();
    let result = operation.wait(Some(Duration::from_secs(30))).await;
    let kar::CompactionStatus::Completed { frame } = &result.status else {
        panic!("Codex compaction did not complete: {result:?}");
    };
    assert_eq!(resumed.snapshot().manual_compaction.as_ref(), Some(&result));
    let boundaries = tokio::time::timeout(Duration::from_secs(30), async {
        let mut reports = Vec::new();
        while let Some(record) = updates.next().await {
            reports.extend(record.compactions());
            if record.native["method"] == "turn/completed" {
                assert_eq!(record.native["params"]["turn"]["status"], "completed");
                break;
            }
        }
        reports
    })
    .await
    .expect("Codex did not finish its compact turn");
    assert_eq!(
        boundaries
            .iter()
            .map(|r| r.state.as_str())
            .collect::<Vec<_>>(),
        ["started", "completed"]
    );
    assert!(boundaries[0].compaction_id.is_some());
    assert_eq!(frame.after_seq, boundaries[1].cursor.seq);
    assert!(result.response.is_some());
    assert_eq!(
        resumed.snapshot().last_compaction.as_ref(),
        boundaries.last()
    );
    let cursor = resumed.records().last().unwrap().cursor();
    let mut feed = resumed.subscribe(Some(&cursor)).unwrap();
    assert_eq!(
        resumed
            .prompt("OAR-METER-PARENT")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    let measured = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let s = resumed.snapshot();
            if s.usage.with_children.is_some() {
                break s;
            }
            feed.next()
                .await
                .expect("Codex closed during child measurement");
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "missing native child usage; links={:?}, token sources={:?}, tool results={:?}",
            resumed.graph(),
            resumed
                .records()
                .iter()
                .filter(|r| r.native["method"] == "thread/tokenUsage/updated")
                .map(|r| r.session_id.clone())
                .collect::<Vec<_>>(),
            resumed
                .records()
                .iter()
                .filter_map(|r| r
                    .native
                    .pointer("/params/item/type")
                    .and_then(Value::as_str)
                    .filter(|s| s.contains("ollab"))
                    .map(str::to_owned))
                .collect::<Vec<_>>()
        )
    });
    assert_eq!(measured.child_usage.len(), 1);
    let (child, child_figures) = measured.child_usage.first_key_value().unwrap();
    let child_usage = child_figures.total.unwrap();
    assert_eq!((child_usage.input, child_usage.output), (7, 3));
    assert_eq!(resumed.usage_for(child).total, Some(child_usage));
    let own = measured.usage.total.unwrap();
    let all = measured.usage.with_children.unwrap();
    assert_eq!((all.input, all.output), (own.input + 7, own.output + 3));
    resumed.close().await.unwrap();
}

// Put both frames in one pipe write while the real CLI is starting. This
// exercises a cancellation before native admission, independent of OS scheduling
// between a running SDK session's prompt and abort calls.
async fn claude_cancel_first_write(runtime: &kar::Runtime, opts: &SessionOptions) {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let session = uuid::Uuid::new_v4().to_string();
    let input = uuid::Uuid::new_v4().to_string();
    let mut process = tokio::process::Command::new(&runtime.program)
        .envs(&opts.env)
        .current_dir(&opts.cwd)
        .args([
            "--print",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--replay-user-messages",
            "--session-id",
            &session,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let prompt = json!({"type":"user","uuid":input,"session_id":session,"parent_tool_use_id":null,"message":{"role":"user","content":"cancel while queued"}});
    let interrupt = json!({"type":"control_request","request_id":"cancel","request":{"subtype":"interrupt","cancel_queued":true}});
    process
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{prompt}\n{interrupt}\n").as_bytes())
        .await
        .unwrap();
    let mut lines = tokio::io::BufReader::new(process.stdout.take().unwrap()).lines();
    let mut states = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["type"] == "command_lifecycle" && frame["command_uuid"] == input {
                states.push(frame["state"].as_str().unwrap().to_owned());
            }
            if frame.pointer("/response/request_id") == Some(&json!("cancel")) {
                assert_eq!(
                    frame.pointer("/response/response/cancelled"),
                    Some(&json!([input]))
                );
                return;
            }
        }
        panic!("Claude exited before the interrupt receipt");
    })
    .await
    .unwrap();
    assert_eq!(states, ["queued", "cancelled"]);
    process.kill().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Claude Code; tests native UUID refusal, cancellation and HTTP replay against a loopback model"]
async fn actual_claude_input_receipts_and_reconnect() {
    use kar::chat::{ChatReplica, ChatStore, HttpChatFeed, InputStatus, TurnStatus};
    use uuid::Uuid;
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().canonicalize().unwrap().join("work");
    std::fs::create_dir(&work).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = requests.clone();
    let model = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let socket = socket.into_std().unwrap();
            socket.set_nonblocking(false).unwrap();
            let calls = calls.clone();
            tokio::task::spawn_blocking(move || respond_with_tool(socket, None, Some(&calls)));
        }
    });
    let mut opts = SessionOptions::new(&work);
    for (key, value) in [
        ("HOME", dir.path().to_str().unwrap()),
        ("CLAUDE_CONFIG_DIR", dir.path().to_str().unwrap()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("ANTHROPIC_BASE_URL", &endpoint),
        ("ANTHROPIC_API_KEY", "fixture-key-only"),
        ("ANTHROPIC_AUTH_TOKEN", ""),
    ] {
        opts.env.insert(key.into(), value.into());
    }
    opts.model = Some("sonnet".into());
    let rt = runtime("claude").unwrap();
    claude_cancel_first_write(&rt, &opts).await;
    assert!(requests.lock().unwrap().is_empty());

    let current = ChatStore::open(dir.path().join("current")).unwrap();
    let original = current
        .create("claude", Some(&rt.program), opts.clone())
        .await
        .unwrap();
    let id = original.info().id;
    let native = original.info().native_id.clone();
    original
        .send(Uuid::new_v4(), "prime the native transcript")
        .await
        .unwrap();
    let stale = ChatStore::open(dir.path().join("stale")).unwrap();
    // Save the host ledger before the input, then let the native session get ahead.
    for suffix in ["oarchat", "lock"] {
        let file = format!("{id}.{suffix}");
        std::fs::copy(
            dir.path().join("current").join(&file),
            dir.path().join("stale").join(file),
        )
        .unwrap();
    }
    let repeated = Uuid::new_v4();
    let fresh = Uuid::new_v4();
    let same = "identical follow-up text";
    let sent = original.send(repeated, same).await.unwrap();
    assert_eq!(sent.native_input_receipt, Some(kar::InputReceipt::Started));
    assert!(sent.native_result_id.is_some());
    original.close().await.unwrap();
    let baseline = requests.lock().unwrap().len();
    assert_eq!(baseline, 2);

    let listen = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listen.local_addr().unwrap());
    let router = kar::chat::event_router(stale.clone(), id);
    let http = tokio::spawn(async move { axum::serve(listen, router).await.unwrap() });
    let mut online = HttpChatFeed::new(&url).unwrap();
    let mut disconnecting = HttpChatFeed::new(&url).unwrap();
    let mut a = ChatReplica::from_snapshot(online.snapshot().await.unwrap());
    let mut b = ChatReplica::from_snapshot(disconnecting.snapshot().await.unwrap());
    stale.enqueue(id, repeated, same).unwrap();
    stale.enqueue(id, fresh, same).unwrap();
    let recover = async {
        for _ in 0..2 {
            let host = stale
                .resume_with(id, |options| options.env = opts.env.clone())
                .await
                .unwrap();
            assert_eq!(host.info().native_id, native);
            host.drain_queue().await.unwrap();
            host.close().await.unwrap();
        }
    };
    let disconnect = async {
        while b.turns().len() < 2 {
            let event = disconnecting.recv(b.cursor()).await.unwrap();
            assert!(b.apply(event).unwrap());
        }
        drop(disconnecting);
    };
    let watch = async {
        while !a
            .turns()
            .last()
            .is_some_and(|t| t.id == fresh && t.status == TurnStatus::Completed)
        {
            let event = online.recv(a.cursor()).await.unwrap();
            assert!(a.apply(event).unwrap());
        }
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(recover, disconnect, watch)
    })
    .await
    .expect("native input queue did not settle");
    let snapshot = stale.feed_snapshot(id).unwrap();
    let mut resumed = HttpChatFeed::new(&url).unwrap();
    for (replica, client) in [(&mut a, &mut online), (&mut b, &mut resumed)] {
        while replica.cursor() != snapshot.cursor {
            let event = client.recv(replica.cursor()).await.unwrap();
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        assert_eq!(replica.turns(), snapshot.history.turns);
    }
    let [_, duplicate, delivered] = a.turns() else {
        panic!("expected three inputs");
    };
    assert_eq!(duplicate.id, repeated);
    assert_eq!(duplicate.status, TurnStatus::Failed);
    assert_eq!(duplicate.native_result_id, None);
    assert!(delivered.native_result_id.is_some());
    assert_ne!(delivered.native_result_id, sent.native_result_id);
    assert_eq!(
        duplicate.native_input_receipt,
        Some(kar::InputReceipt::Duplicate)
    );
    assert_eq!(
        duplicate.failure.as_ref().unwrap().class,
        kar::FailureClass::InvalidRequest
    );
    assert_eq!(
        (delivered.id, delivered.status),
        (fresh, TurnStatus::Completed)
    );
    assert_eq!(
        delivered.native_input_receipt,
        Some(kar::InputReceipt::Started)
    );
    let queue = stale.queued_inputs(id).unwrap();
    assert_eq!(
        queue.iter().map(|i| (i.id, i.status)).collect::<Vec<_>>(),
        [
            (repeated, InputStatus::Duplicate),
            (fresh, InputStatus::Sent)
        ]
    );
    assert_eq!(requests.lock().unwrap().len(), baseline + 1);
    http.abort();
    model.abort();
}

#[tokio::test]
#[ignore = "requires an installed Claude Code binary; uses only a loopback model fixture"]
async fn actual_claude_turn_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let requests = messages.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let requests = requests.clone();
            std::thread::spawn(move || {
                let _ = respond_with_tool(conn, None, Some(&requests));
            });
        }
    });
    let mut opts = SessionOptions::new(dir.path());
    for (name, value) in [
        ("CLAUDE_CODE_MAX_RETRIES", "1"),
        ("HOME", dir.path().to_str().unwrap()),
        ("CLAUDE_CONFIG_DIR", dir.path().to_str().unwrap()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("ANTHROPIC_BASE_URL", &endpoint),
        ("ANTHROPIC_API_KEY", "fixture-key-only"),
        ("ANTHROPIC_AUTH_TOKEN", ""),
    ] {
        opts.env.insert(name.into(), value.into());
    }
    opts.model = Some("sonnet".into());
    let provider = runtime("claude").unwrap();
    missing_target(&provider, &opts).await;
    let session = provider.open(opts.clone()).await.unwrap();
    assert!(session.models().await.unwrap().is_array());
    let assert_turn = async |session: &kar::Session| {
        let outcome = tokio::time::timeout(
            Duration::from_secs(40),
            session.prompt("Say hello.").await.unwrap().wait(),
        )
        .await
        .expect("Claude turn timed out")
        .unwrap();
        assert_eq!(
            outcome,
            Outcome::Completed,
            "{:?}",
            session
                .records()
                .iter()
                .filter(|r| r.native["type"] == "result")
                .map(|r| r.native.get("result"))
                .collect::<Vec<_>>()
        );
        assert!(
            session
                .records()
                .iter()
                .flat_map(|r| &r.events)
                .any(|e| matches!(e,Event::TextDelta{text} if text.contains("rust-wire-ok")))
        );
    };
    assert_turn(&session).await;
    let usage = session.usage().total.expect("actual Claude token report");
    assert_eq!((usage.input, usage.output), (3, 3));
    let occupancy = session.current_context().expect("Claude context report");
    assert!(occupancy.tokens.is_some() && occupancy.context_window.is_some());
    let (first, second) = tokio::join!(session.context_breakdown(), session.context_breakdown());
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    assert!(first.context_window > 0 && !first.categories.is_empty());
    assert!(session.records().iter().all(|r| {
        r.native
            .pointer("/request/subtype")
            .is_none_or(|v| v != "get_context_usage")
            && r.native.pointer("/response/response/categories").is_none()
    }));
    let id = session.id().to_owned();
    let anchor = session
        .records()
        .iter()
        .flat_map(|r| &r.events)
        .find_map(|e| match e {
            Event::ForkAnchor { anchor } => Some(anchor.clone()),
            _ => None,
        })
        .unwrap();
    let before_cancel = session.records().len();
    let pending = session.prompt("interrupt this input").await.unwrap();
    session.abort().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(6), pending.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Aborted
    );
    let after_cancel = session.records();
    let cancellation = &after_cancel[before_cancel..];
    let receipts: Vec<_> = cancellation
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match e {
            Event::NativeInput { receipt, .. } => Some(*receipt),
            _ => None,
        })
        .collect();
    let native_ends = cancellation
        .iter()
        .flat_map(|r| &r.events)
        .filter(|e| matches!(e, Event::TurnEnded { .. }))
        .count();
    assert!(
        matches!(
            (receipts.last(), native_ends),
            (Some(kar::InputReceipt::Cancelled), 0) | (Some(kar::InputReceipt::Started), 1)
        ),
        "{receipts:?}, {native_ends} real turn ends"
    );
    assert_eq!(
        session
            .prompt("claude-source-only")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    session.close().await.unwrap();
    let branch = provider
        .fork(&id, Some(&anchor), opts.clone())
        .await
        .unwrap();
    assert_ne!(branch.id(), id);
    assert_eq!(
        branch
            .prompt("claude-branch-only")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    {
        let requests = messages.lock().unwrap();
        let fork_history = requests
            .iter()
            .find(|r| r.to_string().contains("claude-branch-only"))
            .unwrap()
            .to_string();
        assert!(
            fork_history.contains("Say hello.") && !fork_history.contains("claude-source-only")
        );
    }
    assert!(
        branch
            .records()
            .iter()
            .all(|r| r.native.pointer("/response/response/sections").is_none())
    );
    branch.close().await.unwrap();
    opts.resume = Some(id.clone());
    let resumed = provider.open(opts).await.unwrap();
    assert_eq!(resumed.id(), id);
    assert_turn(&resumed).await;
    let total = resumed
        .usage()
        .total
        .expect("native Claude resume baseline");
    assert_eq!((total.input, total.output), (3, 3));
    assert!(resumed.records().iter().all(|r| {
        r.native
            .pointer("/response/response/session/model_usage")
            .is_none()
    }));
    let begin = resumed.snapshot().cursor.unwrap();
    let receipt = resumed
        .start_compaction()
        .await
        .unwrap()
        .wait(Some(Duration::from_secs(20)))
        .await;
    assert!(
        matches!(receipt.status, kar::CompactionStatus::Completed { .. }),
        "Claude compaction {receipt:?}, lifecycle {:?}",
        resumed
            .records()
            .iter()
            .filter(|r| r.seq > begin.after_seq)
            .map(|r| (
                &r.native["type"],
                &r.native["subtype"],
                &r.native["state"],
                &r.native["command_uuid"],
                &r.native["user_message_uuid"]
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        resumed
            .records()
            .iter()
            .filter(|r| r.seq > begin.after_seq)
            .all(|r| r.native.pointer("/response/response/commands").is_none())
    );
    let report = tokio::time::timeout(
        Duration::from_secs(35),
        resumed
            .prompt("oar-fixture-decline-credential")
            .await
            .unwrap()
            .report(),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no terminal Claude frame, recent frame types: {:?}",
            resumed
                .records()
                .iter()
                .rev()
                .take(12)
                .map(|r| (
                    &r.native["type"],
                    &r.native["subtype"],
                    &r.native["error"],
                    &r.native["api_error_status"]
                ))
                .collect::<Vec<_>>()
        )
    })
    .unwrap();
    assert_eq!(report.outcome, Outcome::Failed);
    let failure = report.failure.unwrap();
    assert_eq!(failure.class, kar::FailureClass::Auth, "{failure:?}");
    assert_eq!(failure.status, Some(401));
    assert_eq!(failure.credential, Some(kar::CredentialProblem::Rejected));
    assert_eq!(resumed.snapshot().last_failure, Some(failure));
    resumed.close().await.unwrap();
}

async fn missing_target(runtime: &kar::Runtime, opts: &SessionOptions) {
    let mut opts = opts.clone();
    let id = uuid::Uuid::new_v4().to_string();
    opts.resume = Some(id.clone());
    let err = runtime
        .open(opts)
        .await
        .err()
        .expect("opened a nonexistent session");
    assert!(
        matches!(&err, kar::Error::Open(e) if matches!(e.as_ref(), kar::OpenError::SessionNotFound {session_id,..} if session_id == &id)),
        "{}: {err}",
        runtime.id
    );
}

#[path = "native_live/opencode_fork.rs"]
mod opencode_fork;

fn opencode_model(opts: &mut SessionOptions, endpoint: &str) {
    let config = json!({"model":"fixture/fixture","small_model":"fixture/fixture","autoupdate":false,"provider":{"fixture":{"npm":"@ai-sdk/anthropic","name":"Fixture","options":{"baseURL":format!("{endpoint}/v1"),"apiKey":"test-credential"},"models":{"fixture":{"name":"Fixture","attachment":true,"modalities":{"input":["text","image"],"output":["text"]},"limit":{"context":200000,"output":8192}}}}}});
    opts.env
        .insert("OPENCODE_CONFIG_CONTENT".into(), config.to_string().into());
    for key in [
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
        "OPENCODE_DISABLE_CLAUDE_CODE",
    ] {
        opts.env.insert(key.into(), "1".into());
    }
}

async fn real_protocol_turn(provider_id: &str) {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let summary_mode = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model_mode = summary_mode.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let summary = model_mode.load(std::sync::atomic::Ordering::Acquire);
            std::thread::spawn(move || {
                let _ = respond_in_workspace(conn, None, None, None, summary);
            });
        }
    });
    let mut opts = SessionOptions::new(dir.path());
    opts.env.insert("HOME".into(), dir.path().into());
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        opts.env
            .insert(key.into(), dir.path().join(key).into_os_string());
    }
    opts.model = Some("fixture/fixture".into());
    if provider_id == "pi" {
        let agent_dir = dir.path().join("pi-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        opts.env.insert(
            "OAR_PI_AGENT_DIR".into(),
            agent_dir.clone().into_os_string(),
        );
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"compaction":{"enabled":false,"keepRecentTokens":50}}"#,
        )
        .unwrap();
        std::fs::write(agent_dir.join("APPEND_SYSTEM.md"), "PI-GLOBAL-RS").unwrap();
        std::fs::create_dir_all(dir.path().join(".pi")).unwrap();
        std::fs::write(dir.path().join(".pi/APPEND_SYSTEM.md"), "PI-PROJECT-RS").unwrap();
        opts.settings.system_prompt = Some("PI-SYSTEM-RS".into());
        opts.settings.append_system_prompt = Some("PI-HOST-RS".into());
        opts.settings.pi_project_trusted = Some(false);
        opts.settings.disallowed_tools.push("bash".into());
        let config = json!({"providers":{"fixture":{"baseUrl":endpoint,"api":"anthropic-messages","apiKey":"test-credential","models":[{"id":"fixture","name":"Fixture","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":200000,"maxTokens":8192}]}}});
        std::fs::write(agent_dir.join("models.json"), config.to_string()).unwrap();
    } else {
        opts.settings.system_prompt = Some("OPEN-BASE {env:OAR_LITERAL_PROBE}".into());
        opts.settings.append_system_prompt = Some("OPEN-APPEND-RS".into());
        opts.env
            .insert("OAR_LITERAL_PROBE".into(), "should-not-interpolate".into());
        opencode_model(&mut opts, &endpoint);
    }
    let runtime = runtime(provider_id).unwrap();
    if provider_id == "pi" {
        missing_target(&runtime, &opts).await;
    }
    let session = runtime.open(opts.clone()).await.unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(35),
        session
            .prompt(if provider_id == "pi" {
                "pi-settings-probe-untrusted"
            } else {
                "opencode-settings-probe"
            })
            .await
            .unwrap()
            .wait(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        outcome,
        Outcome::Completed,
        "{:?}",
        session
            .records()
            .iter()
            .rev()
            .take(4)
            .map(|r| &r.native)
            .collect::<Vec<_>>()
    );
    assert!(
        session
            .records()
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e,Event::TextDelta{text} if text.contains("rust-wire-ok")))
    );
    let measured = session
        .usage()
        .total
        .expect("native Pi or OpenCode token report");
    assert_eq!((measured.input, measured.output), (3, 3));
    if provider_id == "pi" {
        assert!(
            session
                .native_session_file()
                .await
                .unwrap()
                .unwrap()
                .is_file()
        );
    }
    if provider_id == "opencode" {
        assert!(session.capabilities().fork);
        let branch = runtime
            .fork(session.id(), None, opts.clone())
            .await
            .unwrap();
        assert_ne!(branch.id(), session.id());
        assert_eq!(
            branch
                .prompt("new branch")
                .await
                .unwrap()
                .wait()
                .await
                .unwrap(),
            Outcome::Completed
        );
        branch.close().await.unwrap();
    }
    opts.resume = Some(session.id().into());
    if provider_id == "pi" {
        let ctx = session
            .current_context()
            .expect("Pi context at end of turn");
        assert_eq!(ctx.context_window, Some(200_000));
        assert!(ctx.tokens.is_some());
    }
    session.close().await.unwrap();
    if provider_id == "pi" {
        opts.settings.pi_project_trusted = Some(true);
    } else {
        let other = tempfile::tempdir().unwrap();
        let mut wrong = opts.clone();
        wrong.cwd = other.path().to_owned();
        let e = runtime
            .open(wrong)
            .await
            .err()
            .expect("resumed in another workspace");
        assert!(
            matches!(&e, kar::Error::Open(e) if matches!(e.as_ref(),kar::OpenError::ResumeCwd {reason:kar::ResumeCwdReason::OtherCwd {..},..})),
            "{e}"
        );
        let mut missing = opts.clone();
        missing.resume = Some(format!("ses_{}", uuid::Uuid::new_v4().simple()));
        let e = runtime
            .open(missing)
            .await
            .err()
            .expect("opened an unlisted session");
        assert!(
            matches!(&e,kar::Error::Open(e) if matches!(e.as_ref(),kar::OpenError::ResumeCwd {reason:kar::ResumeCwdReason::NotListed,..})),
            "{e}"
        );
    }
    let resumed = runtime.open(opts).await.unwrap();
    if provider_id == "opencode" {
        let r = resumed
            .prompt("opencode-settings-probe-resume")
            .await
            .unwrap()
            .report()
            .await
            .unwrap();
        assert_eq!(r.text, "rust-wire-ok");
        let cursor = resumed.records().last().unwrap().cursor();
        let mut observed = resumed.subscribe(Some(&cursor)).unwrap();
        let parent = resumed
            .prompt("OAR-SPAWN-BACKGROUND")
            .await
            .unwrap()
            .report()
            .await
            .unwrap();
        assert_eq!(parent.outcome, Outcome::Completed);
        let mut child_text = Vec::new();
        let mut root_ended = false;
        let child = tokio::time::timeout(Duration::from_secs(12), async {
            while let Some(record) = observed.next().await {
                for event in &record.events {
                    if record.session_id.as_deref() == Some(resumed.id())
                        && matches!(
                            event,
                            Event::TurnEnded {
                                outcome: Outcome::Completed
                            }
                        )
                    {
                        root_ended = true;
                    }
                    if let Event::TextDelta { text } = event
                        && text.contains("open-child-finished")
                    {
                        assert!(
                            root_ended,
                            "background child fixture must exercise the post-turn path"
                        );
                        child_text.push(record.session_id.clone());
                    }
                    if let Event::Task {
                        id,
                        change:
                            kar::TaskChange::Ended {
                                status: kar::TaskStatus::Completed,
                                ..
                            },
                    } = event
                    {
                        return id.clone();
                    }
                }
            }
            panic!("OpenCode exited before its background child");
        })
        .await
        .expect("OpenCode did not report its background child");
        assert_ne!(child, resumed.id());
        assert_eq!(child_text, [Some(child.clone())]);
        assert!(
            resumed
                .snapshot()
                .graph
                .edges
                .iter()
                .any(|edge| edge.parent == resumed.id() && edge.child == child)
        );
    }
    if provider_id == "pi" {
        assert_eq!(
            resumed
                .prompt("pi-settings-probe-trusted")
                .await
                .unwrap()
                .wait()
                .await
                .unwrap(),
            Outcome::Completed
        );
    }
    assert_eq!(resumed.id(), session.id());
    assert!(resumed.capabilities().manual_compaction);
    if provider_id == "pi" {
        for turn in ["A", "B", "C"] {
            resumed
                .prompt(format!(
                    "Seed compaction {turn} {}",
                    "sample conversation ".repeat(150)
                ))
                .await
                .unwrap()
                .wait()
                .await
                .unwrap();
        }
    }
    if provider_id == "opencode" {
        // The normal fixture response does not satisfy OpenCode's native summary template.
        let r = resumed
            .start_compaction()
            .await
            .unwrap()
            .wait(Some(Duration::from_secs(15)))
            .await;
        assert!(matches!(
            r.status,
            kar::CompactionStatus::Failed {
                code: kar::CompactionFailure::NativeFailed,
                ..
            }
        ));
    }
    summary_mode.store(true, std::sync::atomic::Ordering::Release);
    let handle = resumed.start_compaction().await.unwrap();
    let (_, followup) = resumed
        .queue("continue after explicit compaction")
        .await
        .unwrap();
    let report = handle.wait(Some(Duration::from_secs(15))).await;
    assert!(
        matches!(report.status, kar::CompactionStatus::Completed { .. }),
        "{provider_id} compaction: {report:?}; boundaries: {:?}",
        resumed
            .records()
            .iter()
            .flat_map(|r| r.compactions())
            .collect::<Vec<_>>()
    );
    assert_eq!(followup.wait().await.unwrap(), Outcome::Completed);
    assert_eq!(resumed.snapshot().manual_compaction.as_ref(), Some(&report));
    if provider_id == "opencode" {
        let turn = resumed
            .prompt("oar-fixture-provider-throttle")
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(150), turn.wait())
            .await
            .unwrap();
        assert!(result.is_err() || matches!(result, Ok(Outcome::Failed)));
        let failure = resumed.snapshot().last_failure.unwrap();
        assert_eq!(failure.class, kar::FailureClass::RateLimited, "{failure:?}");
        let raw = resumed.records();
        assert!(raw.iter().any(|r| {
            r.native
                .pointer("/error/data/errorName")
                .is_some_and(|v| v == "provider.rate-limit")
        }));
        assert_eq!(failure.resets_at_ms, None);
    }
    resumed.close().await.unwrap();
}
#[tokio::test]
#[ignore = "requires installed Pi; uses a loopback model fixture"]
async fn actual_pi_turn_and_resume() {
    real_protocol_turn("pi").await
}
#[tokio::test]
#[ignore = "requires installed OpenCode; uses a loopback model fixture"]
async fn actual_opencode_turn_and_resume() {
    real_protocol_turn("opencode").await
}

#[tokio::test]
#[ignore = "requires OpenCode 1 or OpenCode 2 with ACP steering; uses a loopback model"]
async fn actual_opencode_steering_reaches_next_model_step() {
    live_steering("opencode", true).await;
}

#[tokio::test]
#[ignore = "requires Claude Code; checks both tool-loop and separate native turns with a loopback model"]
async fn actual_claude_steering_and_reconnect() {
    live_steering("claude", true).await;
    live_steering("claude", false).await;
}

async fn live_steering(provider: &str, tool: bool) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use kar::chat::{ChatReplica, ChatStore, Item, Role, TurnStatus};
    use uuid::Uuid;
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().canonicalize().unwrap().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("fixture.txt"), "fixture works").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = requests.clone();
    let folder = work.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let calls = calls.clone();
            let folder = folder.clone();
            std::thread::spawn(move || {
                let _ = respond_in_workspace(conn, None, Some(&calls), Some(&folder), false);
            });
        }
    });
    let mut opts = SessionOptions::new(&work);
    for key in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        opts.env
            .insert(key.into(), dir.path().join(key).into_os_string());
    }
    if provider == "claude" {
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"permissions":{"allow":["Write"]}}"#,
        )
        .unwrap();
        opts.model = Some("sonnet".into());
        for (k, v) in [
            ("CLAUDE_CONFIG_DIR", dir.path().to_str().unwrap()),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("ANTHROPIC_BASE_URL", &endpoint),
            ("ANTHROPIC_API_KEY", "fixture-key-only"),
            ("ANTHROPIC_AUTH_TOKEN", ""),
        ] {
            opts.env.insert(k.into(), v.into());
        }
    } else {
        opts.model = Some("fixture/fixture".into());
        opencode_model(&mut opts, &endpoint);
    }
    let store = ChatStore::open(dir.path().join("chats")).unwrap();
    let env = opts.env.clone();
    let session = store.create(provider, None, opts).await.unwrap();
    let chat = session.info().id;
    let mut raw = session.subscribe(None).unwrap();
    let listen = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listen.local_addr().unwrap());
    let router = kar::chat::event_router(store.clone(), chat);
    let http = tokio::spawn(async move { axum::serve(listen, router).await.unwrap() });
    let mut a = ChatReplica::from_snapshot(store.feed_snapshot(chat).unwrap());
    let mut b = ChatReplica::from_snapshot(store.feed_snapshot(chat).unwrap());
    let initial = a.cursor().to_string();
    let mut online = sse::SseClient::open(&url, &initial, None).await;
    let mut disconnecting = sse::SseClient::open(&url, &b.cursor().to_string(), None).await;
    let turn_id = Uuid::new_v4();
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEElEQVR4nGMQdm2HIwbiOABjYw3x5BI1EwAAAABJRU5ErkJggg==";
    let file = work.join("follow up.png");
    std::fs::write(&file, STANDARD.decode(png).unwrap()).unwrap();
    let steer = async {
        for (replica, client) in [(&mut a, &mut online), (&mut b, &mut disconnecting)] {
            while replica.turns().is_empty() {
                assert!(replica.apply(client.next().await).unwrap());
            }
            assert_eq!(replica.turns()[0].id, turn_id);
            assert_eq!(replica.turns()[0].status, TurnStatus::Running);
        }
        drop(disconnecting);
        tokio::time::timeout(Duration::from_secs(15), async {
            while !work.join("requested").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("native model request did not reach the fixture");
        let image = kar::InputImage::read(&file).unwrap();
        session
            .steer_images("OAR-LIVE-FOLLOWUP", vec![image])
            .await
            .unwrap();
        if provider == "claude" {
            session.steer("OAR-LIVE-SECOND").await.unwrap();
        }
        std::fs::write(work.join("release"), "").unwrap();
    };
    let (turn, ()) = tokio::time::timeout(Duration::from_secs(35), async {
        let prompt = if tool {
            "OAR-LIVE-STEER-TOOL"
        } else {
            "OAR-LIVE-STEER-TEXT"
        };
        tokio::join!(session.send(turn_id, prompt), steer)
    })
    .await
    .expect("native turn did not finish");
    assert_eq!(turn.unwrap().status, TurnStatus::Completed);
    let expected_status = if tool {
        kar::SteerStatus::Accepted
    } else {
        kar::SteerStatus::FollowUp
    };
    let expected_results = if tool { 1 } else { 2 };
    let mut rec = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut ended = 0;
        let mut accepted = false;
        while let Some(record) = raw.next().await {
            for event in &record.events {
                ended += usize::from(matches!(event, Event::TurnEnded { .. }));
                accepted |= matches!(
                    event,
                    Event::SteeringInput {
                        status,
                        ..
                    } if *status == expected_status
                );
            }
            rec.push(record);
            if ended == expected_results && accepted {
                return;
            }
        }
        panic!("native runtime exited before acknowledging the steering input");
    })
    .await
    .expect("missing native steering acknowledgement");
    let uri = url::Url::from_file_path(&file).unwrap();
    assert!(rec.iter().any(|r| if provider == "claude" {
        r.native
            .pointer("/message/content/0/source/data")
            .and_then(Value::as_str)
            == Some(png)
    } else {
        r.native
            .pointer("/params/prompt/0/uri")
            .and_then(Value::as_str)
            == Some(uri.as_str())
    }));
    assert_eq!(
        rec.iter()
            .flat_map(|r| &r.events)
            .filter(|e| matches!(e, Event::TurnEnded { .. }))
            .count(),
        expected_results
    );
    assert_eq!(
        rec.iter()
            .flat_map(|r| &r.events)
            .filter(|e| matches!(e,Event::SteeringInput {text,status,..} if text=="OAR-LIVE-FOLLOWUP" && *status == expected_status))
            .count(),
        1
    );
    assert!(requests.lock().unwrap().iter().any(|messages| {
        let body = messages.to_string();
        body.contains("tool_result") == tool
            && body.matches("OAR-LIVE-FOLLOWUP").count() == 1
            && (provider != "claude" || body.matches("OAR-LIVE-SECOND").count() == 1)
            && body.contains(png)
    }));
    session.close().await.unwrap();
    let tip = store.feed_snapshot(chat).unwrap();
    let mut resumed = sse::SseClient::open(&url, &initial, Some(&b.cursor().to_string())).await;
    for (replica, client) in [(&mut a, &mut online), (&mut b, &mut resumed)] {
        while replica.cursor() != tip.cursor {
            let event = client.next().await;
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        assert_eq!(replica.turns(), tip.history.turns);
        assert_eq!(replica.notifications(), tip.history.notifications);
    }
    assert_eq!(tip.history.turns.len(), expected_results);
    let turn = &tip.history.turns[0];
    assert_eq!(turn.status, TurnStatus::Completed);
    let ids: Vec<_> = rec
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match e {
            Event::SteeringInput {
                input_id, status, ..
            } if *status == expected_status => Some(*input_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), if provider == "claude" { 2 } else { 1 });
    assert_eq!(turn.steered_inputs.len(), ids.len());
    assert!(
        ids.iter()
            .all(|id| turn.steered_inputs.get(id) == Some(&expected_status))
    );
    let reply = if tool {
        assert!(tip.history.notifications.is_empty());
        if provider == "claude" {
            assert_eq!(
                turn.native_result_inputs,
                [vec![turn.id], ids.clone()].concat()
            );
        }
        turn
    } else {
        assert_eq!(turn.native_result_inputs, [turn.id]);
        assert!(tip.history.notifications.is_empty());
        let follow_up = &tip.history.turns[1];
        assert_eq!(follow_up.steered_from, Some(turn.id));
        assert_eq!(follow_up.status, TurnStatus::Completed);
        assert_eq!(follow_up.id, ids[0]);
        assert_eq!(follow_up.native_result_inputs, ids);
        assert!(follow_up.fork_anchor.is_some());
        assert!(follow_up.checkpoint.as_ref().unwrap().after_tree.is_some());
        follow_up
    };
    if provider == "claude" {
        let messages: Vec<_> = reply
            .items
            .iter()
            .filter_map(|i| match &i.item {
                Item::Message {
                    role: Role::User,
                    text,
                    provider_id: Some(id),
                    ..
                } if ids.iter().any(|u| u.to_string() == *id) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(messages, ["OAR-LIVE-FOLLOWUP", "OAR-LIVE-SECOND"]);
        assert!(
            matches!(store.enqueue(chat, ids[1], "OAR-LIVE-SECOND"), Err(kar::Error::Refused(reason)) if reason.contains("steering input"))
        );
        if !tool {
            assert_eq!(
                turn.items
                    .iter()
                    .filter(|i| matches!(
                        i.item,
                        Item::Message {
                            role: Role::User,
                            ..
                        }
                    ))
                    .count(),
                1
            );
        }
    }
    for (activity, role, expected) in [
        (reply, Role::User, "OAR-LIVE-FOLLOWUP"),
        (reply, Role::Assistant, "saw the follow-up"),
    ] {
        assert_eq!(activity.items.iter().filter(|item| matches!(&item.item, Item::Message {role: r, text, ..} if *r == role && text == expected)).count(), 1);
    }
    assert_eq!(
        turn.items
            .iter()
            .filter(|item| matches!(&item.item, Item::Tool { activity, .. } if activity.finished))
            .count(),
        usize::from(tool)
    );
    if provider == "opencode" {
        let mut events = store.follow(chat, Some(initial.parse().unwrap())).unwrap();
        let mut cut = None;
        while events.cursor() != tip.cursor {
            let event = events.recv().await.unwrap();
            if matches!(&event.update, kar::chat::ChatUpdate::Frame {frame,..} if frame.edits.iter().any(|edit|
                matches!(edit, kar::chat::ChatEdit::Text{role:Role::Assistant,..})))
            {
                cut = Some(event.cursor);
            }
        }
        let recovered = interrupted_copy(
            &dir.path().join("chats"),
            &dir.path().join("steer-restart"),
            chat,
            cut.unwrap(),
        );
        let resumed = recovered
            .resume_with(chat, |o| o.env = env.clone())
            .await
            .unwrap();
        let repaired = resumed.send(turn_id, "OAR-LIVE-STEER-TOOL").await.unwrap();
        assert_eq!(repaired.status, TurnStatus::Completed);
        assert_eq!(repaired.steered_inputs, turn.steered_inputs);
        assert_eq!(repaired.items, turn.items);
        resumed.close().await.unwrap();
    }
    if provider == "claude" {
        let capture = dir.path().join("recording.jsonl");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&capture).unwrap());
        for r in &rec {
            serde_json::to_writer(&mut file, r.as_ref()).unwrap();
            writeln!(file).unwrap();
        }
        file.flush().unwrap();
        let imported = store
            .import_recording(provider, &capture, &work, None)
            .unwrap();
        let history = store.feed_snapshot(imported.id).unwrap().history;
        assert_eq!(history.turns.len(), expected_results);
        for (actual, original) in history.turns.iter().zip(&tip.history.turns) {
            assert_eq!(
                (actual.id, actual.steered_from),
                (original.id, original.steered_from)
            );
            assert_eq!(actual.steered_inputs, original.steered_inputs);
            assert_eq!(actual.items, original.items);
            assert!(actual.checkpoint.is_none());
        }
    }
    if !tool {
        let changed = |turn: &kar::chat::ChatTurn| {
            turn.checkpoint
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|f| f.path == std::path::Path::new("followup.txt"))
        };
        assert!(!changed(turn));
        assert!(
            changed(reply),
            "follow-up did not capture the native Write; file exists: {}",
            work.join("followup.txt").exists()
        );
        assert_eq!(
            std::fs::read_to_string(work.join("followup.txt")).unwrap(),
            "from the independent turn"
        );
        let version = runtime(provider).unwrap().version().await.unwrap();
        let version = version
            .split_whitespace()
            .find_map(|s| semver::Version::parse(s).ok())
            .unwrap();
        for (selected, includes_steer) in [(turn, false), (reply, true)] {
            let fork = store
                .fork_at_with(chat, selected.id, |options| options.env = env.clone())
                .await;
            if version < semver::Version::new(2, 1, 323) {
                assert!(
                    matches!(fork, Err(kar::Error::Unsupported(reason)) if reason.contains("2.1.323"))
                );
                store
                    .rollback(
                        chat,
                        kar::chat::CheckpointPoint {
                            turn: selected.id,
                            before: false,
                        },
                        false,
                    )
                    .unwrap();
                assert_eq!(work.join("followup.txt").exists(), includes_steer);
                continue;
            }
            let branch = fork.unwrap();
            assert_eq!(work.join("followup.txt").exists(), includes_steer);
            let history = branch.history().unwrap();
            assert_eq!(!history.turns[0].steered_inputs.is_empty(), includes_steer);
            branch
                .send(Uuid::new_v4(), "OAR-BRANCH-PROBE")
                .await
                .unwrap();
            let model = requests.lock().unwrap().last().unwrap().to_string();
            assert!(model.contains("OAR-BRANCH-PROBE") && model.contains("OAR-LIVE-STEER-TEXT"));
            assert_eq!(model.contains("OAR-LIVE-FOLLOWUP"), includes_steer);
            assert_eq!(model.contains("OAR-LIVE-SECOND"), includes_steer);
            branch.close().await.unwrap();
        }
    }
    http.abort();
}

#[tokio::test]
#[ignore = "requires a Claude Code binary; sends a tool request from a loopback fixture"]
async fn actual_claude_waits_for_host_approval() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir(&config).unwrap();
    let work = dir.path().canonicalize().unwrap().join("work");
    std::fs::create_dir(&work).unwrap();
    let target = work.join("approved.txt");
    std::fs::write(
        config.join("settings.json"),
        json!({"permissions":{"ask":["Write","Bash"]}}).to_string(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let path = target.clone();
    let capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let questions_trace = capture.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let path = path.clone();
            let capture = capture.clone();
            std::thread::spawn(move || {
                let _ = respond_with_tool(conn, Some(&path), Some(&capture));
            });
        }
    });
    let mut options = SessionOptions::new(&work);
    options.approvals = kar::ApprovalMode::Host;
    options
        .env
        .insert("OAR_TEST_SECRET".into(), "fixture approved".into());
    options.env.insert("HOME".into(), dir.path().into());
    options
        .env
        .insert("CLAUDE_CONFIG_DIR".into(), config.into_os_string());
    for (k, v) in [
        ("ANTHROPIC_API_KEY", "fixture-key-only"),
        ("ANTHROPIC_AUTH_TOKEN", ""),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("ANTHROPIC_BASE_URL", &endpoint),
    ] {
        options.env.insert(k.into(), v.into());
    }
    let store = kar::chat::ChatStore::open(dir.path().join("store")).unwrap();
    let session = store.create("claude", None, options).await.unwrap();
    let mut stream = session.subscribe(None).unwrap();
    let request = uuid::Uuid::new_v4();
    let initial = store.feed_snapshot(session.info().id).unwrap();
    let initial_cursor = initial.cursor;
    let mut replica = kar::chat::ChatReplica::from_snapshot(initial);
    let mut feed = store
        .follow(session.info().id, Some(initial_cursor))
        .unwrap();
    let approval = async {
        let mut fragments = String::new();
        while let Some(r) = stream.next().await {
            for event in &r.events {
                if let Event::ToolInputDelta { id, delta } = event
                    && id == "toolu_approval"
                {
                    fragments.push_str(delta);
                }
            }
            if let Some(id) = r.events.iter().find_map(|e| match e {
                Event::ApprovalRequested { id, method } if method == "can_use_tool" => {
                    Some(id.clone())
                }
                _ => None,
            }) {
                assert!(!target.exists());
                assert!(
                    !fragments.is_empty(),
                    "no argument fragments before native approval"
                );
                assert!(!fragments.contains("fixture approved"));
                assert_eq!(r.native["request"]["tool_name"], "Write");
                let req = session
                    .snapshot()
                    .pending_requests
                    .into_iter()
                    .find(|p| p.id == id)
                    .unwrap();
                assert_eq!(req.kind, kar::RequestKind::Permission);
                assert_eq!(req.body["input"]["content"], "[redacted]");
                session
                    .decide(&req, kar::PermissionDecision::AllowOnce)
                    .await
                    .unwrap();
                assert!(session.snapshot().pending_requests.is_empty());
                return;
            }
        }
        panic!("runtime exited before asking for approval")
    };
    let (turn, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            session.send(request, "Write the requested test file."),
            approval
        )
    })
    .await
    .expect("approval or native turn timed out");
    let turn = turn.unwrap();
    assert_eq!(turn.status, kar::chat::TurnStatus::Completed);
    // Reconnect while replaying the actual argument stream. The first partial card is
    // applied once, then the completed native object replaces the partial text in place.
    let tip = store.feed_snapshot(session.info().id).unwrap().cursor;
    let mut reconnected = false;
    while replica.cursor() != tip {
        let event = feed.recv().await.unwrap();
        replica.apply(event.clone()).unwrap();
        if !reconnected && replica.turns().last().is_some_and(|t| t.items.iter().any(|i|
            matches!(&i.item, kar::chat::Item::Tool { id, activity } if id == "toolu_approval" && activity.input_partial))) {
            assert!(!replica.apply(event).unwrap());
            feed = store.follow(session.info().id, Some(replica.cursor())).unwrap();
            reconnected = true;
        }
    }
    assert!(reconnected);
    let tools: Vec<_> = replica
        .turns()
        .last()
        .unwrap()
        .items
        .iter()
        .filter_map(|i| match &i.item {
            kar::chat::Item::Tool { id, activity } if id == "toolu_approval" => Some(activity),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1);
    assert!(!tools[0].input_partial);
    assert_eq!(tools[0].input.as_ref().unwrap()["content"], "[redacted]");
    let files = turn.checkpoint.unwrap().files;
    assert!(
        files
            .iter()
            .any(|c| c.path == std::path::Path::new("approved.txt")
                && c.kind == kar::chat::ChangeKind::Added)
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "fixture approved"
    );
    let rewind = session
        .rollback(
            kar::chat::CheckpointPoint {
                turn: request,
                before: true,
            },
            false,
        )
        .await
        .unwrap();
    assert!(rewind.applied);
    assert!(!target.exists());
    let snapshot = store.feed_snapshot(session.info().id).unwrap();
    let mut feed = store
        .follow(session.info().id, Some(snapshot.cursor))
        .unwrap();
    let mut replica = kar::chat::ChatReplica::from_snapshot(snapshot);
    let control =
        async {
            let mut stopped = None;
            while let Some(record) = stream.next().await {
                for e in &record.events {
                    match e {
                        Event::ApprovalRequested { id, .. }
                            if record.native["request"]["tool_name"] == "Bash" =>
                        {
                            let req = session
                                .snapshot()
                                .pending_requests
                                .into_iter()
                                .find(|r| &r.id == id)
                                .unwrap();
                            assert_eq!(req.body["input"]["command"], "sleep 20");
                            session
                                .decide(&req, kar::PermissionDecision::AllowOnce)
                                .await
                                .unwrap();
                        }
                        Event::Task {
                            id,
                            change:
                                kar::TaskChange::Started {
                                    task_type: kar::TaskType::Shell,
                                    details,
                                },
                        } if details.tool_call_id.as_deref() == Some("toolu_bg") => {
                            session.stop_task(id).await.unwrap();
                            stopped = Some(id.clone());
                        }
                        Event::Task { id, .. }
                            if stopped.as_ref() == Some(id)
                                && session.tasks().unwrap().iter().any(|t| {
                                    t.id == *id && t.status == kar::TaskStatus::Stopped
                                }) =>
                        {
                            return id.clone();
                        }
                        _ => {}
                    }
                }
            }
            panic!("Claude exited before confirming its background task stopped")
        };
    let (background, id) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            session.send(uuid::Uuid::new_v4(), "oar-background-task"),
            control
        )
    })
    .await
    .expect("no terminal background task report");
    assert_eq!(background.unwrap().status, kar::chat::TurnStatus::Completed);
    let after = store.feed_snapshot(session.info().id).unwrap();
    while replica.cursor() != after.cursor {
        replica.apply(feed.recv().await.unwrap()).unwrap();
    }
    assert_eq!(replica.tasks(), after.history.tasks);
    assert!(
        replica
            .tasks()
            .iter()
            .any(|t| t.id == id && t.status == kar::TaskStatus::Stopped)
    );
    let questions = async {
        loop {
            replica.apply(feed.recv().await.unwrap()).unwrap();
            let Some(request) = replica.pending_requests().first().cloned() else {
                continue;
            };
            if request.kind != kar::RequestKind::Question {
                continue;
            }
            let form = request.question_form().unwrap();
            assert_eq!(form.fields[0].label, "Which color?");
            assert_eq!(form.fields[1].kind, kar::FieldKind::MultiSelect);
            assert_eq!(
                store
                    .feed_snapshot(session.info().id)
                    .unwrap()
                    .history
                    .pending_requests
                    .as_slice(),
                std::slice::from_ref(&request)
            );
            let reply = kar::QuestionReply::Submit {
                values: [
                    ("0".into(), json!("Blue")),
                    ("1".into(), json!(["Unit", "Replay"])),
                ]
                .into_iter()
                .collect(),
            };
            session
                .answer_question(&request, reply.clone())
                .await
                .unwrap();
            assert!(session.answer_question(&request, reply).await.is_err());
            loop {
                replica.apply(feed.recv().await.unwrap()).unwrap();
                if replica.pending_requests().is_empty() {
                    return;
                }
            }
        }
    };
    let (answer, ()) = tokio::time::timeout(Duration::from_secs(18), async {
        tokio::join!(
            session.send(uuid::Uuid::new_v4(), "oar-form-question"),
            questions
        )
    })
    .await
    .expect("Claude did not finish the native user question");
    assert_eq!(answer.unwrap().status, kar::chat::TurnStatus::Completed);
    assert!(
        questions_trace
            .lock()
            .unwrap()
            .iter()
            .any(|messages| messages
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"] == "user")
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .any(|part| part["type"] == "tool_result"
                    && part["tool_use_id"] == "toolu_form"
                    && part.to_string().contains("Unit, Replay")
                    && part.to_string().contains("Blue"))),
        "next model call did not receive the user's choices"
    );
    session.close().await.unwrap();
}

async fn durable_cli_roundtrip(provider: &str) {
    use tokio::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let workspace = base.join("work");
    let other = base.join("elsewhere");
    let home = base.join("home");
    let cli_home = base.join("config");
    for path in [&workspace, &other, &home, &cli_home] {
        std::fs::create_dir(path).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let requests = log.clone();
    let folder = workspace.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let requests = requests.clone();
            let folder = folder.clone();
            std::thread::spawn(move || {
                let _ = respond_in_workspace(conn, None, Some(&requests), Some(&folder), false);
            });
        }
    });
    std::fs::write(
        cli_home.join("config.toml"),
        format!(
            r#"model = "gpt-5.1"
model_provider = "fixture"
[model_providers.fixture]
name = "loopback"
base_url = "http://127.0.0.1:{port}/v1"
env_key = "OAR_FIXTURE_KEY"
wire_api = "responses"
"#
        ),
    )
    .unwrap();
    let cmd = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
        cmd.kill_on_drop(true)
            .current_dir(&other)
            .env("HOME", &home)
            .env("CODEX_HOME", &cli_home)
            .env("CLAUDE_CONFIG_DIR", &cli_home)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("ANTHROPIC_API_KEY", "local-fixture-key")
            .env("ANTHROPIC_AUTH_TOKEN", "")
            .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
            .env("OAR_FIXTURE_KEY", "local-fixture-key")
            .env("OAR_CHAT_DIR", base.join("store"))
            .args(["chat", "--json"]);
        cmd
    };
    let first = tokio::time::timeout(
        Duration::from_secs(45),
        cmd()
            .args([
                "new",
                provider,
                "orange-original",
                "--name",
                "oar-fixture",
                "--effort",
                "low",
                "--service-tier",
                "default",
                "--add-dir",
                "../elsewhere",
                "--cwd",
            ])
            .arg(&workspace)
            .args((provider == "codex").then_some("--goals"))
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let history = kar::chat::ChatStore::open(base.join("store")).unwrap();
    let id = history.latest().unwrap();
    let native = history.history(id).unwrap().chat.native_id;
    if provider == "codex" {
        let goal = async |args: &[&str]| {
            let output = tokio::time::timeout(
                Duration::from_secs(15),
                cmd().arg("goal").args(args).output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.status.success(),
                "goal: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        };
        let snapshot = history.feed_snapshot(id).unwrap();
        assert_eq!(snapshot.history.goal, Some(None));
        let mut updates = history.follow(id, Some(snapshot.cursor)).unwrap();
        let mut replica = kar::chat::ChatReplica::from_snapshot(snapshot);
        let added = goal(&[
            "set",
            "Track work across restarts",
            "--status",
            "paused",
            "--token-budget",
            "100000",
        ])
        .await;
        assert_eq!(added["status"], "paused");
        assert_eq!(added["token_budget"], 100_000);
        assert_eq!(
            goal(&["read"]).await["objective"],
            "Track work across restarts"
        );
        let last = history.feed_snapshot(id).unwrap();
        while replica.cursor() != last.cursor {
            replica.apply(updates.recv().await.unwrap()).unwrap();
        }
        assert_eq!(
            replica.goal(),
            last.history.goal.as_ref().map(Option::as_ref)
        );
        assert_eq!(
            replica.goal().flatten().unwrap().objective,
            "Track work across restarts"
        );
        let refused = cmd()
            .args(["goal", "set", "   ", "--status", "active"])
            .output()
            .await
            .unwrap();
        assert!(!refused.status.success());
        assert_eq!(goal(&["read"]).await["status"], "paused");
        assert!(goal(&["clear"]).await.is_null());
        assert!(goal(&["clear"]).await.is_null());
        assert_eq!(history.history(id).unwrap().goal, Some(None));
        assert_eq!(history.history(id).unwrap().turns.len(), 1);
    }
    let blue_id = uuid::Uuid::new_v4();
    for _ in 0..2 {
        assert_eq!(
            history
                .enqueue(id, blue_id, "blue-continuation")
                .unwrap()
                .status,
            kar::chat::InputStatus::Pending
        );
    }
    let second = tokio::time::timeout(Duration::from_secs(45), cmd().arg("resume").output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let result = history.history(id).unwrap();
    assert_eq!(result.chat.workspace, workspace);
    assert_eq!(result.chat.native_id, native);
    assert_eq!(result.turns.len(), 2);
    let first_turn = result.turns[0].id.to_string();
    if provider == "claude" {
        // Recreate a host crash just after the durable Begin. Keep the real
        // vendor transcript intact and replay the crash in a second host store.
        let lost = base.join("lost-claude-ack");
        let restored = kar::chat::ChatStore::open(&lost).unwrap();
        for suffix in ["lock", "inputs", "inputs.lock"] {
            let name = format!("{id}.{suffix}");
            std::fs::copy(base.join("store").join(&name), lost.join(&name)).unwrap();
        }
        let name = format!("{id}.oarchat");
        let ledger = std::fs::read_to_string(base.join("store").join(&name)).unwrap();
        let mut prefix = String::new();
        for line in ledger.lines() {
            prefix.push_str(line);
            prefix.push('\n');
            let row: Value = serde_json::from_str(line).unwrap();
            if row["kind"] == "begin" && row["id"] == blue_id.to_string() {
                break;
            }
        }
        std::fs::write(lost.join(name), prefix).unwrap();
        assert_eq!(
            restored.queued_inputs(id).unwrap()[0].status,
            kar::chat::InputStatus::Uncertain
        );
        let calls = log.lock().unwrap().len();
        let snapshot = restored.feed_snapshot(id).unwrap();
        let mut feed = restored.follow(id, Some(snapshot.cursor)).unwrap();
        let mut replay = kar::chat::ChatReplica::from_snapshot(snapshot);
        for _ in 0..2 {
            let output = tokio::time::timeout(
                Duration::from_secs(30),
                cmd()
                    .env("OAR_CHAT_DIR", &lost)
                    .args(["resume", "--chat", &id.to_string()])
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                restored.queued_inputs(id).unwrap()[0].status,
                kar::chat::InputStatus::Sent
            );
        }
        let snapshot = restored.feed_snapshot(id).unwrap();
        let mut receipts = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while replay.cursor() != snapshot.cursor {
                let event = feed.recv().await.unwrap();
                if matches!(event.update, kar::chat::ChatUpdate::InputSent { id } if id == blue_id)
                {
                    receipts += 1;
                }
                assert!(replay.apply(event.clone()).unwrap());
                assert!(!replay.apply(event).unwrap());
            }
        })
        .await
        .unwrap();
        assert_eq!(receipts, 1);
        assert_eq!(replay.turns(), snapshot.history.turns);
        assert_eq!(replay.turns()[1].status, kar::chat::TurnStatus::Interrupted);
        assert_eq!(log.lock().unwrap().len(), calls);
    }
    for (index, turn) in result.turns.into_iter().enumerate() {
        assert_eq!(turn.status, kar::chat::TurnStatus::Completed);
        let replies: Vec<_> = turn
            .items
            .iter()
            .filter_map(|i| match &i.item {
                kar::chat::Item::Message {
                    role: kar::chat::Role::Assistant,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let answer = if index == 0 {
            "rust-wire-ok"
        } else {
            "blue-answer-only"
        };
        if provider == "claude" {
            assert_eq!(replies, [answer, "second-block"]);
        } else {
            assert_eq!(replies, [answer]);
        }
    }
    assert_eq!(
        std::fs::read_to_string(workspace.join("version")).unwrap(),
        "blue"
    );
    let run = |args: Vec<String>| {
        let mut command = cmd();
        command.args(args);
        async move {
            let output = tokio::time::timeout(Duration::from_secs(30), command.output())
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        }
    };
    if provider == "claude" {
        let count = history.list().unwrap().len();
        let parent = history.history(id).unwrap().turns;
        let answer = run(vec![
            "ask".into(),
            "violet-side-question".into(),
            "--chat".into(),
            id.to_string(),
        ])
        .await;
        assert_eq!(answer, "violet-answer");
        assert_eq!(history.history(id).unwrap().turns, parent);
        assert_eq!(history.list().unwrap().len(), count);
        let requests = log.lock().unwrap();
        assert!(requests.iter().any(|r| {
            let body = r.to_string();
            body.contains("violet-side-question")
                && body.contains("blue-continuation")
                && body.contains("orange-original")
        }));
    }
    let fork = run(vec![
        "fork".into(),
        first_turn.clone(),
        "--chat".into(),
        id.to_string(),
    ])
    .await;
    let child_id = history.latest().unwrap();
    assert_ne!(child_id, id);
    assert_eq!(fork["chat"]["id"], child_id.to_string());
    assert_ne!(fork["chat"]["native_id"], native);
    assert_eq!(fork["turns"].as_array().unwrap().len(), 1);
    assert_eq!(fork["turns"][0]["id"], first_turn);
    assert_eq!(
        std::fs::read_to_string(workspace.join("version")).unwrap(),
        "orange"
    );
    run(vec!["resume".into(), "green-branch".into()]).await;
    assert_eq!(history.history(id).unwrap().turns.len(), 2);
    assert_eq!(history.history(child_id).unwrap().turns.len(), 2);
    for text in ["olive-queued", "teal-after-restart"] {
        run(vec![
            "enqueue".into(),
            text.into(),
            "--chat".into(),
            child_id.to_string(),
        ])
        .await;
    }
    run(vec!["resume".into(), "--chat".into(), child_id.to_string()]).await;
    assert!(
        history
            .queued_inputs(child_id)
            .unwrap()
            .iter()
            .all(|input| input.status == kar::chat::InputStatus::Sent)
    );
    let nested = run(vec![
        "fork".into(),
        first_turn.clone(),
        "--chat".into(),
        child_id.to_string(),
    ])
    .await;
    assert_eq!(nested["chat"]["forked_from"]["chat"], child_id.to_string());
    assert_eq!(nested["turns"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.join("version")).unwrap(),
        "orange"
    );
    if provider == "codex" {
        use kar::chat::{ChatReplica, TurnStatus};
        let session = history
            .resume_with(id, |options| {
                options
                    .env
                    .insert("HOME".into(), home.clone().into_os_string());
                options
                    .env
                    .insert("CODEX_HOME".into(), cli_home.clone().into_os_string());
                options
                    .env
                    .insert("OAR_FIXTURE_KEY".into(), "local-fixture-key".into());
            })
            .await
            .unwrap();
        let snapshot = history.feed_snapshot(id).unwrap();
        let before = snapshot.history.turns.len();
        let mut feed = history.follow(id, Some(snapshot.cursor)).unwrap();
        let mut replica = ChatReplica::from_snapshot(snapshot);
        let gate = base.join("native-goal-gate");
        std::fs::create_dir(&gate).unwrap();
        let change = |status: &str| kar::GoalSetOptions {
            objective: "Count native progress".into(),
            status: Some(status.into()),
            token_budget: Some(100_000),
        };
        assert_eq!(
            session.set_goal(change("active")).await.unwrap().status,
            "active"
        );
        tokio::time::timeout(Duration::from_secs(9), async {
            loop {
                replica.apply(feed.recv().await.unwrap()).unwrap();
                if replica.turns().len() > before
                    && replica.turns().last().unwrap().status == TurnStatus::Running
                {
                    break;
                }
            }
            while !gate.join("requested").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("active native goal did not start a turn");
        let turn = replica.turns().last().unwrap();
        assert!(turn.native_continuation);
        let id_native = turn.id;
        assert!(matches!(
            session.send(uuid::Uuid::new_v4(), "overlap").await,
            Err(kar::Error::Busy)
        ));
        assert_eq!(
            session.set_goal(change("paused")).await.unwrap().status,
            "paused"
        );
        assert_eq!(
            session.history().unwrap().turns.last().unwrap().status,
            TurnStatus::Running
        );
        std::fs::write(gate.join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                replica.apply(feed.recv().await.unwrap()).unwrap();
                if replica
                    .turns()
                    .last()
                    .and_then(|t| t.checkpoint.as_ref()?.after_tree.as_ref())
                    .is_some()
                {
                    break;
                }
            }
        })
        .await
        .expect("native goal turn did not finish its checkpoint");
        let done = replica.turns().last().unwrap();
        assert_eq!((done.id, done.status), (id_native, TurnStatus::Completed));
        assert!(done.items.iter().any(|i| matches!(
            &i.item,
            kar::chat::Item::Message {
                role: kar::chat::Role::Assistant,
                ..
            }
        )));
        assert!(!done.items.iter().any(|i| matches!(
            &i.item,
            kar::chat::Item::Message {
                role: kar::chat::Role::User,
                ..
            }
        )));
        assert!(
            done.checkpoint
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|f| f.path == std::path::Path::new("goal-progress"))
        );
        assert_eq!(session.read_goal().await.unwrap().unwrap().status, "paused");
        session.clear_goal().await.unwrap();
        session.close().await.unwrap();
    }
    // Native requests must retain the included sources and omit the excluded ones on both opens.
    if provider == "codex" {
        let file = cli_home.join("config.toml");
        std::fs::write(
            &file,
            format!(
                "developer_instructions = \"EXISTING-CODEX-RULE\"\n{}\n[mcp_servers.rustprobe]\ncommand = {1}\nargs = [\"mcp\",\"--runtimes\",\"codex\"]\ndisabled_tools = [\"close\",\"interrupt\",\"list\",\"run\",\"runtimes\",\"send\",\"spawn\"]\n[mcp_servers.keepprobe]\ncommand = {1}\nargs = [\"mcp\",\"--runtimes\",\"codex\"]\nenabled_tools = [\"list\"]\n",
                std::fs::read_to_string(&file).unwrap(),json!(env!("CARGO_BIN_EXE_kar"))
            ),
        )
        .unwrap();
    } else {
        std::fs::write(cli_home.join("CLAUDE.md"), "EXCLUDE-USER-RULE").unwrap();
        std::fs::write(workspace.join("CLAUDE.md"), "KEEP-PROJECT-RULE").unwrap();
    }
    // A new CLI process drains the saved queue with the original schema and session.
    let first = cmd()
        .args([
            "new",
            provider,
            "schema-probe-1",
            "--system-prompt",
            "HOST-BASE-RS",
            "--append-system-prompt",
            "HOST-APPEND-RS",
        ])
        .args(
            (provider == "claude")
                .then_some(["--exclude-user-config", "--disallow-tool", "Bash"])
                .into_iter()
                .flatten(),
        )
        .args(
            (provider == "codex")
                .then_some(["--disallow-tool", "mcp__rustprobe__wait"])
                .into_iter()
                .flatten(),
        )
        .arg("--cwd")
        .arg(&workspace)
        .arg("--output-schema")
        .arg(output_schema().to_string())
        .output()
        .await
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let structured = history.latest().unwrap();
    let snapshot = history.feed_snapshot(structured).unwrap();
    let original_id = snapshot.history.chat.native_id.clone();
    let mut feed = history.follow(structured, Some(snapshot.cursor)).unwrap();
    let mut replica = kar::chat::ChatReplica::from_snapshot(snapshot);
    let queued = cmd()
        .args([
            "enqueue",
            "schema-probe-2",
            "--chat",
            &structured.to_string(),
        ])
        .output()
        .await
        .unwrap();
    assert!(queued.status.success());
    let checkpoint = history.feed_snapshot(structured).unwrap();
    let queue = history.queued_inputs(structured).unwrap();
    assert_eq!(
        queue.last().unwrap().status,
        kar::chat::InputStatus::Pending
    );
    let calls = log.lock().unwrap().len();
    let empty = base.join("missing-native-store");
    std::fs::create_dir(&empty).unwrap();
    std::fs::copy(cli_home.join("config.toml"), empty.join("config.toml")).unwrap();
    let rejected = cmd()
        .env(
            if provider == "codex" {
                "CODEX_HOME"
            } else {
                "CLAUDE_CONFIG_DIR"
            },
            &empty,
        )
        .args(["resume", "--chat", &structured.to_string()])
        .output()
        .await
        .unwrap();
    assert!(!rejected.status.success());
    let diagnostic: Value =
        serde_json::from_slice(&rejected.stderr).expect("structured open error");
    assert_eq!(diagnostic["error"]["kind"], "session_not_found");
    assert_eq!(diagnostic["error"]["session_id"], original_id);
    assert_eq!(
        history.feed_snapshot(structured).unwrap().cursor,
        checkpoint.cursor
    );
    assert_eq!(
        serde_json::to_value(history.queued_inputs(structured).unwrap()).unwrap(),
        serde_json::to_value(&queue).unwrap()
    );
    assert_eq!(log.lock().unwrap().len(), calls);
    let drained = cmd()
        .args(["resume", "--chat", &structured.to_string()])
        .output()
        .await
        .unwrap();
    assert!(
        drained.status.success(),
        "{}",
        String::from_utf8_lossy(&drained.stderr)
    );
    let snapshot = history.feed_snapshot(structured).unwrap();
    assert_eq!(snapshot.history.chat.native_id, original_id);
    tokio::time::timeout(Duration::from_secs(5), async {
        while replica.cursor() != snapshot.cursor {
            let e = feed.recv().await.unwrap();
            assert!(replica.apply(e.clone()).unwrap());
            assert!(!replica.apply(e).unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(replica.turns(), snapshot.history.turns);
    assert_eq!(replica.turns().len(), 2);
    for turn in replica.turns() {
        assert_eq!(turn.status, kar::chat::TurnStatus::Completed);
        if provider == "claude" {
            assert_eq!(turn.structured_output, Some(json!({"answer":4})));
        } else {
            assert!(turn.structured_output.is_none());
            assert!(turn.items.iter().any(|item| matches!(&item.item, kar::chat::Item::Message {role:kar::chat::Role::Assistant,text,..} if text == r#"{"answer":4}"#)));
        }
    }
    if provider == "claude" {
        let answer = run(vec![
            "ask".into(),
            "isolated-side-question".into(),
            "--chat".into(),
            structured.to_string(),
        ])
        .await;
        assert_eq!(answer, "isolated-answer");
        assert_eq!(history.history(structured).unwrap().turns, replica.turns());
    }
    let log = log.lock().unwrap();
    if provider == "codex" {
        for marker in ["schema-probe-1", "schema-probe-2"] {
            let probe = log
                .iter()
                .find(|v| v["probe"].as_str().is_some_and(|p| p.contains(marker)))
                .unwrap();
            let tools = probe["mcp_names"].as_array().unwrap();
            assert!(
                tools
                    .iter()
                    .any(|t| t.as_str().is_some_and(|s| s.starts_with("mcp__keepprobe")))
                    && !tools
                        .iter()
                        .any(|t| t.as_str().is_some_and(|s| s.starts_with("mcp__rustprobe"))),
                "native MCP namespace selection: {probe}"
            );
            let instruction = |key: &str| {
                probe[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r == "developer" || r == "system")
            };
            assert!(
                probe["base_instruction"] == true || instruction("base_roles"),
                "Codex base: {probe}"
            );
            assert!(
                probe["global_existing"] == true || instruction("existing_roles"),
                "Codex prior: {probe}"
            );
            assert!(
                probe["global_append"] == true || instruction("append_roles"),
                "Codex append: {probe}"
            );
        }
    }
    let teal = log
        .iter()
        .find(|b| b.to_string().contains("teal-after-restart"))
        .unwrap()
        .to_string();
    assert!(
        teal.contains("olive-queued")
            && teal.contains("green-branch")
            && teal.contains("orange-original")
            && !teal.contains("blue-continuation")
            && !teal.contains("blue-answer-only")
            && !teal.contains("violet-side-question")
    );
    let green = log
        .iter()
        .find(|b| b.to_string().contains("green-branch"))
        .unwrap()
        .to_string();
    assert!(
        green.contains("orange-original")
            && green.contains("rust-wire-ok")
            && !green.contains("blue-continuation")
            && !green.contains("blue-answer-only"),
        "incorrect forked model context: {green}"
    );
    assert!(
        log.iter().any(|body| {
            let s = body.to_string();
            s.contains("orange-original")
                && s.contains("blue-continuation")
                && s.contains("rust-wire-ok")
        }),
        "model did not receive prior context"
    );
}
#[tokio::test]
#[ignore = "installed Codex CLI, isolated home and loopback model"]
async fn actual_codex_durable_chat_across_processes() {
    durable_cli_roundtrip("codex").await;
}
#[tokio::test]
#[ignore = "Claude Code 2.1.326+ for the side-question part, isolated home and loopback model"]
async fn actual_claude_durable_chat_across_processes() {
    durable_cli_roundtrip("claude").await;
}

#[tokio::test]
#[ignore = "requires a Cursor CLI with ACP subagents; initializes two isolated connections without a model"]
async fn actual_cursor_acp_subagents_handshake() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let program = std::env::var_os("OAR_CURSOR_BIN").unwrap_or_else(|| "cursor-agent".into());
    for opt_in in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = tokio::process::Command::new(&program);
        cmd.arg("acp")
            .current_dir(dir.path())
            .env_clear()
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("XDG_CONFIG_HOME", dir.path())
            .env("APPDATA", dir.path())
            .env("LOCALAPPDATA", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for key in ["PATH", "SystemRoot", "TMPDIR", "TMP", "TEMP"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        let mut proc = cmd.spawn().unwrap();
        let caps = if opt_in {
            json!({"_meta":{"subagents":true}})
        } else {
            json!({})
        };
        let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":caps}});
        let mut stdin = proc.stdin.take().unwrap();
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            Duration::from_secs(12),
            tokio::io::BufReader::new(proc.stdout.take().unwrap()).read_line(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        proc.kill().await.unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["id"], 1);
        let caps = &value["result"]["agentCapabilities"]["sessionCapabilities"];
        assert!(caps["list"].is_object());
        assert_eq!(caps["subagents"].is_object(), opt_in, "{caps}");
    }
}
