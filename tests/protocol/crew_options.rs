use super::emit;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Lines, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Client {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    sequence: u64,
}
impl Client {
    fn open(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut client = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()).lines(),
            sequence: 0,
            child,
        };
        assert_eq!(
            client.call("initialize", json!({"protocolVersion":"2025-11-25"}))["protocolVersion"],
            "2025-11-25"
        );
        client
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.sequence += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.sequence,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        let response: Value = serde_json::from_str(&self.output.next().unwrap().unwrap()).unwrap();
        assert_eq!(response["id"], self.sequence);
        response["result"].clone()
    }
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.call("tools/call", json!({"name":name,"arguments":arguments}))
    }
    fn close(mut self) {
        drop(self.input);
        assert!(self.child.wait().unwrap().success());
    }
}

fn mcp(cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.args(["mcp", "--cwd"]).arg(cwd);
    cmd
}

pub(super) fn peer(args: &[String], trace: &Path) {
    if args.iter().any(|a| a == "--version") {
        println!("codex-cli 0.131.0");
        return;
    }
    if args.iter().any(|a| a == "--help") {
        println!("Usage: codex app-server [OPTIONS]");
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let depth = std::env::var("KAR_AGENT_DEPTH").unwrap();
    let mut settings = Value::Null;
    for line in std::io::stdin().lock().lines() {
        let packet: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let reply = |value: Value| emit(json!({"jsonrpc":"2.0","id":packet["id"],"result":value}));
        match packet["method"].as_str().unwrap() {
            "initialize" => reply(json!({})),
            "initialized" => {}
            "thread/start" => {
                settings = packet["params"].clone();
                let model = if settings["model"] == "denied" {
                    json!("different")
                } else {
                    settings["model"].clone()
                };
                reply(
                    json!({"thread":{"id":"child"},"model":model,"reasoningEffort":settings["config"]["model_reasoning_effort"],"serviceTier":settings["serviceTier"]}),
                );
            }
            "turn/start" => {
                let task = packet["params"]["input"][0]["text"].as_str().unwrap();
                let mut row = json!({"cwd":cwd,"settings":settings,"depth":depth,"max":std::env::var("KAR_AGENT_MAX_DEPTH").unwrap()});
                reply(json!({"turn":{"id":"t"}}));
                emit(
                    json!({"method":"turn/started","params":{"threadId":"child","turn":{"id":"t"}}}),
                );
                if task == "nested" {
                    let mut cmd = mcp(&cwd);
                    let mut nested = Client::open(cmd.args(["--runtimes", "codex"]));
                    row["next"]=nested.tool("run",json!({"runtime":"codex","task":task}))["structuredContent"].clone();
                    nested.close();
                }
                fs::write(trace.join(format!("{task}-{depth}.json")), row.to_string()).unwrap();
                emit(
                    json!({"method":"turn/completed","params":{"threadId":"child","turn":{"id":"t","status":"completed"}}}),
                );
            }
            other => panic!("unexpected Codex method {other}"),
        }
    }
}

fn open(cwd: &Path, trace: &Path, runtimes: Option<&str>, limit: Option<usize>) -> Client {
    let mut cmd = mcp(cwd);
    cmd.env_remove("KAR_AGENT_DEPTH")
        .env_remove("KAR_AGENT_MAX_DEPTH")
        .env("HOME", cwd)
        .env("KAR_CREW_TRACE", trace);
    for runtime in kar::runtimes()
        .into_iter()
        .chain([kar::runtime("morph").unwrap()])
    {
        let key = format!("KAR_{}_BIN", runtime.id.to_ascii_uppercase());
        cmd.env(
            key,
            if runtime.id == "codex" {
                std::env::current_exe().unwrap()
            } else {
                cwd.join("absent-program")
            },
        );
    }
    if let Some(list) = runtimes {
        cmd.args(["--runtimes", list]);
    }
    if let Some(depth) = limit {
        cmd.arg("--max-depth").arg(depth.to_string());
    }
    Client::open(&mut cmd)
}
fn trace(path: &Path, task: &str, depth: usize) -> Value {
    serde_json::from_slice(&fs::read(path.join(format!("{task}-{depth}.json"))).unwrap()).unwrap()
}

pub(super) fn exercise() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().canonicalize().unwrap();
    let logs = root.join("packets");
    fs::create_dir(&logs).unwrap();
    let mut client = open(
        &root,
        &logs,
        Some(" codex,missing,codex,morph,cybion "),
        None,
    );
    let offered = client.tool("runtimes", json!({}))["structuredContent"]["runtimes"].clone();
    let offered = offered.as_array().unwrap();
    assert_eq!(
        offered
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["codex", "cybion", "morph"]
    );
    for row in offered.iter().filter(|r| r["id"] != "codex") {
        assert_eq!(row["spawnable"], false);
        assert_eq!(
            row["reason"],
            kar::runtime(row["id"].as_str().unwrap())
                .unwrap()
                .protocol
                .child_environment_refusal()
                .unwrap()
        );
        assert_eq!(
            client.tool("spawn", json!({"runtime":row["id"],"task":"forbidden"}))["structuredContent"]
                ["code"],
            "open_failed"
        );
    }
    assert_eq!(
        client.tool("run", json!({"runtime":"pi","task":"unoffered"}))["structuredContent"]["code"],
        "unknown_runtime"
    );
    let tools = client.call("tools/list", json!({}));
    for tool in tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| matches!(t["name"].as_str(), Some("run" | "spawn")))
    {
        for key in ["cwd", "model", "effort", "serviceTier"] {
            assert_eq!(tool["inputSchema"]["properties"][key]["type"], "string");
        }
    }
    let mut ids = vec![];
    for (name, task, model, effort, tier) in [
        ("spawn", "first", "one", "low", "fast"),
        ("run", "second", "two", "high", "default"),
    ] {
        let cwd = root.join(task);
        fs::create_dir(&cwd).unwrap();
        let out=client.tool(name,json!({"runtime":"codex","task":task,"cwd":cwd,"model":model,"effort":effort,"serviceTier":tier}));
        assert_ne!(out["isError"], true, "{out}");
        ids.push(out["structuredContent"]["id"].clone());
        if name == "spawn" {
            let wait = client.tool(
                "wait",
                json!({"ids":[ids.last().unwrap()],"timeoutMs":3000}),
            );
            assert_eq!(wait["structuredContent"]["reports"][0]["turn"], 1);
        }
        let row = trace(&logs, task, 1);
        assert_eq!(row["cwd"], json!(cwd));
        assert_eq!(row["settings"]["cwd"], json!(cwd));
        assert_eq!(row["settings"]["model"], model);
        assert_eq!(row["settings"]["config"]["model_reasoning_effort"], effort);
        assert_eq!(row["settings"]["serviceTier"], tier);
    }
    assert_ne!(ids[0], ids[1]);
    assert_ne!(
        client.tool("run", json!({"runtime":"codex","task":"plain"}))["isError"],
        true
    );
    let row = trace(&logs, "plain", 1);
    assert_eq!(row["cwd"], json!(root));
    assert!(
        row["settings"].get("model").is_none()
            && row["settings"].get("serviceTier").is_none()
            && row["settings"]
                .pointer("/config/model_reasoning_effort")
                .is_none()
    );
    for key in ["cwd", "model", "effort", "serviceTier", "name", "resume"] {
        let mut args = json!({"runtime":"codex","task":"bad"});
        args[key] = 42.into();
        assert_eq!(
            client.tool("spawn", args)["content"][0]["text"],
            format!("{key} must be a string")
        );
    }
    for bad in [
        json!({"cwd":root.join("absent")}),
        json!({"model":"denied"}),
    ] {
        let mut args = bad;
        args["task"] = "denied".into();
        args["runtime"] = "codex".into();
        assert_eq!(
            client.tool("run", args)["structuredContent"]["code"],
            "open_failed"
        );
    }
    assert!(!logs.join("denied-1.json").exists());
    client.close();
    for limit in [1, 2] {
        let path = logs.join(format!("depth-{limit}"));
        fs::create_dir(&path).unwrap();
        let mut client = open(&root, &path, None, (limit != 1).then_some(limit));
        let offered = client.tool("runtimes", json!({}))["structuredContent"]["runtimes"].clone();
        let ids: std::collections::BTreeSet<_> = offered
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        let expected: std::collections::BTreeSet<_> = kar::runtimes()
            .into_iter()
            .map(|r| r.id)
            .chain(["morph".into()])
            .collect();
        assert_eq!(ids, expected.iter().map(String::as_str).collect());
        assert_eq!(
            client.tool("run", json!({"runtime":"pi","task":"missing"}))["structuredContent"]["code"],
            "not_installed"
        );
        assert_ne!(
            client.tool("run", json!({"runtime":"codex","task":"nested"}))["isError"],
            true
        );
        for level in 1..=limit {
            let child = trace(&path, "nested", level);
            assert_eq!(child["depth"], level.to_string());
            assert_eq!(child["max"], limit.to_string());
            if level == limit {
                assert_eq!(child["next"]["code"], "depth_limit");
            } else {
                assert!(child["next"]["id"].is_string());
            }
        }
        assert!(!path.join(format!("nested-{}.json", limit + 1)).exists());
        client.close();
    }
}
