//! A real Kar controller, saved turn journal and HTTP server; native provider and its files are fixtures.
use super::{
    emit,
    workbench_fork::git,
    workbench_server::{client, json as ok, serve_with_env},
};
use kar::{SessionOptions, chat::ChatStore};
use serde_json::{Value, json};
use std::{fs, io::BufRead, path::Path};
use uuid::Uuid;

pub(super) fn peer() {
    let argv: Vec<_> = std::env::args().collect();
    assert!(
        argv.windows(2)
            .any(|a| a[0] == "--app-data-dir" && a[1] == "antigravity-acp")
    );
    let dir = argv.windows(2).find(|a| a[0] == "--gemini-dir").unwrap();
    assert_eq!(
        Some(dir[1].as_str()),
        std::env::var("KAR_TEST_CHILD_PROFILE").ok().as_deref()
    );
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let reply = |r| emit(json!({"jsonrpc":"2.0","id":v["id"],"result":r}));
        match v["method"].as_str().unwrap() {
            "initialize" => reply(
                json!({"protocolVersion":1,"agentInfo":{"name":"Antigravity","version":"1.5.15"},"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{}}}}),
            ),
            "session/new" => reply(json!({"sessionId":"native-root"})),
            "session/list" => reply(
                json!({"sessions":[{"sessionId":"native-root","cwd":std::env::current_dir().unwrap()}]}),
            ),
            "session/load" => reply(json!({})),
            "session/prompt" => {
                let id = v["params"]["prompt"][0]["text"].as_str().unwrap();
                let update = |u| {
                    emit(
                        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"native-root","update":u}}),
                    )
                };
                update(
                    json!({"sessionUpdate":"tool_call","title":"Run invoke_subagent","kind":"other","status":"in_progress","toolCallId":id,"rawInput":{"Role":"review","Prompt":if id.starts_with("pair"){"same"}else{id}},"_meta":{"is_mcp_tool_call":id=="mcp"}}),
                );
                update(
                    json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"completed"}),
                );
                reply(json!({"stopReason":"end_turn"}));
            }
            _ => reply(json!({})),
        }
    }
}
fn field(key: u64, b: &[u8]) -> Vec<u8> {
    fn var(mut v: u64) -> Vec<u8> {
        let mut out = vec![];
        while v >= 128 {
            out.push((v as u8 & 127) | 128);
            v >>= 7;
        }
        out.push(v as u8);
        out
    }
    [var((key << 3) | 2), var(b.len() as u64), b.to_vec()].concat()
}
fn trajectory(base: &Path) {
    use sqlite_core::{
        Value as S,
        rebuild::{RecoveredTable, build_recovered_db_tables},
    };
    let planner = field(
        20,
        &field(
            7,
            &[field(1, b"call-one"), field(2, b"invoke_subagent")].concat(),
        ),
    );
    let mut rows = vec![vec![
        S::Integer(1),
        S::Integer(1),
        S::Integer(0),
        S::Blob(planner),
    ]];
    for (index, call, id, prompt) in [
        (2, "different", "other", "other launch"),
        (3, "call-one", "one", "native-root:1"),
        (4, "third", "three", "random"),
        (5, "pair-call", "pair", "same"),
    ] {
        let uri = url::Url::from_file_path(
            base.join("brain")
                .join(id)
                .join(".system_generated/logs/transcript.jsonl"),
        )
        .unwrap();
        let specs = field(
            9,
            &[field(2, b"review"), field(3, prompt.as_bytes())].concat(),
        );
        let result = field(
            10,
            &[field(1, id.as_bytes()), field(2, uri.as_str().as_bytes())].concat(),
        );
        let p = [
            field(5, &field(4, &field(1, call.as_bytes()))),
            field(143, &[specs, result].concat()),
        ]
        .concat();
        rows.push(vec![
            S::Integer(index),
            S::Integer(127),
            S::Integer(0),
            S::Blob(p),
        ]);
        transcript(base, id, prompt);
    }
    fs::create_dir_all(base.join("conversations")).unwrap();
    fs::write(
        base.join("conversations/native-root.db"),
        build_recovered_db_tables(&[RecoveredTable {
            name: "steps".into(),
            columns: ["idx", "step_type", "step_format", "step_payload"]
                .map(String::from)
                .to_vec(),
            rows,
        }]),
    )
    .unwrap();
}
fn transcript(base: &Path, child: &str, prompt: &str) {
    let logs = base
        .join("brain")
        .join(child)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let hidden =
        json!({"step_index":0,"type":"SYSTEM_MESSAGE","content":format!("Start: {prompt}")});
    let step = json!({"step_index":1,"type":"PLANNER_RESPONSE","content":child,"tool_calls":[{"name":"read","args":{"offset":"12"}}]});
    fs::write(logs.join("transcript.jsonl"), format!("{hidden}\n{step}\n")).unwrap();
}
fn detached(base: &Path, child: &str, id: &str) {
    let root = base.join("brain/native-root/.system_generated/subagents");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(format!("{child}.json")),
        json!({"conversationId":child,"subagentDescriptor":{"role":"review"}}).to_string(),
    )
    .unwrap();
    let inbox = base
        .join("brain")
        .join(child)
        .join(".system_generated/messages");
    fs::create_dir_all(&inbox).unwrap();
    fs::write(inbox.join("launch.json"),json!({"sender":"native-root","content":id,"sourceMetadata":{"tool":{"conversationId":"native-root","toolCall":{"name":"invoke_subagent","id":id}}}}).to_string()).unwrap();
    transcript(base, child, id);
}
async fn read(a: &reqwest::Client, chat: &str, turn: Uuid, tool: &str) -> Value {
    ok(a.get(format!("{chat}/turns/{turn}/tools/{tool}/children"))).await["children"].clone()
}
#[tokio::main]
pub(super) async fn run() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let allowed = base.join("roots");
    let work = allowed.join("repo");
    let state = base.join("state");
    let native = base.join("codex");
    let profile = base.join("profile");
    let decoy = base.join("decoy");
    for p in [&work, &native, &profile, &decoy] {
        fs::create_dir_all(p).unwrap();
    }
    git(&work, &["init", "-b", "main"]);
    fs::write(work.join("initial"), "base").unwrap();
    git(&work, &["add", "initial"]);
    git(&work, &["commit", "-m", "base"]);
    let files = profile.join("antigravity-acp");
    trajectory(&files);
    detached(&files, "worker", "detach");
    detached(&files, "impostor", "mcp");
    detached(&decoy.join("antigravity-acp"), "wrong-profile", "detach");
    let store = ChatStore::open(&state).unwrap();
    let mut opt = SessionOptions::new(&work);
    opt.launch_args
        .extend(["--app-data-dir".into(), "antigravity-acp".into()]);
    for name in ["GEMINI_HOME", "KAR_TEST_CHILD_PROFILE"] {
        opt.env
            .insert(name.into(), profile.clone().into_os_string());
    }
    let s = store
        .create("antigravity", Some(&std::env::current_exe().unwrap()), opt)
        .await
        .unwrap();
    let chat = s.info().id;
    let mut turns = std::collections::HashMap::new();
    for name in [
        "detach",
        "native-root:1",
        "random",
        "pair-a",
        "pair-b",
        "mcp",
    ] {
        let id = Uuid::new_v4();
        s.send(id, name).await.unwrap();
        turns.insert(name, id);
    }
    s.close().await.unwrap();
    let s = store
        .resume_with(chat, |o| {
            o.env
                .insert("GEMINI_HOME".into(), decoy.clone().into_os_string());
            o.env.insert(
                "KAR_TEST_CHILD_PROFILE".into(),
                profile.clone().into_os_string(),
            );
        })
        .await
        .unwrap();
    s.close().await.unwrap();
    let (mut server, url) = serve_with_env(
        &state,
        &allowed,
        &native,
        &[("GEMINI_HOME", decoy.as_os_str())],
    )
    .await;
    let a = client();
    let p = ok(a
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"native work"})))
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let root = format!("{url}/projects/{p}/chats/{chat}");
    for (name, child) in [
        ("detach", "worker"),
        ("native-root:1", "one"),
        ("random", "three"),
    ] {
        let children = read(&a, &root, turns[name], name).await;
        assert_eq!(children.as_array().unwrap().len(), 1, "{name}: {children}");
        assert_eq!(children[0]["conversation_id"], child);
        assert_eq!(children[0]["steps"].as_array().unwrap().len(), 1);
        assert_eq!(children[0]["steps"][0]["text"], child);
        assert_eq!(
            children[0]["steps"][0]["tool_calls"][0]["input"],
            r#"{"offset":12}"#
        );
    }
    for (turn, tool) in [("detach", "random"), ("pair-a", "pair-a"), ("mcp", "mcp")] {
        assert_eq!(read(&a, &root, turns[turn], tool).await, json!([]));
    }
    let switch = ok(a
        .post(format!("{root}/handoffs"))
        .json(&json!({"id":Uuid::new_v4(),"runtime":"codex"})))
    .await;
    assert_eq!(switch["status"], "ready");
    let cut = ok(a
        .post(format!("{root}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"branch anchor"})))
    .await;
    let fork = Uuid::new_v4();
    ok(a.post(format!("{root}/forks"))
        .json(&json!({"id":fork,"turn":cut["id"]})))
    .await;
    let inherited = format!("{url}/projects/{p}/chats/{fork}");
    assert_eq!(
        read(&a, &inherited, turns["detach"], "detach").await[0]["conversation_id"],
        "worker"
    );
    detached(&files, "duplicate", "detach");
    assert_eq!(read(&a, &root, turns["detach"], "detach").await, json!([]));
    #[cfg(unix)]
    {
        let log = files.join("brain/one/.system_generated/logs/transcript.jsonl");
        fs::remove_file(&log).unwrap();
        std::os::unix::fs::symlink(
            files.join("brain/other/.system_generated/logs/transcript.jsonl"),
            &log,
        )
        .unwrap();
        let result = read(&a, &root, turns["native-root:1"], "native-root:1").await;
        assert_eq!(result[0]["conversation_id"], "one");
        assert!(result[0]["steps"].is_null());
    }
    server.kill().await.unwrap();
}
#[tokio::main]
pub(super) async fn live() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("missing {k}"));
    let mut url = url::Url::parse(&env("KAR_CHILD_WORK_URL")).unwrap();
    assert!(
        matches!(url.scheme(), "http" | "https")
            && (matches!(url.host(),Some(url::Host::Ipv4(a)) if a.is_loopback())
                || matches!(url.host(),Some(url::Host::Ipv6(a)) if a.is_loopback())
                || url.host_str() == Some("localhost"))
    );
    let project = Uuid::parse_str(&env("KAR_CHILD_WORK_PROJECT"))
        .unwrap()
        .to_string();
    let chat = Uuid::parse_str(&env("KAR_CHILD_WORK_CHAT"))
        .unwrap()
        .to_string();
    let turn = Uuid::parse_str(&env("KAR_CHILD_WORK_TURN"))
        .unwrap()
        .to_string();
    let tool = env("KAR_CHILD_WORK_TOOL");
    url.path_segments_mut().unwrap().pop_if_empty().extend([
        "projects", &project, "chats", &chat, "turns", &turn, "tools", &tool, "children",
    ]);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let result = ok(client.get(url).bearer_auth(env("KAR_WORKBENCH_TOKEN"))).await;
    let items = result["children"].as_array().unwrap();
    let expected = env("KAR_CHILD_WORK_EXPECTED");
    let expected: Vec<_> = expected.split(',').collect();
    assert!(!expected.contains(&""));
    assert_eq!(
        items
            .iter()
            .map(|i| i["conversation_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        items
            .iter()
            .any(|i| i["steps"].as_array().is_some_and(|s| !s.is_empty()))
    );
    println!(
        "Kar HTTP read {} expected native children with retained transcript steps",
        items.len()
    );
}
