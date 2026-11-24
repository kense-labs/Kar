#[path = "claude_inventory.rs"]
mod claude;
use kar::inventory::InventoryOptions;
use serde_json::{Value, json};
use std::io::{self, BufRead};

pub(super) fn peer(id: &str, case: &str, args: &[&str]) {
    if id == "claude" {
        return claude::peer(case, args);
    }
    if id == "antigravity" {
        assert_eq!(
            std::env::var("AGY_CLI_DISABLE_AUTO_UPDATE").unwrap(),
            "true"
        );
        if args == ["models"] {
            if case == "out" {
                eprintln!("Please sign in.");
                std::process::exit(1);
            }
            if case == "unknown_auth" {
                eprintln!("{}", super::SECRET);
                std::process::exit(5);
            }
            return println!("model");
        }
        assert_eq!(args, ["--output-format", "json", "--print=/skills"]);
        assert!(!matches!(case, "out" | "unknown_auth"));
        let row = json!({"name":"native skill","builtin":true,"model_invocable":false,"body":super::SECRET});
        let mut response = json!({"status":"SUCCESS","conversation_id":"","num_turns":0,"command":{"name":"skills","data":{"skills":[row]}}});
        match case {
            "turn" => response["num_turns"] = 1.into(),
            "flag" => response["command"]["data"]["skills"][0]["model_invocable"] = "no".into(),
            "conversation" => response["conversation_id"] = "created".into(),
            _ => (),
        }
        return println!("{response}");
    }
    if id == "pi" {
        assert_eq!(args, ["--mode", "rpc", "--no-session"]);
        let mut lines = io::stdin().lock().lines();
        let packet: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(packet["type"], "get_commands");
        println!(
            "{}",
            json!({"type":"extension_ui_request","method":"select","id":"synthetic"})
        );
        let cancel: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(
            cancel,
            json!({"type":"extension_ui_response","id":"synthetic","cancelled":true})
        );
        println!(
            "{}",
            json!({"type":"response","id":"other","command":"get_commands","success":true,"data":{"commands":[]}})
        );
        let mut result = json!({"type":"response","id":packet["id"],"command":"get_commands","success":true,"data":{"commands":[
            {"source":"prompt","name":"sample","description":"template"},
            {"source":"skill","name":"skill:sample","body":super::SECRET,"sourceInfo":{"path":"/fixture/sample","origin":"package","source":"example-package","scope":"temporary"}}
        ]}});
        match case {
            "malformed" => result["data"]["commands"][1]["sourceInfo"]["path"] = Value::Null,
            "wrong_command" => result["command"] = "get_state".into(),
            "rejected" => result["success"] = false.into(),
            _ => (),
        }
        return println!("{result}");
    }
    if id == "grok" {
        assert_eq!(args, ["inspect", "--json"]);
        println!(
            "{}",
            json!({"skills":[{"name":"alpha","description":"fixture","source":{"type":"plugin","path":"/fixture","plugin_name":"tools"},"userInvocable":false}],"mcpServers":[{"name":"example","source":"project","target":super::SECRET}]})
        );
        return;
    }
    if id == "kimi" {
        return super::billing::peer(case, args);
    }
    if id == "devin" {
        assert_eq!(args, ["skills", "list", "--json"]);
        let triggers = if case == "malformed" {
            json!([1])
        } else {
            json!(["user"])
        };
        println!(
            "{}",
            json!([{"name":"alpha","base_dir":"","provider":"Builtin","triggers":triggers,"errors":[super::SECRET],"body":super::SECRET}])
        );
        return;
    }
    for line in io::stdin().lock().lines() {
        let packet: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let method = packet["method"].as_str().unwrap();
        let mut reply = json!({"jsonrpc":"2.0","id":packet["id"]});
        let result = match (id, method) {
            ("codex" | "qwen", "initialize") => json!({"protocolVersion":1}),
            ("codex", "initialized") => continue,
            ("codex", "mcpServerStatus/list") => {
                let args = &packet["params"];
                assert_eq!(args["limit"], 100);
                assert_eq!(args["detail"], "full");
                match args.get("cursor").unwrap() {
                    Value::Null => {
                        json!({"data":[{"name":"first","runtimeStatus":"ready","authStatus":"OAuth","tools":{"lookup":{"name":"find","description":"search","inputSchema":{"type":"object"},"_meta":super::SECRET}},"env":super::SECRET}],"nextCursor":"two"})
                    }
                    s if s == "two" => match case {
                        "repeat" => json!({"data":[],"nextCursor":"two"}),
                        "invalid" => json!({"data":[],"nextCursor":42}),
                        "missing_cursor" => json!({"data":[]}),
                        _ => {
                            json!({"data":[{"name":"second","toolsError":super::SECRET,"tools":{}}],"nextCursor":null})
                        }
                    },
                    _ => panic!("invalid native cursor"),
                }
            }
            ("codex", "skills/list") => {
                assert_eq!(
                    std::path::Path::new(packet["params"]["cwds"][0].as_str().unwrap())
                        .canonicalize()
                        .unwrap(),
                    std::env::current_dir().unwrap().canonicalize().unwrap()
                );
                json!({"data":[{"errors":[super::SECRET],"skills":[{"name":"alpha","scope":"admin","pluginId":"example","enabled":false,"instructions":super::SECRET}]}]})
            }
            ("qwen", "qwen/status/workspace/skills") if case == "rejected" || case == "missing" => {
                let code = if case == "missing" { -32601 } else { -32000 };
                reply["error"] = json!({"code":code,"message":super::SECRET});
                println!("{reply}");
                return;
            }
            ("qwen", "qwen/status/workspace/skills") => {
                json!({"v":1,"initialized":true,"workspaceCwd":"workspace","skills":[{"name":"alpha","kind":"skill","status":"disabled","level":"extension","modelInvocable":false,"extensionName":"example"}],"errors":[{"message":super::SECRET}]})
            }
            ("factory", "droid.list_skills") => {
                assert_eq!(packet["params"], json!({"includeContent":false}));
                reply["type"] = "response".into();
                reply["factoryApiVersion"] = "1.0.0".into();
                json!({"skills":[{"name":"alpha","location":"personal","content":super::SECRET}],"projectAvailable":false})
            }
            _ => panic!("Unexpected skill discovery method {method}"),
        };
        reply["result"] = result;
        println!("{reply}");
        if method != "initialize" && method != "mcpServerStatus/list" {
            return;
        }
    }
}

