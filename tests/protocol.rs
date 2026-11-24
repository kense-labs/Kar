use kar::{ApprovalMode, Error, Event, Outcome, Protocol, Runtime, SessionOptions};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    time::Duration,
};
use tempfile::TempDir;
#[path = "protocol/account.rs"]
mod account;
#[path = "protocol/acp_failures.rs"]
mod acp_failures;
#[path = "protocol/acp_model_list.rs"]
mod acp_model_list;
#[path = "protocol/antigravity_model_list.rs"]
mod antigravity_model_list;
#[path = "protocol/antigravity_steering.rs"]
mod antigravity_steering;
#[path = "support/billing.rs"]
mod billing_fixture;
#[path = "protocol/child_models.rs"]
mod child_models;
#[path = "protocol/claude_input.rs"]
mod claude_input;
#[path = "protocol/codex_catalog.rs"]
mod codex_catalog;
#[path = "protocol/crew.rs"]
mod crew;
#[path = "protocol/crew_options.rs"]
mod crew_options;
#[path = "protocol/dsh_skills.rs"]
mod dsh_skills;
#[path = "protocol/environment.rs"]
mod environment;
#[path = "protocol/grok_turns.rs"]
mod grok_turns;
#[path = "protocol/installation.rs"]
mod installation;
#[path = "protocol/inventory_batch.rs"]
mod inventory_batch;
#[path = "protocol/launch_args.rs"]
mod launch_args;
#[path = "protocol/mcp_handoff.rs"]
mod mcp_handoff;
#[path = "protocol/opencode_models.rs"]
mod opencode_models;
#[path = "protocol/opencode_steering.rs"]
mod opencode_steering;
#[path = "protocol/opening.rs"]
mod opening;
#[path = "protocol/orientation.rs"]
mod orientation;
#[path = "protocol/pi_registry.rs"]
mod pi_registry;
#[path = "support/private_inputs.rs"]
mod private_inputs;
#[path = "protocol/provider.rs"]
mod provider;
#[path = "support/public_readbacks.rs"]
mod public_readbacks;
#[path = "protocol/qoder.rs"]
mod qoder;
#[path = "protocol/qwen_providers.rs"]
mod qwen_providers;

fn record_routing(protocol: &str, task: &str) {
    let Some(dir) = std::env::var_os("KAR_TEST_MCP_ROUTING") else {
        return;
    };
    assert_eq!(task, format!("route/{protocol}"));
    assert_eq!(std::env::var("KAR_AGENT_DEPTH").as_deref(), Ok("1"));
    let path = std::path::Path::new(&dir).join(format!("received-{protocol}"));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();
    file.write_all(task.as_bytes()).unwrap();
}

fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn peer_with_child() {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("resource-child")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    peer("acp");
    drop(child.stdin.take());
    child.wait().unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn process_resources() -> kar::Result<()> {
    let (_dir, _, opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    let rt = Runtime::new(
        "fixture",
        std::env::current_exe()?,
        &["resource-peer"],
        Protocol::Acp,
    );
    let session = rt.open(opts).await?;
    let before = session.records().len();
    let (a, b) = tokio::join!(session.resources(), session.resources());
    let reading = a.expect("local process tree");
    assert_eq!(Some(reading), b);
    assert_eq!(reading.processes, 2);
    assert!(reading.rss > 0 && reading.scope.is_none());
    assert_eq!(session.records().len(), before);
    session.close().await?;
    assert!(session.resources().await.is_none());
    Ok(())
}
fn peer(protocol: &str) {
    let mut prompt = Value::Null;
    let mut name = String::new();
    let mut compactions = 0;
    let arg: Vec<_> = std::env::args().collect();
    let id = arg
        .windows(2)
        .find(|w| matches!(w[0].as_str(), "--resume" | "--session-id"))
        .map_or("session-1", |w| &w[1]);
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let req = &v["id"];
        match protocol {
            "acp" | "codex" => {
                let rpc = |data: Value| emit(json!({"id":req,"jsonrpc":"2.0","result":data}));
                match v["method"].as_str() {
                    Some("initialize") => {
                        let mut capabilities = json!({"loadSession":true});
                        if std::env::var_os("KAR_TEST_APPEND_CAPABILITY").is_some() {
                            capabilities["sessionCapabilities"] = json!({"list":{}});
                            capabilities["_meta"] =
                                json!({"antigravity":{"systemPrompt":{"append":true}}});
                        }
                        rpc(json!({"protocolVersion":1,"agentCapabilities":capabilities}))
                    }
                    Some("initialized") => {}
                    Some("config/read") => {
                        assert!(v["params"]["cwd"].is_string());
                        rpc(
                            json!({"config":{"developer_instructions":"native-private-key","mcp_servers":{
                            "primary":{"command":"private-server-path","disabled_tools":["old"]},
                            "amb":{"command":"private-server-path"}, "amb__iguous":{} }}}),
                        );
                    }
                    Some("thread/start" | "thread/resume") => {
                        if v["params"]["baseInstructions"] == "replace-test" {
                            assert_eq!(
                                v["params"]["developerInstructions"],
                                "native-private-key\n\nappend-test"
                            );
                            let servers = &v["params"]["config"]["mcp_servers"];
                            assert_eq!(
                                servers["primary"],
                                json!({"disabled_tools":["old","more"]})
                            );
                            assert_eq!(servers["session"]["http_headers"]["X-Tiny"], "abc");
                            assert_eq!(servers["session"]["disabled_tools"], json!(["another"]));
                        }
                        rpc(
                            json!({"thread":{"id":"session-1"},"model":"demo","reasoningEffort":"medium","serviceTier":null}),
                        );
                    }
                    Some("thread/fork") => {
                        match v
                            .pointer("/params/threadId")
                            .and_then(Value::as_str)
                            .unwrap()
                        {
                            "source" => {
                                rpc(json!({"thread":{"id":"copy","turns":[{"id":"anchor"}]}}))
                            }
                            "same" => rpc(json!({"thread":{"id":"same"}})),
                            _ => rpc(
                                json!({"thread":{"id":"wrong-cut","turns":[{"id":"past-anchor"}]}}),
                            ),
                        }
                    }
                    Some("session/list") => rpc(
                        json!({"sessions":[{"sessionId":"session-1","cwd":std::env::current_dir().unwrap()}]}),
                    ),
                    Some("session/new" | "session/load") => {
                        if std::env::var_os("KAR_TEST_APPEND_CAPABILITY").is_some() {
                            assert_eq!(
                                v.pointer("/params/_meta/antigravity/systemPrompt/append"),
                                Some(&json!("custom"))
                            );
                        }
                        let mut info = json!({"sessionId":"session-1","models":{"currentModelId":"demo","availableModels":[{"modelId":"demo"}]}});
                        if std::env::var_os("KAR_TEST_BUDDY_FLAGS").is_some() {
                            let file = &arg
                                .windows(2)
                                .find(|pair| pair[0] == "--system-prompt-file")
                                .unwrap()[1];
                            assert_eq!(std::fs::read_to_string(file).unwrap(), "buddy-rules");
                            assert!(
                                arg.windows(2)
                                    .any(|pair| pair == ["--disallowedTools", "Read,Write"])
                            );
                            info["promptFile"] = file.as_str().into();
                        }
                        rpc(info);
                    }
                    Some("_x.ai/compact_conversation") => {
                        assert_eq!(v["params"], json!({"sessionId":"session-1"}));
                        match compactions {
                            0 => rpc(json!({})),
                            1 => rpc(Value::Null),
                            n => emit(
                                json!({"id":req,"error":{"message":"compact response","data":{
                                    "kind":match n {2 => "compact_cancelled", 3 => "compact_failed", _ => "other"}
                                }}}),
                            ),
                        }
                        compactions += 1;
                    }
                    Some("model/list") => rpc(json!({"data":[{"id":"demo"}]})),
                    Some("turn/start" | "session/prompt") => {
                        prompt = req.clone();
                        name = v
                            .pointer("/params/prompt/0/text")
                            .or_else(|| v.pointer("/params/input/0/text"))
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .into();
                        if name.starts_with("route/") {
                            record_routing(protocol, &name);
                            name = "normal".into();
                        }
                        if let Some(script) = name.strip_prefix("failure-fixture ") {
                            let script: Value = serde_json::from_str(script).unwrap();
                            for frame in script["frames"].as_array().into_iter().flatten() {
                                emit(frame.clone());
                            }
                            if let Some(error) = script.get("error") {
                                emit(json!({"id":req,"error":error}));
                            } else {
                                rpc(script["result"].clone());
                            }
                            continue;
                        }
                        if name.starts_with("structured-") {
                            assert_eq!(protocol, "acp");
                            assert!(
                                v["params"]["_meta"]["promptId"]
                                    .as_str()
                                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
                                    .is_some()
                            );
                            assert_eq!(
                                v["params"]["_meta"]["outputSchema"],
                                json!({"type":"object"})
                            );
                            let meta = match name.as_str() {
                                "structured-null" => json!({"structuredOutput":null,
                                    "totalTokens":17,"contextWindow":128,
                                    "usage":{"inputTokens":1024,"outputTokens":4}}),
                                "structured-error" => {
                                    json!({"structuredOutput":17,"structuredOutputError":"wrong type"})
                                }
                                "structured-missing" => json!({}),
                                _ => unreachable!(),
                            };
                            rpc(json!({"stopReason":"end_turn","_meta":meta}));
                            continue;
                        }
                        if name == "malformed" {
                            println!("invalid-json");
                            std::io::stdout().flush().unwrap();
                            continue;
                        }
                        if name == "exit" {
                            return;
                        }
                        if name == "refuse" {
                            emit(json!({"id":req,"error":{"code":-32602,"message":"bad input"}}));
                            continue;
                        }
                        if protocol == "codex" {
                            rpc(json!({"turn":{"id":"t"}}));
                            emit(
                                json!({"method":"turn/started","params":{"threadId":"session-1","turn":{"id":"t"}}}),
                            );
                        }
                        if name == "child-selections" {
                            child_models::rpc_frames(protocol);
                            if protocol == "codex" {
                                emit(
                                    json!({"method":"turn/completed","params":{"threadId":"session-1","turn":{"id":"t","status":"completed"}}}),
                                );
                            } else {
                                rpc(json!({"stopReason":"end_turn"}));
                            }
                            continue;
                        }
                        if name == "wait" {
                            if protocol == "codex" {
                                emit(
                                    json!({"method":"item/started","params":{"threadId":"session-1","turnId":"t","item":{"type":"collabAgentToolCall","id":"tool-1","senderThreadId":"session-1","receiverThreadIds":["child-1"],"tool":"spawnAgent"}}}),
                                );
                                emit(
                                    json!({"method":"thread/settings/updated","params":{"threadId":"child-1","threadSettings":{"model":"child-model","effort":"low"}}}),
                                );
                                emit(
                                    json!({"method":"turn/completed","params":{"threadId":"child-1","turn":{"id":"child-turn","status":"completed"}}}),
                                );
                            }
                            continue;
                        }
                        if name == "owned-children" {
                            for (parent, child, path) in [
                                ("session-1", "nested", "/root/builder"),
                                ("nested", "leaf", "/root/builder/leaf"),
                            ] {
                                emit(
                                    json!({"method":"item/completed","params":{"threadId":parent,"item":{"type":"subAgentActivity","id":"spawn","kind":"started","agentThreadId":child,"agentPath":path}}}),
                                );
                            }
                            emit(
                                json!({"method":"item/completed","params":{"threadId":"leaf","item":{"type":"subAgentActivity","kind":"interacted","agentThreadId":"session-1","agentPath":"/root"}}}),
                            );
                            for (child, turn) in
                                [("outsider", "not-owned"), ("leaf", "old"), ("leaf", "new")]
                            {
                                emit(
                                    json!({"method":"turn/started","params":{"threadId":child,"turn":{"id":turn}}}),
                                );
                            }
                            emit(
                                json!({"method":"turn/completed","params":{"threadId":"leaf","turn":{"id":"old","status":"completed"}}}),
                            );
                            continue;
                        }
                        if name == "fine-grants" {
                            emit(
                                json!({"id":93,"method":"item/permissions/requestApproval","params":{"threadId":"session-1","permissions":{"fileSystem":{"read":["red","green"]},"network":{"enabled":true}}}}),
                            );
                            continue;
                        }
                        if name == "reused-question" {
                            let ask = |word| {
                                emit(
                                    json!({"id":92,"method":"item/tool/requestUserInput","params":{"questions":[{"id":"q","question":word,"isSecret":true}]}}),
                                )
                            };
                            ask("First");
                            emit(
                                json!({"method":"serverRequest/resolved","params":{"requestId":92}}),
                            );
                            ask("Second");
                            continue;
                        }
                        let method = if protocol == "acp" {
                            "session/request_permission"
                        } else {
                            "item/commandExecution/requestApproval"
                        };
                        emit(
                            json!({"id":"approval-1","method":method,"params":{"sessionId":"session-1","options":[{"optionId":"yes","kind":"allow_once"}]}}),
                        );
                    }
                    Some("session/cancel") => {
                        emit(json!({"id":prompt,"result":{"stopReason":"cancelled"}}))
                    }
                    Some("turn/interrupt")
                        if name == "owned-children" && v["params"]["threadId"] != "session-1" =>
                    {
                        assert_eq!(v["params"], json!({"threadId":"leaf","turnId":"new"}));
                        rpc(json!({}));
                        emit(
                            json!({"method":"turn/completed","params":{"threadId":"leaf","turn":{"id":"new","status":"interrupted"}}}),
                        );
                        emit(
                            json!({"method":"item/completed","params":{"threadId":"nested","item":{"type":"subAgentActivity","kind":"interrupted","agentThreadId":"leaf","agentPath":"/root/builder/leaf"}}}),
                        );
                    }
                    Some("turn/interrupt") => {
                        rpc(json!({}));
                        emit(
                            json!({"method":"turn/completed","params":{"threadId":"session-1","turn":{"id":"t","status":"interrupted"}}}),
                        );
                    }
                    Some("turn/steer") if v["params"]["input"][0]["text"] == "fallback" => {
                        emit(
                            json!({"id":req,"error":{"code":-32602,"message":"native control refused"}}),
                        );
                    }
                    Some("turn/steer") => rpc(json!({"turnId":"t"})),
                    None if v["id"] == 93 => {
                        assert_eq!(
                            v["result"],
                            json!({"permissions":{"fileSystem":{"read":["green"]}},"scope":"turn"})
                        );
                        emit(
                            json!({"id":94,"method":"item/commandExecution/requestApproval","params":{"threadId":"session-1","proposedExecpolicyAmendment":["cargo","test"]}}),
                        );
                    }
                    None if v["id"] == 94 => {
                        assert_eq!(
                            v["result"],
                            json!({"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["cargo","test"]}}})
                        );
                        emit(
                            json!({"method":"turn/completed","params":{"threadId":"session-1","turn":{"id":"t","status":"completed"}}}),
                        );
                    }
                    None if v["id"] == 92 => {
                        assert_eq!(
                            v.pointer("/result/answers/q/answers/0"),
                            Some(&json!("answer"))
                        );
                        emit(
                            json!({"method":"turn/completed","params":{"threadId":"session-1","turn":{"id":"t","status":"completed"}}}),
                        );
                    }
                    None if v["id"] == "approval-1" => {
                        let allowed = if protocol == "acp" {
                            v.pointer("/result/outcome/optionId") == Some(&json!("yes"))
                        } else {
                            v.pointer("/result/decision") == Some(&json!("accept"))
                        };
                        assert_eq!(allowed, name == "host");
                        if protocol == "acp" {
                            emit(
                                json!({"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}),
                            );
                            emit(json!({"id":prompt,"result":{"stopReason":"end_turn"}}));
                        } else {
                            emit(
                                json!({"method":"item/agentMessage/delta","params":{"threadId":"session-1","delta":"hello"}}),
                            );
                            emit(
                                json!({"method":"turn/completed","params":{"threadId":"session-1","turn":{"status":"completed","id":"t"}}}),
                            );
                        }
                    }
                    other => panic!("unexpected RPC {other:?}"),
                }
            }
            "claude" | "qwen" => match v["type"].as_str() {
                Some("control_request") if protocol == "qwen" => {
                    let body = match v.pointer("/request/subtype").and_then(Value::as_str) {
                        Some("initialize") => {
                            assert_eq!(
                                v.pointer("/request/mcpServers/example/headers/X-Test"),
                                Some(&json!("tiny"))
                            );
                            json!({"session_id":id,"effort_status":{"applied":true,"effort":"high","override":null}})
                        }
                        Some("get_available_models") => json!({"models":[{"id":"native-qwen"}]}),
                        other => panic!("unexpected Qwen control {other:?}"),
                    };
                    emit(
                        json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":body}}),
                    );
                }
                Some("control_request")
                    if v["request"]["subtype"] == "initialize"
                        && v["request_id"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("kar-private-"))
                        && std::env::var_os("KAR_TEST_COMPACT_CATALOG").is_some() =>
                {
                    let mut commands = vec![json!({"name":"compact","builtin":true})];
                    if std::env::var("KAR_TEST_COMPACT_CATALOG").as_deref() == Ok("shadow") {
                        commands.push(json!({"name":"compact","builtin":false}));
                    }
                    let packet = json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],
                        "response":{"commands":commands,"private_detail":"catalog-private-fixture"}}});
                    emit(packet.clone());
                    emit(packet);
                }
                Some("user")
                    if v.pointer("/message/content/0/text")
                        .is_some_and(|t| t == "/compact") =>
                {
                    let mode = std::env::var("KAR_TEST_COMPACT_CATALOG").unwrap();
                    assert_ne!(mode, "shadow", "overridden command was dispatched");
                    let uuid = &v["uuid"];
                    emit(
                        json!({"type":"command_lifecycle","command_uuid":uuid,"state":"started","session_id":id}),
                    );
                    emit(
                        json!({"type":"result","session_id":id,"user_message_uuid":"another-command","is_error":false}),
                    );
                    emit(
                        json!({"type":"system","session_id":id,"subtype":"compact_boundary","compact_metadata":{"trigger":"auto"}}),
                    );
                    emit(
                        json!({"type":"system","session_id":id,"parent_tool_use_id":"child","subtype":"compact_boundary","compact_metadata":{"trigger":"manual"}}),
                    );
                    if mode == "changed" {
                        emit(json!({"type":"system","session_id":id,"subtype":"commands_changed"}));
                        emit(
                            json!({"type":"system","session_id":id,"subtype":"compact_boundary","compact_metadata":{"trigger":"manual"}}),
                        );
                    }
                    if mode == "failed" {
                        emit(
                            json!({"type":"system","session_id":id,"subtype":"status","compact_result":"failed"}),
                        );
                    }
                    emit(
                        json!({"type":"result","session_id":id,"user_message_uuid":uuid,"is_error":false}),
                    );
                }
                Some("control_request") => emit(
                    json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":{"models":[{"value":"demo"}]}}}),
                ),
                Some("user") => {
                    if std::env::var_os("KAR_TEST_MCP_ROUTING").is_some() {
                        let content = &v["message"]["content"];
                        record_routing(
                            protocol,
                            content
                                .as_str()
                                .or_else(|| content[0]["text"].as_str())
                                .unwrap(),
                        );
                    }
                    if v["message"]["content"] == "child-selections"
                        || v["message"]["content"][0]["text"] == "child-selections"
                    {
                        child_models::claude_frames(id);
                    }
                    if protocol == "qwen" {
                        assert!(uuid::Uuid::parse_str(v["uuid"].as_str().unwrap()).is_ok());
                    }
                    emit(
                        json!({"type":"stream_event","session_id":id,"event":{"type":"content_block_delta","delta":{"text":"hello"}}}),
                    );
                    emit(
                        json!({"type":"result","session_id":id,"subtype":"success","is_error":false,"usage":{"input_tokens":2}}),
                    );
                }
                other => panic!("unexpected Claude frame {other:?}"),
            },
            "pi" => match v["type"].as_str() {
                Some("get_session_stats") => emit(
                    json!({"type":"response","id":req,"command":"get_session_stats","success":true,"data":{"contextUsage":{"tokens":10,"contextWindow":200,"percent":5}}}),
                ),
                Some("get_state") => emit(
                    json!({"type":"response","id":req,"command":"get_state","success":true,"data":{"sessionId":"session-1"}}),
                ),
                Some("get_available_models") => emit(
                    json!({"type":"response","id":req,"command":"get_available_models","success":true,"data":{"models":[{"id":"demo"}]}}),
                ),
                Some("prompt") => {
                    emit(json!({"type":"response","id":req,"command":"prompt","success":true}));
                    if v["message"] == "hold steer" {
                        continue;
                    }
                    if v["message"] == "two dialogs" {
                        emit(
                            json!({"type":"extension_ui_request","id":"expired","method":"select","title":"Which?","options":["A","B"],"timeout":30}),
                        );
                        emit(
                            json!({"type":"extension_ui_request","id":"live","method":"confirm","title":"Continue?","message":"Ready?"}),
                        );
                        continue;
                    }
                    emit(
                        json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hello"}}),
                    );
                    emit(
                        json!({"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop"}]}),
                    );
                }
                Some("steer") => {
                    // None of these lookalikes confirms this steer.
                    let id = req.as_str().unwrap().parse::<u64>().unwrap();
                    for (id, command) in [
                        (json!(id), "steer"),
                        (json!(format!("0{id}")), "steer"),
                        (req.clone(), "prompt"),
                    ] {
                        emit(
                            json!({"type":"response","id":id,"command":command,"success":true,"data":{"disposition":"handled"}}),
                        );
                    }
                    if v["message"] == "missing success" {
                        emit(
                            json!({"type":"response","id":req,"command":"steer","data":{"disposition":"handled"}}),
                        );
                    } else {
                        emit(
                            json!({"type":"response","id":req,"command":"steer","success":false,"error":"extension refused steer"}),
                        );
                    }
                }
                Some("abort") => {
                    let id: u64 = req.as_str().unwrap().parse().unwrap();
                    for (wire, command) in [
                        (json!(id), "abort"),
                        (json!(format!("0{id}")), "abort"),
                        (req.clone(), "steer"),
                    ] {
                        emit(json!({"type":"response","id":wire,"command":command,"success":true}));
                    }
                    emit(
                        json!({"type":"response","id":req,"command":"abort","success":false,"error":"native abort refused"}),
                    );
                }
                Some("extension_ui_response") => {
                    assert_eq!(
                        v,
                        json!({"type":"extension_ui_response","id":"live","confirmed":false})
                    );
                    emit(
                        json!({"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop"}]}),
                    );
                }
                other => panic!("unexpected Pi frame {other:?}"),
            },
            _ => panic!("unknown peer"),
        }
    }
}
async fn qwen_http_mcp_version() {
    let (root, mut runtime, options) = rig(Protocol::Qwen, ApprovalMode::Host);
    let basename = format!("qwen{}", std::env::consts::EXE_SUFFIX);
    let program = root.path().join(&basename);
    std::fs::hard_link(&runtime.program, &program)
        .or_else(|_| std::fs::copy(&runtime.program, &program).map(|_| ()))
        .unwrap();
    runtime.program = program;
    let package = root.path().join("package.json");
    for (release, supported) in [("0.25.9", false), ("0.26.0", true)] {
        let metadata =
            json!({"name":"@qwen-code/qwen-code","version":release,"bin":{"qwen":basename}});
        std::fs::write(&package, metadata.to_string()).unwrap();
        match runtime.open(options.clone()).await {
            Ok(session) if supported => {
                assert_eq!(
                    session.capabilities().mcp_transports,
                    Some([kar::McpTransport::Stdio, kar::McpTransport::Http].as_slice())
                );
                session.close().await.unwrap();
            }
            Err(kar::Error::Unsupported(reason)) if !supported => {
                assert!(reason.contains("HTTP MCP"))
            }
            _ => panic!("Qwen {release} HTTP transport decision disagrees with the installation"),
        }
    }
}

