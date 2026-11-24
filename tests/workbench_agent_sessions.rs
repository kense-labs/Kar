//! The service, journal, compressed corpus and restart are real. The resumed Codex CLI is a protocol fixture.
#[path = "support/handoff_runtime.rs"]
mod runtime;
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod server;
use serde_json::{Value, json};
use server::json as ok;
use std::{fs, path::Path};
use uuid::Uuid;

fn lines(rows: &[Value]) -> Vec<u8> {
    let mut out = vec![];
    for r in rows {
        serde_json::to_writer(&mut out, r).unwrap();
        out.push(b'\n');
    }
    out
}
fn write(path: &Path, rows: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines(rows)).unwrap();
}
fn main() {
    if runtime::child() {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(run());
}
async fn run() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let allowed = root.join("projects");
    let cwd = allowed.join("project");
    let db = root.join("native");
    let state = root.join("kar");
    for p in [&cwd, &db] {
        fs::create_dir_all(p).unwrap()
    }
    let codex = db.join(format!("codex{}", std::env::consts::EXE_SUFFIX));
    fs::copy(std::env::current_exe().unwrap(), &codex).unwrap();
    let codex_home = root.join("codex");
    let claude_home = root.join("claude");
    let pi_home = root.join("pi");
    let config = root.join("instances.toml");
    let other = root.join("second-pi");
    let flat = other.join("custom");
    fs::write(
        &config,
        format!(
            "[instances.personal]
runtime='pi'
home={:?}
sessions={:?}
[instances.default-alias]
runtime='pi'
home={:?}
",
            other, flat, pi_home
        ),
    )
    .unwrap();
    let vars = [
        ("KAR_AGENT_SESSION_CONFIG", config.as_os_str()),
        ("KAR_CODEX_BIN", codex.as_os_str()),
        ("KAR_HANDOFF_FIXTURE", db.as_os_str()),
        ("KAR_HANDOFF_REAL_CODEX", std::ffi::OsStr::new("")),
        ("CODEX_HOME", codex_home.as_os_str()),
        ("CLAUDE_CONFIG_DIR", claude_home.as_os_str()),
        ("KAR_PI_AGENT_DIR", pi_home.as_os_str()),
    ];
    let id = Uuid::new_v4();
    let codex_path = codex_home.join(format!(
        "sessions/2026/11/24/rollout-2026-11-24-{id}.jsonl.zst"
    ));
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    let rows = lines(&[
        json!({"type":"session_meta","payload":{"cwd":cwd,"id":id}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"same input"}]}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"same input"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"retained"}]}}),
    ]);
    fs::write(
        &codex_path,
        ruzstd::encoding::compress_to_vec(&rows[..], ruzstd::encoding::CompressionLevel::Fastest),
    )
    .unwrap();
    fs::write(db.join(id.to_string()), cwd.as_os_str().as_encoded_bytes()).unwrap();
    write(
        &codex_home.join("session_index.jsonl"),
        &[json!({"id":id,"thread_name":"Codex saved title"})],
    );
    let claude = Uuid::new_v4();
    let claude_path = claude_home.join(format!("projects/saved/{claude}.jsonl"));
    let mut transcript = vec![
        json!({"type":"user","cwd":cwd,"sessionId":claude,"message":{"content":"Claude question"}}),
        json!({"type":"user","isSidechain":true,"message":{"content":"private sidechain"}}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"Claude answer"}]}}),
    ];
    write(&claude_path, &transcript);
    let pi = Uuid::new_v4();
    let pi_path = pi_home.join("sessions/custom.jsonl");
    write(
        &pi_path,
        &[
            json!({"type":"session","id":pi,"cwd":cwd}),
            json!({"type":"message","id":"a","message":{"role":"user","content":"Pi question"}}),
            json!({"type":"message","id":"b","parentId":"a","message":{"role":"assistant","content":"abandoned"}}),
            json!({"type":"message","id":"c","parentId":"a","message":{"role":"assistant","content":"active"}}),
        ],
    );
    write(
        &flat.join("duplicate-id.jsonl"),
        &[
            json!({"type":"session","id":pi,"cwd":cwd}),
            json!({"type":"message","message":{"role":"user","content":"different Pi profile"}}),
            json!({"type":"message","message":{"role":"assistant","content":"second account"}}),
        ],
    );
    let dir = codex_path.parent().unwrap();
    write(
        &dir.join("rollout-subagent.jsonl"),
        &[
            json!({"type":"session_meta","payload":{"id":Uuid::new_v4(),"cwd":cwd,"source":{"subagent":"review"}}}),
        ],
    );
    let outside = root.join("private");
    fs::create_dir(&outside).unwrap();
    write(
        &dir.join("rollout-private.jsonl"),
        &[json!({"type":"session_meta","payload":{"id":Uuid::new_v4(),"cwd":outside}})],
    );
    let http = server::client();
    let (mut server, base) = server::serve_with_env(&state, &allowed, &db, &vars).await;
    let scan = ok(http.get(format!("{base}/agent-sessions/scan"))).await;
    assert_eq!(scan["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(scan["candidates"][0]["thread_count"], 4);
    assert_eq!(scan["candidates"][0]["already_imported"], false);
    let p = ok(http
        .post(format!("{base}/projects"))
        .json(&json!({"root":cwd,"name":"project"})))
    .await;
    let p = p["id"].as_str().unwrap();
    let import = |base: &str| format!("{base}/projects/{p}/agent-sessions/import");
    let refuse = http
        .post(import(&base))
        .json(&json!({"expected_workspace_root":outside}))
        .send()
        .await
        .unwrap();
    assert!(!refuse.status().is_success());
    let first = ok(http
        .post(import(&base))
        .json(&json!({"expected_workspace_root":cwd})))
    .await;
    assert_eq!(first["imported_count"], 4);
    assert_eq!(first["skipped_count"], 0);
    let chats = ok(http.get(format!("{base}/projects/{p}/chats"))).await;
    let mut codex_chat = None;
    for chat in chats.as_array().unwrap() {
        let id = chat["info"]["id"].as_str().unwrap();
        let snap = ok(http.get(format!("{base}/projects/{p}/chats/{id}/snapshot"))).await;
        let turns = snap["history"]["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 1);
        let items = turns[0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let (input, reply) = match chat["info"]["runtime"].as_str().unwrap() {
            "codex" => {
                codex_chat = Some(id.to_owned());
                assert_eq!(chat["metadata"]["title"], "Codex saved title");
                ("same input", "retained")
            }
            "claude" => ("Claude question", "Claude answer"),
            "pi" if chat["info"]["imported"]["instance"]["id"] == "personal" => {
                ("different Pi profile", "second account")
            }
            "pi" => ("Pi question", "active"),
            _ => unreachable!(),
        };
        assert_eq!(items[0]["text"], input);
        assert_eq!(items[1]["text"], reply);
    }
    let codex_chat = codex_chat.unwrap();
    server.kill().await.unwrap();
    let (mut server, base) = server::serve_with_env(&state, &allowed, &db, &vars).await;
    let again = ok(http.post(import(&base)).json(&json!({}))).await;
    assert_eq!(again["chat_ids"], first["chat_ids"]);
    assert_eq!(again["imported_count"], 0);
    assert_eq!(again["already_present_count"], 4);
    transcript.push(
        json!({"type":"user","cwd":cwd,"sessionId":claude,"message":{"content":"external change"}}),
    );
    write(&claude_path, &transcript);
    let changed = ok(http.post(import(&base)).json(&json!({}))).await;
    assert_eq!(changed["skipped_count"], 1);
    let send = http
        .post(format!("{base}/projects/{p}/chats/{codex_chat}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"continue original"}));
    let result = ok(send).await;
    assert_eq!(result["status"], "completed");
    let audited = fs::read_to_string(db.join("inputs")).unwrap();
    let events = audited
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["session"], id.to_string());
    assert_eq!(events[0]["cwd"], json!(cwd));
    server.kill().await.unwrap();
}
