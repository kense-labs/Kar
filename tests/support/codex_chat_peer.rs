//! A minimal Codex app-server for normal-CLI chat integration scenarios.
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub(crate) fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}
pub(crate) fn agent() {
    agent_with_observer(|_| {});
}
pub(crate) fn agent_with_observer(observe: impl Fn(&Value)) {
    let cwd = std::env::current_dir().unwrap();
    let database = PathBuf::from(std::env::var_os("FIXTURE_NATIVE_DB").unwrap());
    let mut session = String::new();
    let mut late_sample: Option<Value> = None;
    let mut input = std::io::stdin().lock();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let v: Value = serde_json::from_str(&line).unwrap();
        observe(&v);
        let reply = |data: Value| emit(json!({"id":v["id"],"result":data}));
        if let Some(file) = std::env::var_os("FIXTURE_ENV_AUDIT")
            && matches!(
                v["method"].as_str(),
                Some("thread/start" | "thread/resume" | "thread/fork" | "turn/start")
            )
        {
            let mut output = OpenOptions::new()
                .append(true)
                .create(true)
                .open(file)
                .unwrap();
            writeln!(output,"{}",json!({"method":v["method"],"marker":std::env::var_os("KAR_SESSION_MARKER").is_some(),"control":std::env::var("KAR_CONTROL_MARKER").ok()})).unwrap();
        }
        match v["method"].as_str() {
            Some("initialize") => reply(json!({})),
            Some("initialized") => {}
            Some("model/list") => reply(
                json!({"data":[{"id":"demo","model":"demo","supportedReasoningEfforts":[{"reasoningEffort":"high"}]}]}),
            ),
            Some("config/read") => reply(json!({"config":{"model":"demo"}})),
            Some("thread/start") => {
                session = Uuid::new_v4().to_string();
                fs::write(database.join(&session), cwd.to_str().unwrap()).unwrap();
                reply(json!({"thread":{"id":session}}));
                if std::env::var_os("FIXTURE_IDLE_FORM").is_some() {
                    emit(
                        json!({"id":901,"method":"item/tool/requestUserInput","params":{"threadId":session,"questions":[{"id":"q","question":"Choose a color"}]}}),
                    );
                }
            }
            Some("thread/fork") => {
                if database.join("hold-native-fork").is_file() {
                    fs::write(database.join("entered-native-fork"), "").unwrap();
                    let limit = Instant::now() + Duration::from_secs(10);
                    while database.join("hold-native-fork").exists() {
                        assert!(Instant::now() < limit, "native fork gate timed out");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                if std::env::var_os("FIXTURE_FORK_REFUSED").is_some() {
                    emit(
                        json!({"id":v["id"],"error":{"code":-1,"message":"native anchor refused"}}),
                    );
                    continue;
                }
                let source = v["params"]["threadId"].as_str().unwrap();
                let parent_cwd = fs::read_to_string(database.join(source)).unwrap();
                assert_eq!(v["params"]["cwd"].as_str(), cwd.to_str());
                let anchor = v["params"]["lastTurnId"].as_str().unwrap();
                session = Uuid::new_v4().to_string();
                fs::write(database.join(&session), cwd.to_str().unwrap()).unwrap();
                writeln!(
                    OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(database.join("forks"))
                        .unwrap(),
                    "{session}"
                )
                .unwrap();
                let reported = if std::env::var_os("FIXTURE_FORK_WRONG_CWD").is_some() {
                    parent_cwd.as_str()
                } else {
                    cwd.to_str().unwrap()
                };
                reply(json!({"cwd":reported,"thread":{"id":session,"turns":[{"id":anchor}]}}));
            }
            Some("thread/resume") => {
                session = v["params"]["threadId"].as_str().unwrap().into();
                if std::env::var_os("FIXTURE_PAUSE_RESUME").is_some() {
                    fs::write(database.join("awaiting-resume"), &session).unwrap();
                    let _ = std::io::copy(&mut input, &mut std::io::sink());
                    return;
                }
                if std::env::var_os("FIXTURE_PAUSE_BRANCH").is_some()
                    && fs::read_to_string(database.join("forks"))
                        .is_ok_and(|s| s.lines().any(|id| id == session))
                {
                    fs::write(database.join("awaiting-publication"), &session).unwrap();
                    let _ = std::io::copy(&mut input, &mut std::io::sink());
                    return;
                }
                assert_eq!(
                    fs::read_to_string(database.join(&session)).unwrap(),
                    cwd.to_str().unwrap()
                );
                if database.join("usage-baseline").exists() {
                    emit(
                        json!({"method":"thread/tokenUsage/updated","params":{"threadId":session,"turnId":"prior","tokenUsage":{"total":{"inputTokens":10,"outputTokens":4}}}}),
                    );
                }
                // Native replay on attach, with identical content to a real reply.
                emit(
                    json!({"method":"item/completed","params":{"threadId":session,"item":{"id":"old","type":"agentMessage","text":"haha"}}}),
                );
                let turns: Value = fs::read(database.join("turns"))
                    .ok()
                    .map(|b| serde_json::from_slice(&b).unwrap())
                    .unwrap_or(json!([]));
                reply(json!({"cwd":cwd,"thread":{"id":session,"turns":turns}}));
            }
            Some("thread/sample") => {
                assert_eq!(v["params"]["threadId"], session);
                assert_eq!(v["params"]["effort"], "low");
                assert!(v["params"]["prompt"].as_str().unwrap().contains("Do not use tools"));
                let turn = v["params"]["expectedTurnId"].as_str().unwrap();
                let mode = fs::read_to_string(database.join("sample-mode")).unwrap();
                let mut calls = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(database.join("samples"))
                    .unwrap();
                writeln!(calls,"{turn}").unwrap();
                let value = json!({"sourceTurnId":turn,"text":format!("  suggested {mode}  ")});
                match mode.as_str() {
                    "late" => late_sample = Some(json!({"id":v["id"],"result":value})),
                    "wrong" => reply(json!({"sourceTurnId":"other","text":"unrelated"})),
                    "unsupported" => emit(json!({"id":v["id"],"error":{"code":-32601,"message":"method not found"}})),
                    _ => reply(value),
                }
            }
            Some("thread/read") => {
                assert_eq!(v["params"]["threadId"], session);
                assert_eq!(v["params"]["includeTurns"], true);
                let turns: Value = fs::read(database.join("read-turns"))
                    .ok()
                    .map(|b| serde_json::from_slice(&b).unwrap())
                    .unwrap_or(json!([]));
                reply(json!({"thread":{"id":session,"turns":turns}}));
            }
            Some("thread/compact/start") => {
                let mode = fs::read_to_string(database.join("compact-mode")).unwrap_or_default();
                let turn = Uuid::new_v4().to_string();
                let item = |thread: &str, method: &str, id: &str| {
                    emit(json!({"method":method,"params":{
                    "threadId":thread,"turnId":turn,"item":{"type":"contextCompaction","id":id}}}))
                };
                let end = |thread: &str, id: &str, status: &str| {
                    emit(json!({"method":"turn/completed","params":{
                    "threadId":thread,"turn":{"id":id,"status":status}}}))
                };
                if mode == "refuse" {
                    emit(json!({"id":v["id"],"error":{"message":"native refusal"}}));
                    continue;
                }
                if mode != "late-ack" && mode != "after-item" {
                    reply(json!({}));
                }
                item("other-session", "item/started", "other");
                item("other-session", "item/completed", "other");
                end("other-session", &turn, "completed");
                item(&session, "item/started", "manual");
                item(&session, "item/completed", "wrong");
                end(&session, "unrelated", "completed");
                if mode == "after-item" {
                    item(&session, "item/completed", "manual");
                    reply(json!({}));
                }
                fs::write(database.join("compact-started"), "").unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                while database.join("compact-hold").exists() {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if mode != "missing" {
                    item(&session, "item/completed", "manual");
                }
                end(
                    &session,
                    &turn,
                    if mode == "abort" {
                        "interrupted"
                    } else {
                        "completed"
                    },
                );
                if mode == "late-ack" {
                    reply(json!({}));
                }
            }
            Some("turn/steer") => {
                let params = &v["params"];
                assert!(
                    params["clientUserMessageId"]
                        .as_str()
                        .is_some_and(|id| Uuid::parse_str(id).is_ok())
                );
                let turn = params["expectedTurnId"].as_str().unwrap();
                if params["input"][0]["text"] == "refused steer" {
                    emit(
                        json!({"id":v["id"], "error":{"code":-32602,"message":"steering refused"}}),
                    );
                } else if params["input"][0]["text"] == "echo before reply" {
                    let echo = |thread: &str, turn: &str| {
                        emit(json!({"method":"item/started","params":{
                        "threadId":thread,"turnId":turn,"item":{"type":"userMessage","id":"steer-item",
                            "clientId":params["clientUserMessageId"],"content":params["input"]}}}))
                    };
                    echo("other-thread", turn);
                    echo(&session, "unrelated-turn");
                    echo(&session, turn);
                    echo(&session, turn);
                    reply(json!({"turnId":turn}));
                } else {
                    // Deliberately deliver the RPC reply after its root turn is over.
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":session,"turn":{"id":turn,"status":"completed"}}}),
                    );
                    reply(json!({"turnId":turn}));
                }
            }
            Some("feedback/upload")=>{
                assert_eq!(v["params"]["threadId"],session);assert_eq!(v["params"]["classification"],"bug");assert_eq!(v["params"]["includeLogs"],true);
                assert_eq!(v["params"]["reason"],"private-fixture-feedback-reason");
                writeln!(OpenOptions::new().create(true).append(true).open(database.join("feedback-count")).unwrap(),"upload").unwrap();
                reply(json!({"threadId":"provider-receipt"}));
            }
            Some("mcpServer/oauth/login")=>{
                assert_eq!(v["params"]["threadId"],session);
                if v["params"]["name"]=="refused"{
                    emit(json!({"id":v["id"],"error":{"code":-32601,"message":"fixture-private-native-error"}}));continue;
                }
                assert_eq!(v["params"]["name"],"vault");
                let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let redirect=format!("http://{}/cb",listener.local_addr().unwrap());
                let mut target=url::Url::parse("https://id.example.org/oauth").unwrap();
                target.query_pairs_mut().append_pair("redirect_uri",&redirect).append_pair("state","private-fixture-state");
                let db=database.clone();let root=session.clone();
                std::thread::spawn(move||{
                    let(mut c,_)=listener.accept().unwrap();
                    let mut request=String::new();std::io::BufReader::new(c.try_clone().unwrap()).read_line(&mut request).unwrap();
                    assert!(request.starts_with("GET /cb?") && request.contains("code=private-fixture-code"));
                    c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    std::fs::write(db.join("oauth-connected"),"ok").unwrap();
                    emit(json!({"method":"mcpServer/oauthLogin/completed","params":{"threadId":root,"name":"vault","success":true,"error":"private-fixture-code"}}));
                });
                reply(json!({"authorizationUrl":target.as_str()}));
            }
            Some("mcpServerStatus/list")=>{
                if v["params"]["serverName"]=="vault" || v["params"]["serverName"]=="refused" {
                    assert_eq!(v["params"]["threadId"],session);
                    reply(json!({"data":[{"name":v["params"]["serverName"],"runtimeStatus":if database.join("oauth-connected").exists(){"connected"}else{"needsAuth"}}],"nextCursor":null}));continue;
                }
                assert_eq!(v["params"]["threadId"],session);assert_eq!(v["params"]["serverName"],"weather");
                reply(json!({"data":[{"name":"weather","tools":{
                    "lookup":{"name":"lookup","title":"Weather lookup","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}},
                    "hidden":{"name":"hidden","_meta":{"ui":{"visibility":["model"]}}}
                }}],"nextCursor":null}));
            }
            Some("mcpServer/resource/read")=>{
                assert_eq!(v["params"]["threadId"],session);assert_eq!(v["params"]["server"],"weather");
                let uri=&v["params"]["uri"];reply(json!({"contents":[{"uri":uri,"mimeType":"text/html;profile=mcp-app","text":"<p>weather app</p>"}]}));
            }
            Some("mcpServer/tool/call")=>{
                assert_eq!(v["params"]["threadId"],session);assert_eq!(v["params"]["server"],"weather");assert_eq!(v["params"]["tool"],"lookup");
                writeln!(OpenOptions::new().create(true).append(true).open(database.join("mcp-user-calls")).unwrap(),"call").unwrap();
                reply(json!({"content":[],"structuredContent":v["params"]["arguments"]}));
            }
            Some("turn/start") => {
                if let Some(stale) = late_sample.take() {
                    emit(stale);
                }
                let text = v.pointer("/params/input/0/text").and_then(Value::as_str).unwrap_or("");
                if text == "edited A" {
                    assert_eq!(
                        v.pointer("/params/input/1/url").and_then(Value::as_str),
                        Some("data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGMwLIgHAAHWAQH4l8EzAAAAAElFTkSuQmCC")
                    );
                }
                match text {
                    "shared-A" | "shared-B" => {
                        fs::write(database.join(text), []).unwrap();
                        let other = if text == "shared-A" {
                            "shared-B"
                        } else {
                            "shared-A"
                        };
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while !database.join(other).is_file() {
                            assert!(
                                Instant::now() < deadline,
                                "parallel chat did not reach its turn"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        fs::write("shared", text).unwrap();
                    }
                    "shared-next" | "shared-child" => fs::write("shared", text).unwrap(),
                    "edit selected repositories" => {
                        fs::write("api/turn-change.txt", "api change\n").unwrap();
                        fs::write("wrapper/web/turn-change.txt", "web change\n").unwrap();
                    }
                    "edit selected again" => {
                        fs::write("api/turn-change.txt", "api second\n").unwrap();
                        fs::write("wrapper/web/turn-change.txt", "web second\n").unwrap();
                    }
                    "edit-one" => {
                        fs::write("staged.txt", b"first").unwrap();
                        fs::write("binary", [0, 255, 6]).unwrap();
                        fs::remove_file("gone").unwrap();
                        fs::write("shape", b"leaf").unwrap();
                        fs::write("ignored.tmp", b"first ignored").unwrap();
                    }
                    "edit-two" => {
                        fs::write("staged.txt", b"second").unwrap();
                        fs::remove_file("binary").unwrap();
                        fs::remove_file("shape").unwrap();
                        fs::create_dir("shape").unwrap();
                        fs::write("shape/child", "child").unwrap();
                        fs::write("new file", "fresh").unwrap();
                        fs::write("ignored.tmp", b"second ignored").unwrap();
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::{PermissionsExt, symlink};
                            fs::set_permissions("mode", fs::Permissions::from_mode(0o755)).unwrap();
                            symlink("../elsewhere", "link").unwrap();
                        }
                    }
                    "edit-three" => {
                        fs::write("staged.txt", "third").unwrap();
                        fs::write("new file", [0, 255, 3]).unwrap();
                        fs::remove_file("shape/child").unwrap();
                        fs::remove_dir("shape").unwrap();
                        fs::write("binary", [0, 128, 255]).unwrap();
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions("mode", fs::Permissions::from_mode(0o644)).unwrap();
                            fs::remove_file("link").unwrap();
                        }
                    }
                    "branch-edit" => fs::write("branch", "branch").unwrap(),
                    _ => {}
                }
                let mut calls = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(database.join("calls"))
                    .unwrap();
                let images: Vec<_> = v["params"]["input"].as_array().unwrap().iter()
                    .filter(|part| part["type"] == "image").map(|part| &part["url"]).collect();
                writeln!(calls, "{}", json!({"id":session,"cwd":cwd,"text":text,"input":v["params"]["clientUserMessageId"],"images":images})).unwrap();
                if text == "hold without reply" {
                    continue;
                }
                let turn = Uuid::new_v4().to_string();
                reply(json!({"turn":{"id":turn}}));
                emit(
                    json!({"method":"turn/started","params":{"threadId":session,"turn":{"id":turn}}}),
                );
                if text.contains("KAR-MCP-APP-CONTEXT-NEXT"){fs::write(database.join("app-wire-next"),text).unwrap();}
                if text=="KAR-MCP-APP-ITEM" {
                    for method in ["item/started","item/completed"] {
                        emit(json!({"method":method,"params":{"threadId":session,"turnId":turn,"item":{"type":"mcpToolCall","id":"card","server":"weather","tool":"launch","mcpAppUi":{"resourceUri":"ui://weather/main"},"arguments":{},"status":"completed","result":{"content":[]}}}}));
                    }
                }
                if text.starts_with("mcp collaboration run") {
                    assert!(text.contains("kar_submit_output"));
                    fs::write(database.join("collaboration-ready"), &turn).unwrap();
                    let limit = Instant::now() + Duration::from_secs(60);
                    while database.join("hold-collaboration").is_file() {
                        assert!(Instant::now() < limit);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                if text=="delegation wait in parent" {
                    fs::write(database.join("delegation-parent-ready"),&turn).unwrap();
                    let limit=Instant::now()+Duration::from_secs(100);
                    while database.join("hold-delegation-parent").is_file(){
                        assert!(Instant::now()<limit);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                if text == "shared-next" && database.join("hold-parent-turn").exists() {
                    fs::write(database.join("parent-turn-running"), []).unwrap();
                    let until = Instant::now() + Duration::from_secs(30);
                    while database.join("hold-parent-turn").exists() {
                        assert!(Instant::now() < until, "held parent turn timed out");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                if text == "await queue steer" {
                    fs::write(database.join("steer-ready"), &turn).unwrap();
                    let mut line = String::new();
                    if input.read_line(&mut line).unwrap() == 0 {
                        return;
                    }
                    let steer: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(steer["method"], "turn/steer");
                    assert_eq!(steer["params"]["expectedTurnId"], turn);
                    assert_eq!(steer["params"]["threadId"], session);
                    assert_eq!(
                        steer["params"]["input"][0]["text"],
                        "apply inside this turn"
                    );
                    assert_eq!(
                        steer.pointer("/params/input/1/url").and_then(Value::as_str),
                        Some("data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGMwLIgHAAHWAQH4l8EzAAAAAElFTkSuQmCC")
                    );
                    fs::write(
                        database.join("steered-text"),
                        steer["params"]["input"][0]["text"].as_str().unwrap(),
                    )
                    .unwrap();
                    emit(json!({"id":steer["id"],"result":{"turnId":turn}}));
                }
                if text == "await interrupt" {
                    fs::write(database.join("interrupt-ready"), &turn).unwrap();
                    let mut line = String::new();
                    if input.read_line(&mut line).unwrap() == 0 {
                        return;
                    }
                    let interrupt: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(interrupt["method"], "turn/interrupt");
                    assert_eq!(interrupt["params"]["turnId"], turn);
                    assert_eq!(interrupt["params"]["threadId"], session);
                    emit(json!({"id":interrupt["id"],"result":{}}));
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":session,"turn":{"id":turn,"status":"interrupted"}}}),
                    );
                    continue;
                }
                if text == "hold for scoped steer" {
                    continue;
                }
                if text == "ask clients" {
                    emit(
                        json!({"id":901,"method":"item/tool/requestUserInput","params":{"threadId":session,"turnId":turn,"questions":[{"id":"q","question":"Choose a color"}]}}),
                    );
                    let mut answer = String::new();
                    if input.read_line(&mut answer).unwrap() == 0 {
                        return;
                    }
                    let answer: Value = serde_json::from_str(&answer).unwrap();
                    assert_eq!(answer["id"], 901);
                    assert!(answer["result"]["answers"]["q"]["answers"][0].is_string());
                    let mut seen = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(database.join("form-answers"))
                        .unwrap();
                    writeln!(
                        seen,
                        "{}",
                        json!({"session":session,"turn":turn,"answer":answer["result"]})
                    )
                    .unwrap();
                }
                if text == "declared quota" {
                    emit(
                        json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":100,"resetsAt":1900000000}}}}),
                    );
                    emit(
                        json!({"method":"error","params":{"threadId":session,"turnId":turn,"willRetry":false}}),
                    );
                    emit(
                        json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":10}}}}),
                    );
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":session,"turn":{"id":turn,"status":"failed","error":{"codexErrorInfo":"usageLimitExceeded","message":"workspace test allowance exhausted"}}}}),
                    );
                    continue;
                }
                if text == "open deferred tool" {
                    fs::write(database.join("deferred-owner"), &turn).unwrap();
                    emit(
                        json!({"method":"item/started","params":{"turnId":turn,"threadId":session,"item":{"type":"commandExecution","id":"deferred","command":"fixture"}}}),
                    );
                }
                if text == "finish deferred tool" {
                    let earlier = fs::read_to_string(database.join("deferred-owner")).unwrap();
                    for (owner, id, output) in [
                        ("unknown", "deferred", "wrong turn"),
                        (&earlier, "other", "unknown call"),
                        (&earlier, "deferred", "late output"),
                    ] {
                        emit(
                            json!({"method":"item/commandExecution/outputDelta","params":{"threadId":session,"turnId":owner,"itemId":id,"delta":output}}),
                        );
                    }
                    emit(
                        json!({"method":"item/completed","params":{"threadId":session,"turnId":earlier,"item":{"type":"commandExecution","id":"deferred","exitCode":9,"status":"failed"}}}),
                    );
                }
                if text == "initial" {
                    fs::write(database.join("usage-baseline"), "").unwrap();
                    emit(
                        json!({"method":"thread/tokenUsage/updated","params":{"threadId":session,"turnId":turn,"tokenUsage":{"total":{"inputTokens":10,"outputTokens":4}}}}),
                    );
                }
                if text == "watch command output" {
                    let status = |method: &str, item: Value| {
                        emit(
                            json!({"method":method,"params":{"threadId":session,"turnId":turn,"item":item}}),
                        )
                    };
                    status(
                        "item/started",
                        json!({"type":"commandExecution","id":"output-target","command":"fixture"}),
                    );
                    for (signal, chunk) in [
                        ("output-append", String::from("10%\r20%")),
                        ("output-replace", "x".repeat(18000)),
                    ] {
                        let deadline = Instant::now() + Duration::from_secs(15);
                        while !database.join(signal).exists() {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        emit(
                            json!({"method":"item/commandExecution/outputDelta","params":{"turnId":turn,"threadId":session,"itemId":"output-target","delta":chunk}}),
                        );
                    }
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while !database.join("output-finish").exists() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    status(
                        "item/completed",
                        json!({"type":"commandExecution","id":"output-target","status":"completed","exitCode":0,"aggregatedOutput":"done\rPASS\n"}),
                    );
                }
                if text == "observe live tool" {
                    let tool = |method: &str, status: &str| json!({"method":method,"params":{"threadId":session,"turnId":turn,"item":{"type":"commandExecution","id":"stream-tool","command":"fixture","status":status,"aggregatedOutput":"result"}}});
                    emit(tool("item/started", "inProgress"));
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while !database.join("release-stream-tool").exists() {
                        assert!(Instant::now() < deadline, "tool release timed out");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    emit(tool("item/completed", "completed"));
                    for (method, status) in [
                        ("item/started", "inProgress"),
                        ("item/completed", "completed"),
                    ] {
                        let mut reused = tool(method, status);
                        reused["params"]["item"]["command"] = "second invocation".into();
                        reused["params"]["item"]["aggregatedOutput"] = "second result".into();
                        emit(reused);
                    }
                    let usage = |who: &str, input: i64, output: i64| {
                        emit(
                            json!({"method":"thread/tokenUsage/updated","params":{"threadId":who,"turnId":turn,"tokenUsage":{"total":{"inputTokens":input,"outputTokens":output,"cachedInputTokens":2}}}}),
                        )
                    };
                    usage("unrelated-thread", 1000, 1000);
                    usage("deep", 6, 3);
                    usage("worker", 14, 2);
                    usage("worker", 14, 2);
                    usage(&session, 21, 9);
                    for (parent, child) in [
                        ("worker", "deep"),
                        (&*session, "worker"),
                        (&*session, "worker"),
                    ] {
                        emit(
                            json!({"method":"item/completed","params":{"threadId":parent,"item":{"id":format!("link-{child}"),"type":"collabAgentToolCall","senderThreadId":parent,"receiverThreadIds":[child]}}}),
                        );
                    }

                    for method in ["item/started", "item/completed", "thread/compacted"] {
                        emit(
                            json!({"method":method,"params":{"threadId":session,"turnId":turn,"item":{"type":"contextCompaction","id":"compact-1"}}}),
                        );
                    }
                }
                // A late delivery for another turn must not complete the active handle.
                emit(
                    json!({"method":"turn/completed","params":{"threadId":session,"turn":{"id":"previous","status":"completed"}}}),
                );
                for chunk in ["ha", "ha"] {
                    emit(
                        json!({"method":"item/agentMessage/delta","params":{"threadId":session,"turnId":turn,"itemId":"a","delta":chunk}}),
                    );
                }
                if text == "crash here" {
                    continue;
                }
                let message = |id| json!({"method":"item/completed","params":{"threadId":session,"turnId":turn,"item":{"type":"agentMessage","id":id,"text":"haha"}}});
                emit(message("a"));
                emit(message("a"));
                emit(message("b"));
                emit(
                    json!({"method":"turn/completed","params":{"threadId":session,"turn":{"id":turn,"status":"completed"}}}),
                );
            }
            other => panic!("unexpected method {other:?}"),
        }
    }
}