fn rig(protocol: Protocol, mode: ApprovalMode) -> (TempDir, Runtime, SessionOptions) {
    let dir = TempDir::new().unwrap();
    let name = format!("{protocol:?}").to_ascii_lowercase();
    let runtime = Runtime::new(
        "fixture",
        std::env::current_exe().unwrap(),
        &["peer", &name],
        protocol,
    );
    let mut opts = SessionOptions::new(dir.path());
    if protocol == Protocol::Qwen {
        opts.mcp_servers.insert(
            "example".into(),
            kar::McpServer::Http {
                url: "http://127.0.0.1:1".into(),
                headers: [("X-Test".into(), "tiny".into())].into(),
            },
        );
    }
    if protocol == Protocol::Pi {
        let root = dir.path().join("pi");
        opts.env
            .insert("KAR_PI_AGENT_DIR".into(), root.clone().into_os_string());
        let cwd = dir.path().canonicalize().unwrap();
        let slug = cwd
            .to_string_lossy()
            .trim_start_matches(['/', char::from(92)])
            .replace(['/', char::from(92), ':'], "-");
        let folder = root.join("sessions").join(format!("--{slug}--"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("saved.jsonl"),
            json!({"type":"session","id":"session-1","cwd":cwd}).to_string(),
        )
        .unwrap();
    }
    opts.approvals = mode;
    opts.handshake_timeout = Duration::from_secs(4);
    (dir, runtime, opts)
}
fn check_opening(session: &kar::Session, requested: kar::PlacementRequest) {
    let records = session.records();
    let first = &records[0];
    let second = &records[1];
    assert_eq!((first.seq, second.seq), (0, 1));
    assert_eq!(
        (first.kind, first.direction, second.kind),
        (
            kar::RecordKind::Request,
            kar::Direction::ToRuntime,
            kar::RecordKind::Response
        )
    );
    let Some(kar::Opening::Open {
        request_id,
        placement,
    }) = &first.opening
    else {
        panic!("missing opening request")
    };
    assert_eq!(*placement, requested);
    assert_eq!(second.response_to.as_ref(), Some(request_id));
    assert_eq!(first.control_request_id(), second.response_to.as_deref());
    assert_eq!(
        second.opening,
        Some(kar::Opening::Opened {
            placement: kar::SessionPlacement::Isolated
        })
    );
    assert_eq!(session.placement(), Some(kar::SessionPlacement::Isolated));
    assert_eq!(
        kar::SessionPlacement::from_records(records.iter().map(AsRef::as_ref)),
        session.placement()
    );
    for record in [first, second] {
        assert_eq!(record.session_id.as_deref(), Some(session.id()));
        assert!(
            record.agent_path.is_empty() && record.native.is_null() && record.events.is_empty()
        );
    }
}
async fn exercise(protocol: Protocol) -> kar::Result<()> {
    let (_dir, r, opts) = rig(protocol, ApprovalMode::Deny);
    assert_eq!(r.execution(), Some(kar::Execution::Process));
    assert!(
        matches!(r.with_placement(kar::PlacementRequest::Shared).open(opts.clone()).await,Err(Error::Unsupported(reason)) if reason.contains("placement"))
    );
    let session = r.open(opts.clone()).await?;
    check_opening(&session, kar::PlacementRequest::Auto);
    if protocol == Protocol::Qwen {
        assert_eq!(session.effort().as_deref(), Some("high"));
        let records = session.records();
        let init = &records
            .iter()
            .find(|r| r.native.pointer("/request/mcpServers/example").is_some())
            .expect("native initialize request")
            .native;
        assert_eq!(
            init.pointer("/request/mcpServers/example/headers/X-Test"),
            Some(&json!("[redacted]"))
        );
    }
    if matches!(protocol, Protocol::Codex | Protocol::Acp) {
        assert_eq!(session.model().as_deref(), Some("demo"));
    }
    assert_eq!(session.models().await?.as_array().unwrap().len(), 1);
    let readbacks = r.open(opts.clone()).await?;
    public_readbacks::check(&readbacks).await?;
    public_readbacks::stream::check(&readbacks).await?;
    let active = r.open(opts.clone()).await?;
    public_readbacks::stream::dispose_mid_turn(&active).await?;
    let mut stream = session.subscribe(None)?;
    let result = session.prompt("normal").await?.wait().await?;
    assert_eq!(result, Outcome::Completed);
    let mut last = None;
    while let Some(rec) = stream.next().await {
        if rec
            .events
            .iter()
            .any(|e| matches!(e,Event::TextDelta{text} if text=="hello"))
        {
            last = Some(rec.cursor());
            break;
        }
    }
    let cursor = last.unwrap();
    let mut replay = session.subscribe(Some(&cursor))?;
    assert_eq!(replay.next().await.unwrap().seq, cursor.after_seq + 1);
    let prior = session.records();
    assert!(prior.iter().enumerate().all(|(i, r)| r.seq == i as u64));
    let id = session.id().to_owned();
    session.close().await?;
    let mut view = kar::observe::SessionView::default();
    for record in session.records() {
        view.apply(&record)?;
    }
    let mut fold = kar::observe::SessionReducer::new(&id);
    for record in session.records() {
        fold.apply(&record)?;
    }
    assert_eq!(fold.snapshot(), &session.snapshot());
    assert!(matches!(
        session.status(),
        kar::observe::AgentStatus::Idle { released: true, .. }
    ));
    assert!(view.closed);
    assert!(view.agents.values().any(|v| v.messages == ["hello"]));
    assert!(view.agents.values().any(|v| v.user_messages == ["normal"]));
    assert!(view.apply(&prior[0]).is_err());
    while replay.next().await.is_some() {}
    let mut resume = opts;
    resume.resume = Some(id.clone());
    let next = r
        .with_placement(kar::PlacementRequest::Isolated)
        .open(resume)
        .await?;
    check_opening(&next, kar::PlacementRequest::Isolated);
    assert_eq!(id, next.id());
    assert_ne!(next.stream_id(), cursor.stream_id);
    assert!(matches!(
        next.subscribe(Some(&cursor)),
        Err(Error::InvalidCursor)
    ));
    let resumed = next.prompt("normal").await?;
    let input = resumed.input_id().to_string();
    assert_eq!(resumed.wait().await?, Outcome::Completed);
    let records = next.records();
    let prompt = records
        .iter()
        .find(|r| r.prompt.as_ref().is_some_and(|p| p.input_id == input))
        .unwrap();
    let receipt = records
        .iter()
        .find(|r| {
            r.direction == kar::Direction::Host
                && r.kind == kar::RecordKind::Response
                && r.response_to.as_deref() == prompt.control_request_id()
        })
        .unwrap();
    let terminal = records
        .iter()
        .find(|r| {
            r.kind == kar::RecordKind::Frame
                && r.agent_path.is_empty()
                && r.events.contains(&Event::TurnEnded {
                    outcome: Outcome::Completed,
                })
        })
        .unwrap();
    assert!(prompt.seq < receipt.seq && receipt.seq < terminal.seq);
    for row in [prompt, receipt, terminal] {
        assert_eq!(row.session_id.as_deref(), Some(id.as_str()));
    }
    next.close().await
}
async fn controls(p: Protocol) -> kar::Result<()> {
    let (_d, r, opts) = rig(p, ApprovalMode::Host);
    let session = r.open(opts).await?;
    let mut records = session.subscribe(None)?;
    let turn = session.prompt("host").await?;
    assert!(session.status().is_running());
    assert!(matches!(session.prompt("racing").await, Err(Error::Busy)));
    let original = turn.admission().unwrap();
    let rejected = session
        .submit_prompt(Some(turn.input_id()), "host", vec![])
        .await?;
    let kar::PromptDecision::Rejected {
        receipt,
        error: Error::Busy,
    } = rejected
    else {
        panic!("an overlapping retry must have its own busy receipt")
    };
    assert_ne!(
        receipt.request.control_request_id(),
        original.request.control_request_id()
    );
    assert_eq!(
        receipt.request.control_request_id(),
        receipt.response.control_response_to()
    );
    assert_eq!(receipt.response.native["code"], "busy");
    let empty = session
        .submit_prompt(Some(turn.input_id()), "", vec![])
        .await?;
    assert!(
        matches!(empty, kar::PromptDecision::Rejected { receipt, error: Error::Unsupported(_) }
        if receipt.response.native["code"] == "unsupported")
    );
    loop {
        let rec = records.next().await.unwrap();
        if let Some(id) = rec.events.iter().find_map(|e| match e {
            Event::ApprovalRequested { id, .. } => Some(id.clone()),
            _ => None,
        }) {
            let req = session
                .snapshot()
                .pending_requests
                .into_iter()
                .find(|r| r.id == id)
                .unwrap();
            assert_eq!(req.kind, kar::RequestKind::Permission);
            let mut stale = req.clone();
            stale.cursor.after_seq += 1;
            assert!(
                session
                    .decide(&stale, kar::PermissionDecision::AllowOnce)
                    .await
                    .is_err()
            );
            assert!(session.answer_request(&stale, Value::Null).await.is_err());
            assert!(
                req.decisions()
                    .contains(&kar::PermissionDecision::AllowOnce)
            );
            session
                .decide(&req, kar::PermissionDecision::AllowOnce)
                .await?;
            assert!(session.snapshot().pending_requests.is_empty());
            assert!(session.respond(id, Value::Null).await.is_err());
            break;
        }
    }
    assert_eq!(turn.wait().await?, Outcome::Completed);
    if p == Protocol::Codex {
        let root_turn = session.prompt("owned-children").await?;
        loop {
            let r = records.next().await.unwrap();
            if r.native
                .pointer("/params/turn/id")
                .is_some_and(|id| id == "old")
                && r.native["method"] == "turn/completed"
            {
                break;
            }
        }
        assert_eq!(session.tasks().len(), 2);
        for forbidden in ["session-1", "outsider", "nested"] {
            assert!(session.stop_task(forbidden).await.is_err(), "{forbidden}");
        }
        session.stop_task("leaf").await?;
        loop {
            let r = records.next().await.unwrap();
            if r.native
                .pointer("/params/item/kind")
                .is_some_and(|v| v == "interrupted")
            {
                break;
            }
        }
        let tasks = session.tasks();
        let leaf = tasks.iter().find(|t| t.id == "leaf").unwrap();
        assert_eq!(leaf.source.session_id.as_deref(), Some("nested"));
        assert_eq!(leaf.status, kar::TaskStatus::Stopped);
        assert!(session.stop_task("leaf").await.is_err());
        assert!(session.status().is_running());
        session.abort().await?;
        assert_eq!(root_turn.wait().await?, Outcome::Aborted);
    }
    let mut stalls = session.observe_stalls(Duration::from_millis(50))?;
    let turn = session.prompt("wait").await?;
    if p == Protocol::Codex {
        loop {
            let r = records.next().await.unwrap();
            if r.seq
                > session
                    .records()
                    .iter()
                    .rev()
                    .find(|r| r.native["method"] == "turn/start")
                    .unwrap()
                    .seq
                && r.session_id.as_deref() == Some("child-1")
                && r.events
                    .iter()
                    .any(|e| matches!(e, Event::TurnEnded { .. }))
            {
                break;
            }
        }
        assert!(session.status().is_running());
        assert!(session.graph().edges.contains(&kar::observe::SessionEdge {
            parent: "session-1".into(),
            child: "child-1".into()
        }));
        assert_eq!(session.model().as_deref(), Some("demo"));
        assert_eq!(session.effort().as_deref(), Some("medium"));
        assert_eq!(session.service_tier().as_deref(), Some("default"));
        session.steer("continue").await?;
    }
    let (input, cancelled) = session.queue("withdrawn").await?;
    session.withdraw(input).await?;
    assert_eq!(cancelled.wait().await?, Outcome::Aborted);
    let (_, queued) = session.queue("refuse").await?;
    let stall = tokio::time::timeout(Duration::from_secs(3), stalls.next())
        .await
        .unwrap()?
        .unwrap();
    assert!(stall.silent_for_ms >= 50);
    assert_eq!(stall.last_record.stream_id, session.stream_id());
    assert!(
        tokio::time::timeout(Duration::from_millis(80), stalls.next())
            .await
            .is_err()
    );
    let (extra, removed) = session.queue("withdraw-before-dispatch").await?;
    session.withdraw(extra).await?;
    assert_eq!(removed.wait().await?, Outcome::Aborted);

    let next = tokio::time::timeout(Duration::from_secs(3), stalls.next())
        .await
        .unwrap()?
        .unwrap();
    assert_eq!(next.since_seq, stall.since_seq);
    assert!(next.last_record.after_seq > stall.last_record.after_seq);
    session.abort().await?;
    assert_eq!(turn.wait().await?, Outcome::Aborted);
    assert!(queued.wait().await.is_err());
    assert!(!session.await_idle().await?.status.is_running());
    assert!(session.prompt("refuse").await?.wait().await.is_err());
    session.close().await
}
async fn failures() -> kar::Result<()> {
    for message in ["exit", "malformed"] {
        let (_d, r, opts) = rig(Protocol::Acp, ApprovalMode::Deny);
        let session = r.open(opts).await?;
        assert!(session.prompt(message).await?.wait().await.is_err());
        let records = session.records();
        assert!(records.iter().all(|r| r.native["type"] != "dispose"));
        let exit = &records.last().unwrap().native;
        assert_eq!(exit["type"], "exited");
        assert!(exit.get("requestId").is_none());
        if message == "exit" {
            assert_eq!(exit["code"], 0);
        }
        assert!(
            session
                .records()
                .last()
                .unwrap()
                .events
                .iter()
                .any(|e| matches!(e, Event::Exited { .. }))
        );
    }
    Ok(())
}
async fn pi_control_reply_identity() -> kar::Result<()> {
    let (_dir, rt, opts) = rig(Protocol::Pi, ApprovalMode::Host);
    let session = rt.open(opts).await?;
    let _turn = session.prompt("hold steer").await?;
    for (text, status) in [
        ("refuse", kar::SteerStatus::Refused),
        ("missing success", kar::SteerStatus::Unconfirmed),
    ] {
        let from = session.records().len();
        let id = uuid::Uuid::new_v4();
        let result = session.steer_with_id(id, text).await;
        if status == kar::SteerStatus::Refused {
            assert!(
                matches!(result, Err(Error::Refused(ref reason)) if reason == "extension refused steer")
            );
        } else {
            assert!(result.is_err());
        }
        let rows = session.records();
        assert_eq!(
            rows[from..]
                .iter()
                .filter(|r| r.native["data"]["disposition"] == "handled"
                    && r.native["success"] == true
                    && r.response_to.is_none()
                    && r.events.is_empty())
                .count(),
            3
        );
        let receipts: Vec<_> = rows
            .iter()
            .flat_map(|r| &r.events)
            .filter_map(|e| match e {
                Event::SteeringInput {
                    input_id, status, ..
                } if *input_id == id => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(receipts, [kar::SteerStatus::Submitted, status]);
        let request = rows
            .iter()
            .find(|r| r.native["inputId"] == id.to_string())
            .unwrap();
        assert!(rows.iter().any(
            |r| r.native["requestId"] == request.native["id"] && r.native["type"] == "rejected"
        ));
        if status == kar::SteerStatus::Refused {
            let from = rows.len();
            assert!(
                matches!(session.abort().await, Err(Error::Refused(reason)) if reason.contains("native abort refused"))
            );
            let rows = session.records();
            let rows = &rows[from..];
            let request = rows
                .iter()
                .find(|r| {
                    r.native["type"] == "abort"
                        && r.native["id"]
                            .as_str()
                            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
                })
                .unwrap();
            let decisions: Vec<_> = rows
                .iter()
                .filter(|r| r.native["requestId"] == request.native["id"])
                .collect();
            assert_eq!(decisions.len(), 1);
            assert_eq!(decisions[0].native["type"], "rejected");
            assert_eq!(decisions[0].native["code"], "runtime_refused");
            assert!(session.status().is_running());
        }
    }
    Ok(())
}
async fn pi_question_timeout() -> kar::Result<()> {
    let (_d, rt, opts) = rig(Protocol::Pi, ApprovalMode::Host);
    let session = rt.open(opts).await?;
    let mut stream = session.subscribe(None)?;
    let turn = session.prompt("two dialogs").await?;
    let mut expired = None;
    while let Some(record) = stream.next().await {
        if record.native["id"] == "expired" && record.native["type"] == "extension_ui_request" {
            let mut view = kar::observe::SessionReducer::new(session.id());
            for r in session.records().iter().take_while(|r| r.seq <= record.seq) {
                view.apply(r)?;
            }
            let req = view.snapshot().pending_requests[0].clone();
            assert_eq!(req.question_form().unwrap().fields[0].options.len(), 2);
            expired = Some(req);
        }
        if record.native["type"] != "extension_ui_timeout" {
            continue;
        }
        assert!(
            session
                .answer_question(&expired.unwrap(), kar::QuestionReply::Cancel)
                .await
                .is_err()
        );
        let req = session.snapshot().pending_requests.pop().unwrap();
        assert_eq!(req.id, "live");
        session
            .answer_question(
                &req,
                kar::QuestionReply::Submit {
                    values: [("value".into(), false.into())].into_iter().collect(),
                },
            )
            .await?;
        break;
    }
    assert_eq!(turn.wait().await?, Outcome::Completed);
    session.close().await
}
async fn request_reuse_and_fork_boundaries() -> kar::Result<()> {
    let (_d, rt, opts) = rig(Protocol::Codex, ApprovalMode::Host);
    let session = rt.open(opts.clone()).await?;
    let mut records = session.subscribe(None)?;
    let turn = session.prompt("reused-question").await?;
    let mut replay = kar::observe::SessionReducer::new(session.id());
    let mut prior = None;
    while let Some(r) = records.next().await {
        replay.apply(&r)?;
        if r.events
            .iter()
            .any(|e| matches!(e, Event::RequestResolved { .. }))
        {
            assert!(replay.snapshot().pending_requests.is_empty());
        }
        if !r
            .events
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested { .. }))
        {
            continue;
        }
        let req = replay.snapshot().pending_requests[0].clone();
        if let Some(old) = prior {
            assert_eq!(req.kind, kar::RequestKind::Question);
            assert!(req.decisions().is_empty());
            assert_eq!(req.body["questions"][0]["question"], "Second");
            assert!(session.answer_request(&old, json!({})).await.is_err());
            assert!(
                session
                    .decide(&req, kar::PermissionDecision::AllowOnce)
                    .await
                    .is_err()
            );
            assert_eq!(req.question_form().unwrap().fields[0].label, "Second");
            let submit = |k: &str| kar::QuestionReply::Submit {
                values: [(k.into(), json!("answer"))].into_iter().collect(),
            };
            assert!(session.answer_question(&old, submit("q")).await.is_err());
            assert!(session.answer_question(&req, submit("bad")).await.is_err());
            session.answer_question(&req, submit("q")).await?;
            break;
        }
        prior = Some(req);
    }
    assert_eq!(turn.wait().await?, Outcome::Completed);
    assert!(session.snapshot().pending_requests.is_empty());
    assert!(session.records().iter().any(|r| {
        r.native
            .pointer("/result/answers/q/answers/0")
            .is_some_and(|v| v == "[redacted]")
    }));
    let second = session.prompt("fine-grants").await?;
    while let Some(record) = records.next().await {
        if !record
            .events
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested { .. }))
        {
            continue;
        }
        let mut req = session.snapshot().pending_requests.pop().unwrap();
        if req.id == 93 {
            assert_eq!(req.permission_resources().len(), 3);
            req.body["permissions"]["fileSystem"]["read"] = json!(["untrusted UI path"]);
            assert!(
                session
                    .grant_resources(
                        &req,
                        kar::PermissionDecision::AllowForSession,
                        vec![kar::ResourceId::Read(4)]
                    )
                    .await
                    .is_err()
            );
            session
                .grant_resources(
                    &req,
                    kar::PermissionDecision::AllowForTurn,
                    vec![kar::ResourceId::Read(1)],
                )
                .await?;
        } else if req.id == 94 {
            let choice = req.policy_options().first().unwrap().choice;
            req.body["proposedExecpolicyAmendment"] = json!(["wrong"]);
            session.select_policy(&req, choice).await?;
            break;
        }
    }
    assert_eq!(second.wait().await?, Outcome::Completed);
    session.close().await?;
    let copy = rt.fork("source", Some("anchor"), opts.clone()).await?;
    assert_eq!(copy.id(), "copy");
    copy.close().await?;
    assert!(rt.fork("same", None, opts.clone()).await.is_err());
    assert!(rt.fork("wrong", Some("anchor"), opts).await.is_err());
    let (_d, acp, opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    assert!(matches!(
        acp.fork("source", None, opts.clone()).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        acp.fork("source", Some("anchor"), opts).await,
        Err(Error::Unsupported(_))
    ));
    Ok(())
}
async fn open_instruction_policies() -> kar::Result<()> {
    let (_d, codex, mut opts) = rig(Protocol::Codex, ApprovalMode::Deny);
    opts.env
        .insert("TEST_API_KEY".into(), "native-private-key".into());
    opts.settings.system_prompt = Some("replace-test".into());
    opts.settings.append_system_prompt = Some("append-test".into());
    opts.settings.disallowed_tools = [
        "mcp__primary__old",
        "mcp__primary__more",
        "mcp__session__another",
    ]
    .map(str::to_owned)
    .to_vec();
    opts.mcp_servers.insert(
        "session".into(),
        kar::McpServer::Http {
            url: "http://127.0.0.1:1".into(),
            headers: [("X-Tiny".into(), "abc".into())].into(),
        },
    );
    for resume in [None, Some("session-1".to_owned())] {
        opts.resume = resume;
        let session = codex.open(opts.clone()).await?;
        let native: Vec<_> = session.records().iter().map(|r| r.native.clone()).collect();
        let raw = serde_json::to_string(&native)?;
        assert!(
            !raw.contains("native-private-key")
                && !raw.contains("private-server-path")
                && !raw.contains("append-test")
                && !raw.contains("replace-test")
        );
        let args = native
            .iter()
            .find(|v| matches!(v["method"].as_str(), Some("thread/start" | "thread/resume")))
            .unwrap();
        assert_eq!(
            args.pointer("/params/config/mcp_servers/session/http_headers/X-Tiny"),
            Some(&json!("[redacted]"))
        );
        session.close().await?;
    }
    for deny in [
        "mcp__unknown__tool",
        "mcp__amb__iguous__tool",
        "mcp__primary__",
        "shell",
    ] {
        opts.settings.disallowed_tools = vec![deny.into()];
        assert!(codex.open(opts.clone()).await.is_err());
    }
    let (_d, mut acp, mut opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    opts.settings.append_system_prompt = Some("custom".into());
    assert!(matches!(
        acp.open(opts.clone()).await,
        Err(Error::Unsupported(_))
    ));
    acp.id = "antigravity".into();
    assert!(matches!(
        acp.open(opts.clone()).await,
        Err(Error::Unsupported(_))
    ));
    let mut advertised = opts.clone();
    advertised
        .env
        .insert("KAR_TEST_APPEND_CAPABILITY".into(), "1".into());
    for resume in [None, Some("session-1".into())] {
        advertised.resume = resume;
        acp.open(advertised.clone()).await?.close().await?;
    }
    acp.id = "codebuddy".into();
    let mut buddy = opts.clone();
    buddy.settings.append_system_prompt.take();
    buddy.settings.system_prompt = Some("buddy-rules".into());
    buddy.settings.disallowed_tools = vec!["Read".into(), "Write".into()];
    buddy.env.insert("KAR_TEST_BUDDY_FLAGS".into(), "1".into());
    let session = acp.open(buddy).await?;
    let path: std::path::PathBuf = session
        .records()
        .iter()
        .find_map(|r| {
            r.native
                .pointer("/result/promptFile")
                .and_then(Value::as_str)
        })
        .unwrap()
        .into();
    assert!(path.is_file());
    session.close().await?;
    assert!(!path.exists());
    acp.id = "grok".into();
    opts.resume = Some("session-1".into());
    assert!(matches!(
        acp.open(opts.clone()).await,
        Err(Error::Unsupported(_))
    ));
    opts.settings.system_prompt = Some("grok-base".into());
    let session = acp.open(opts).await?;
    let init = session
        .records()
        .iter()
        .find(|r| r.native["method"] == "initialize")
        .unwrap()
        .native
        .clone();
    assert_eq!(
        init["params"]["_meta"],
        json!({"clientIdentifier":"kar","clientType":"generic",
            "startupHints":{"nonInteractive":true,"skipGitStatus":true,"skipProjectLayout":true},
            "systemPromptOverride":"grok-base","rules":"custom"})
    );
    session.close().await
}
async fn structured_output() -> kar::Result<()> {
    let (_d, mut r, mut opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    opts.settings.output_schema = Some(serde_json::from_value(json!({"type":"object"}))?);
    assert!(matches!(
        r.open(opts.clone()).await,
        Err(Error::Unsupported(_))
    ));
    r.id = "grok".into();
    let session = r.open(opts).await?;
    assert!(session.capabilities().output_schema);
    let report = session.prompt("structured-null").await?.report().await?;
    let persisted: kar::TurnReport = serde_json::from_value(serde_json::to_value(report)?)?;
    assert_eq!(persisted.structured_output, Some(Value::Null));
    let context = session.current_context().unwrap();
    assert_eq!(
        (context.tokens, context.context_window, context.percent),
        (Some(17), Some(128), Some(13.0))
    );
    assert_eq!(session.usage().total.unwrap().input, 1024);
    assert_eq!(session.snapshot().last_structured_output, Some(Value::Null));
    let (_, turn) = session.queue("structured-missing").await?;
    let absent = serde_json::to_value(turn.report().await?)?;
    assert!(absent.get("structured_output").is_none());
    let failed = session.prompt("structured-error").await?.report().await?;
    assert_eq!(failed.outcome, Outcome::Failed);
    assert_eq!(
        failed.failure.unwrap().class,
        kar::FailureClass::OutputSchema
    );
    assert!(failed.structured_output.is_none());
    session.close().await
}
async fn manual_compaction_controls() -> kar::Result<()> {
    let (_dir, mut r, opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    r.id = "grok".into();
    let session = r.open(opts).await?;
    use kar::{CompactionFailure, CompactionStatus as Status, CompactionUnconfirmed};
    for i in 0..5 {
        let r = session
            .start_compaction()
            .await?
            .wait(Some(Duration::from_secs(3)))
            .await;
        match i {
            0 => assert!(matches!(r.status, Status::Completed { .. })),
            1 => assert!(matches!(
                r.status,
                Status::Unconfirmed {
                    code: CompactionUnconfirmed::ObservationFailed
                }
            )),
            2 => assert!(matches!(r.status, Status::Aborted { .. })),
            3 => assert!(matches!(
                r.status,
                Status::Failed {
                    code: CompactionFailure::NativeFailed,
                    ..
                }
            )),
            _ => assert!(matches!(r.status, Status::Rejected { .. })),
        }
    }

    session.close().await?;
    for mode in ["shadow", "changed", "failed", "missing"] {
        let (_dir, r, mut opts) = rig(Protocol::Claude, ApprovalMode::Deny);
        opts.env
            .insert("KAR_TEST_COMPACT_CATALOG".into(), mode.into());
        let claude = r.open(opts).await?;
        let before = claude.snapshot().cursor.unwrap().after_seq;
        let op = claude.start_compaction().await;
        if mode == "shadow" {
            assert!(matches!(op, Err(Error::Unsupported(_))));
            assert!(claude.snapshot().manual_compaction.is_none());
        } else {
            let result = op?.wait(Some(Duration::from_secs(3))).await;
            let expected = match mode {
                "changed" => CompactionFailure::CommandChanged,
                "failed" => CompactionFailure::NativeFailed,
                _ => CompactionFailure::NotCompacted,
            };
            assert!(matches!(result.status,Status::Failed{code,..} if code == expected));
        }
        assert!(
            claude
                .records()
                .iter()
                .filter(|r| r.seq > before)
                .all(|r| !r.native.to_string().contains("catalog-private-fixture"))
        );
        claude.close().await?;
    }
    Ok(())
}
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    if std::env::var_os("KAR_INVENTORY_BATCH").is_some() {
        let id = std::path::Path::new(&args[0])
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap();
        if id == "claude"
            && args[1..] == ["--version"]
            && std::env::var_os("KAR_STALL_INVENTORY").is_some()
        {
            std::thread::sleep(Duration::from_secs(10));
        }
        return account::peer(id, &args[1..]);
    }
    if let Some(dir) = std::env::var_os("KAR_CREW_TRACE") {
        return crew_options::peer(&args, std::path::Path::new(&dir));
    }
    if args.get(1).is_some_and(|a| a == "peer")
        && let Some(path) = std::env::var_os("KAR_TEST_SESSION_ARGV")
    {
        std::fs::write(path, serde_json::to_vec(&args[1..]).unwrap()).unwrap();
    }
    match args.get(1).map(String::as_str) {
        Some("antigravity-picker") => return antigravity_model_list::run().await.unwrap(),
        Some("acp-picker") => return acp_model_list::run().await.unwrap(),
        Some("codex-catalog") => return codex_catalog::run().await.unwrap(),
        Some("opencode-model-listing") => return opencode_models::run().await.unwrap(),
        Some("launch-args") => return launch_args::run().await.unwrap(),
        Some("claude-mcp-native") => return mcp_handoff::claude().await,
        Some("qoder-mcp-native") => return mcp_handoff::qoder().await,
        Some("stdio-echo-once") => return mcp_handoff::stdio(true),
        Some("stdio-echo") => return mcp_handoff::stdio(false),
        Some("environment-leaf") => return environment::leaf(),
        Some("environment-sessions") => return environment::sessions().await,
        Some("peer") => environment::capture(),
        _ => {}
    }
    if let Ok(mode) = std::env::var("KAR_TEST_AGY_PICKER") {
        return antigravity_model_list::peer(&mode, &args[1..]);
    }
    if let Ok(vendor) = std::env::var("KAR_TEST_MENU") {
        return acp_model_list::peer(&vendor, &args[1..]);
    }
    if let Ok(mode) = std::env::var("KAR_TEST_CODEX_CATALOG") {
        return codex_catalog::peer(&mode);
    }
    if std::env::var_os("KAR_TEST_CLAUDE_INPUT").is_some() {
        return claude_input::peer(&args);
    }
    if let Ok(mode) = std::env::var("KAR_TEST_PI_REGISTRY") {
        return pi_registry::peer(&mode, &args);
    }
    if let Ok(mode) = std::env::var("KAR_TEST_DSH_SKILLS") {
        return dsh_skills::peer(&mode, &args);
    }
    if std::env::var_os("KAR_TEST_GROK_TURNS").is_some() {
        return grok_turns::peer();
    }
    if let Ok(version) = std::env::var("KAR_TEST_AGY_STEER") {
        return antigravity_steering::peer(&version);
    }
    if let Ok(case) = std::env::var("KAR_TEST_OPENCODE_MODELS") {
        return opencode_models::peer(&case, &args[1..]);
    }
    if let Ok(mode) = std::env::var("KAR_TEST_OPENCODE_STEERING") {
        return opencode_steering::peer(&mode, &args);
    }
    if let Ok(script) = std::env::var("KAR_TEST_OPEN_SCRIPT") {
        return opening::peer(&script);
    }
    if std::env::var_os("KAR_QWEN_PROVIDER_TEST").is_some() {
        return qwen_providers::peer(&args[1..]);
    }
    if std::env::var_os("KAR_PROVIDER_TEST").is_some() {
        return provider::peer(&args[1..]).await;
    }
    if let Ok(id) = std::env::var("KAR_ACCOUNT_TEST") {
        return account::peer(&id, &args[1..]);
    }
    if std::env::var_os("KAR_TEST_INSTALLATION").is_some() {
        return installation::peer(&args);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match args.get(1).map(String::as_str) {
        Some("resource-child") => {
            std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink()).unwrap();
            return;
        }
        Some("resource-peer") => return peer_with_child(),
        None => process_resources().await.unwrap(),
        _ => {}
    }
    if let Ok(version) = std::env::var("KAR_FIXTURE_QODER_VERSION") {
        if args.get(1).is_some_and(|a| a == "--version") {
            println!("qodercli {version}");
        } else {
            qoder::peer(&version, &args[1..]);
        }
        return;
    }
    if std::env::var_os("KAR_TEST_MCP_ROUTING").is_some()
        && args.iter().any(|s| s == "--input-format")
    {
        peer("claude");
        return;
    }
    if args.get(1).is_some_and(|s| s == "app-server") {
        peer("codex");
        return;
    }
    if args.get(1).is_some_and(|s| s == "--version") {
        println!("fixture 1.0.0");
        return;
    }
    if args.get(1).is_some_and(|s| s == "peer") {
        peer(&args[2]);
        return;
    }
    antigravity_model_list::run().await.unwrap();
    acp_model_list::run().await.unwrap();
    codex_catalog::run().await.unwrap();
    environment::exercise(&std::env::current_exe().unwrap());
    launch_args::run().await.unwrap();
    account::exercise().await;
    provider::exercise().await;
    qwen_http_mcp_version().await;
    qwen_providers::exercise().await;
    #[cfg(unix)]
    installation::exercise().await;
    tokio::time::timeout(Duration::from_secs(25), qoder::sessions())
        .await
        .expect("Qoder protocol timed out")
        .unwrap();
    println!(
        "Qoder worker and daemon routing, resume, usage, private controls and compaction passed"
    );
    for p in [
        Protocol::Codex,
        Protocol::Claude,
        Protocol::Qwen,
        Protocol::Acp,
        Protocol::Pi,
    ] {
        tokio::time::timeout(Duration::from_secs(12), exercise(p))
            .await
            .expect("contract timed out")
            .unwrap();
        println!("{p:?} conversation, native model catalog, replay and resume passed");
    }
    tokio::time::timeout(Duration::from_secs(12), child_models::exercise())
        .await
        .unwrap()
        .unwrap();
    println!("Child model and effort attribution, task snapshots and recording replay passed");
    for p in [Protocol::Codex, Protocol::Acp] {
        tokio::time::timeout(Duration::from_secs(12), controls(p))
            .await
            .unwrap()
            .unwrap();
        println!("{p:?} approval, busy, abort and refusal passed");
    }
    tokio::time::timeout(Duration::from_secs(25), crew::exercise())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), cli_contract())
        .await
        .unwrap()
        .unwrap();
    println!("CLI recording, cursor replay and MCP delegation passed");
    orientation::exercise();
    inventory_batch::exercise();
    crew_options::exercise();
    open_instruction_policies().await.unwrap();
    structured_output().await.unwrap();
    manual_compaction_controls().await.unwrap();
    failures().await.unwrap();
    acp_failures::run().await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), claude_input::run())
        .await
        .unwrap()
        .unwrap();
    dsh_skills::run().await.unwrap();
    pi_registry::run().await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), grok_turns::run())
        .await
        .expect("Grok native turns timed out")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), antigravity_steering::run())
        .await
        .unwrap()
        .unwrap();
    opencode_models::run().await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), opencode_steering::run())
        .await
        .unwrap()
        .unwrap();
    opening::run().await.unwrap();
    tokio::time::timeout(Duration::from_secs(6), pi_control_reply_identity())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(6), pi_question_timeout())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), request_reuse_and_fork_boundaries())
        .await
        .unwrap()
        .unwrap();
    println!("process exit and malformed frame passed");
}