async fn check(id: &str, case: &str) -> Value {
    let tmp = tempfile::tempdir().unwrap();
    let options = super::opts(tmp.path(), id, &format!("skills_{case}"));
    let mut runtime = kar::runtime(id).unwrap();
    runtime.program = std::env::current_exe().unwrap();
    let result = runtime
        .skills(&InventoryOptions {
            installation: options.installation,
            timeout: options.timeout,
        })
        .await
        .unwrap();
    let v = serde_json::to_value(result).unwrap();
    if id == "kimi" {
        assert!(tmp.path().join("native-stopped").exists(), "{v}");
        assert_eq!(
            tmp.path().join("removed-workspace").exists(),
            case != "existing"
        );
    }
    assert!(!v.to_string().contains(super::SECRET));
    assert!(!v.to_string().contains(super::super::billing_fixture::KEY));
    v
}

async fn mcp() {
    let tmp = tempfile::tempdir().unwrap();
    let mut runtime = kar::runtime("codex").unwrap();
    runtime.program = std::env::current_exe().unwrap();
    for case in ["partial", "repeat", "invalid", "missing_cursor"] {
        let fixture = super::opts(tmp.path(), "codex", &format!("skills_{case}"));
        let options = InventoryOptions {
            installation: fixture.installation,
            timeout: fixture.timeout,
        };
        let servers = serde_json::to_value(runtime.mcp_servers(&options).await.unwrap()).unwrap();
        let tools = serde_json::to_value(runtime.tools(&options).await.unwrap()).unwrap();
        for result in [&servers, &tools] {
            assert!(!result.to_string().contains(super::SECRET));
            if case == "partial" {
                assert_eq!(result["partial"], true);
            } else {
                assert_eq!(result["code"], "query_failed");
            }
        }
        if case == "partial" {
            assert_eq!(servers["items"][0]["toolCount"], 1);
            assert_eq!(servers["items"][1]["error"], "Native tool discovery failed");
            assert_eq!(
                tools["items"],
                json!([{"name":"find","description":"search","inputSchema":{"type":"object"},"mcpServerId":"first","source":"mcp"}])
            );
            assert_eq!(tools["view"], "mcp-only");
            assert_eq!(servers["view"], "discovered");
        }
    }
}
pub(super) async fn exercise() {
    let pi = check("pi", "complete").await;
    assert_eq!(
        pi["items"],
        json!([{"name":"sample","source":"example-package","path":"/fixture/sample","userInvocable":true,"plugin":"example-package","scope":"other"}])
    );
    for case in ["malformed", "wrong_command", "rejected"] {
        assert_eq!(check("pi", case).await["code"], "query_failed");
    }
    assert_eq!(
        check("antigravity", "out").await["code"],
        "not_authenticated"
    );
    for case in ["turn", "flag", "conversation", "unknown_auth"] {
        assert_eq!(check("antigravity", case).await["code"], "query_failed");
    }
    let agy = check("antigravity", "complete").await;
    assert_eq!(
        agy["items"],
        json!([{"name":"native skill","scope":"builtin","disableModelInvocation":true,"userInvocable":true}])
    );
    mcp().await;
    claude::exercise().await;
    for (id, scope) in [
        ("codex", "managed"),
        ("devin", "builtin"),
        ("factory", "user"),
        ("qwen", "other"),
        ("kimi", "project"),
    ] {
        let v = check(id, "partial").await;
        assert_eq!(v["kind"], "ok", "{id}: {v}");
        assert_eq!(v["partial"], id != "kimi");
        assert_eq!(v["items"][0]["scope"], scope);
        if id == "devin" {
            assert_eq!(v["items"][0]["userInvocable"], true);
            assert!(v["items"][0].get("path").is_none());
        }
        if matches!(id, "devin" | "kimi" | "qwen") {
            assert_eq!(v["items"][0]["disableModelInvocation"], true);
        }
    }
    assert_eq!(check("kimi", "existing").await["kind"], "ok");
    for (id, case, expected) in [
        ("kimi", "malformed", "query_failed"),
        ("devin", "malformed", "query_failed"),
        ("qwen", "rejected", "query_failed"),
        ("qwen", "missing", "native_query_unavailable"),
    ] {
        assert_eq!(check(id, case).await["code"], expected);
    }
}