async fn cli_contract() -> kar::Result<()> {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        process::Command,
    };
    let dir = TempDir::new()?;
    let record = dir.path().join("run.jsonl");
    let exe = std::env::current_exe()?;
    let bin = env!("CARGO_BIN_EXE_kar");
    let fork = Command::new(bin)
        .args([
            "open",
            "codex",
            "--fork",
            "source",
            "--through",
            "anchor",
            "--bin",
        ])
        .arg(&exe)
        .arg("--cwd")
        .arg(dir.path())
        .output()
        .await?;
    assert!(
        fork.status.success(),
        "{}",
        String::from_utf8_lossy(&fork.stderr)
    );
    assert_eq!(serde_json::from_slice::<Value>(&fork.stdout)?["id"], "copy");
    let conflict = Command::new(bin)
        .args(["open", "codex", "--fork", "source", "--resume", "source"])
        .output()
        .await?;
    assert!(!conflict.status.success());
    let run = Command::new(bin)
        .args(["run", "codex", "normal", "--record"])
        .arg(&record)
        .arg("--bin")
        .arg(&exe)
        .arg("--cwd")
        .arg(dir.path())
        .arg("--json")
        .output()
        .await?;
    assert!(
        run.status.success(),
        "{:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, std::fs::read(&record)?);
    let records: Vec<kar::Record> = std::fs::read_to_string(&record)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let cursor = records[3].cursor();
    let replay = Command::new(bin)
        .arg("replay")
        .arg(&record)
        .arg("--after")
        .arg(cursor.after_seq.to_string())
        .arg("--stream")
        .arg(cursor.stream_id.to_string())
        .output()
        .await?;
    assert!(replay.status.success());
    assert_eq!(
        String::from_utf8_lossy(&replay.stdout).lines().count(),
        records.len() - 4
    );
    let log_dir = dir.path().join("children");
    let child = Command::new(bin)
        .args([
            "mcp",
            "--runtimes",
            "codex,claude",
            "--max-running",
            "1",
            "--max-depth",
            "2",
            "--max-sessions",
            "2",
            "--cwd",
        ])
        .arg(dir.path())
        .arg("--log-dir")
        .arg(&log_dir)
        .env_remove("KAR_AGENT_DEPTH")
        .env_remove("KAR_AGENT_MAX_DEPTH")
        .env("KAR_CODEX_BIN", &exe)
        .env("KAR_CLAUDE_BIN", &exe)
        .env("KAR_TEST_MCP_ROUTING", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut child = child;
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let calls = [
        (
            "initialize",
            json!({"protocolVersion":"2025-11-25","clientInfo":{"name":"test","version":"1"},"capabilities":{}}),
        ),
        ("tools/list", json!({})),
        ("tools/call", json!({"name":"runtimes","arguments":{}})),
        (
            "tools/call",
            json!({"name":"run","arguments":{"runtime":"codex","task":"route/codex"}}),
        ),
        (
            "tools/call",
            json!({"name":"run","arguments":{"runtime":"claude","task":"route/claude"}}),
        ),
        (
            "tools/call",
            json!({"name":"run","arguments":{"runtime":"other","task":"must not run"}}),
        ),
        ("tools/call", json!({"name":"list","arguments":{}})),
    ];
    for (i, (method, params)) in calls.iter().enumerate() {
        let v = json!({"jsonrpc":"2.0","id":i,"method":method,"params":params});
        input.write_all(format!("{v}\n").as_bytes()).await?;
        let reply: Value = serde_json::from_str(&lines.next_line().await?.unwrap())?;
        assert_eq!(reply["id"], i);
        assert!(reply.get("error").is_none());
        let r = &reply["result"];
        match i {
            0 => assert_eq!(r["protocolVersion"], "2025-11-25"),
            1 => assert_eq!(r["tools"].as_array().unwrap().len(), 8),
            2 => assert_eq!(
                r["structuredContent"]["runtimes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["claude", "codex"]
            ),
            3 | 4 => assert_eq!(r["structuredContent"]["text"], "hello", "{reply}"),
            5 => assert_eq!(r["isError"], true, "{reply}"),
            6 => assert_eq!(
                r["structuredContent"]["agents"].as_array().unwrap().len(),
                2
            ),
            _ => unreachable!(),
        }
    }
    for runtime in ["codex", "claude"] {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(format!("received-{runtime}")))?,
            format!("route/{runtime}")
        );
    }
    drop(input);
    assert!(child.wait().await?.success());
    let logs = std::fs::read_dir(log_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(logs.len(), 2);
    for entry in logs {
        let archive = entry.path();
        let export = dir
            .path()
            .join(archive.file_name().unwrap())
            .with_extension("replay");
        let result = Command::new(bin)
            .arg("import")
            .arg(&archive)
            .arg(&export)
            .output()
            .await?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let replay = Command::new(bin)
            .arg("replay")
            .arg(&export)
            .output()
            .await?;
        assert!(replay.status.success());
        let original = std::fs::read_to_string(&archive)?;
        let frames = original
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(frames[0]["format"], "kar-voyage/3");
        assert_eq!(frames.last().unwrap()["kind"], "end");
        let output = String::from_utf8(replay.stdout).unwrap();
        let restored = output
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let expected: Vec<_> = frames
            .iter()
            .filter(|f| f["kind"] == "record")
            .map(|f| f["record"].clone())
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(restored, expected);
    }
    Ok(())
}
