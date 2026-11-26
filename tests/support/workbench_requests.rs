use super::{
    sse::SseClient,
    workbench_server::{client, json as ok, serve, serve_with_env},
};
use kar::chat::{ChatFeedSnapshot, ChatReplica};
use reqwest::Client;
use serde_json::{Value, json};
use std::{fs, time::Duration};
use uuid::Uuid;

async fn pending(client: &Client, chat: &str) -> Value {
    loop {
        let snapshot = ok(client.get(format!("{chat}/snapshot"))).await;
        if let Some(request) = snapshot["history"]["pending_requests"].get(0) {
            return request.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
fn begin(client: &Client, chat: &str) -> tokio::task::JoinHandle<Value> {
    let post = client
        .post(format!("{chat}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"ask clients"}));
    tokio::spawn(ok(post))
}
fn answer(client: &Client, chat: &str, request: &Value, color: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{chat}/requests/respond"))
        .json(&json!({
            "cursor":request["cursor"],"id":request["id"],"action":"question",
            "reply":{"action":"submit","values":{"q":color}}
        }))
}
#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(90), scenario())
        .await
        .unwrap();
}
async fn scenario() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let native = temp.path().join("native");
    let state = temp.path().join("state");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&native).unwrap();
    let a = client();
    let b = client();
    let (mut server, url) = serve(&state, &work, &native).await;
    let project = ok(a
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"requests"})))
    .await;
    let p = project["id"].as_str().unwrap();
    let id = Uuid::new_v4();
    let created = ok(a
        .post(format!("{url}/projects/{p}/chats"))
        .json(&json!({"id":id,"runtime":"codex","approvals":"host"})))
    .await;
    let path = format!("/projects/{p}/chats/{id}");
    let chat = format!("{url}{path}");
    let sending = begin(&a, &chat);
    let first = pending(&b, &chat).await;
    let attached = ok(b.post(format!("{chat}/attach"))).await;
    assert_eq!(attached[0]["cursor"], first["cursor"]);
    let (left, right) = tokio::join!(
        answer(&a, &chat, &first, "green").send(),
        answer(&b, &chat, &first, "blue").send()
    );
    let wins = [left.unwrap(), right.unwrap()]
        .into_iter()
        .filter(|r| r.status().is_success())
        .count();
    assert_eq!(wins, 1);
    assert_eq!(sending.await.unwrap()["status"], "completed");
    server.kill().await.unwrap();
    let (mut server, url) = serve(&state, &work, &native).await;
    let chat = format!("{url}{path}");
    assert_eq!(ok(b.post(format!("{chat}/attach"))).await, json!([]));
    let sending = begin(&b, &chat);
    let second = pending(&a, &chat).await;
    assert_eq!(second["id"], first["id"]);
    assert_ne!(second["cursor"], first["cursor"]);
    assert!(
        !answer(&a, &chat, &first, "stale")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let store = kar::chat::ChatStore::open(&state).unwrap();
    let attachment = Uuid::new_v4();
    let notebook = br#"{"cells":[{"cell_type":"markdown","source":"client evidence"}]}"#;
    store
        .persist_attachment(
            id,
            attachment,
            "reference.ipynb",
            "application/json",
            kar::chat::AttachmentKind::File,
            notebook,
        )
        .unwrap();
    let other: Uuid = Uuid::new_v4();
    ok(a.post(format!("{url}/projects/{p}/chats"))
        .json(&json!({"id":other,"runtime":"codex"})))
    .await;
    let outside = Uuid::new_v4();
    store
        .persist_attachment(
            other,
            outside,
            "private.txt",
            "text/plain",
            kar::chat::AttachmentKind::File,
            b"other chat",
        )
        .unwrap();
    let with_files = |qid: &str, file: Uuid| {
        a.post(format!("{chat}/requests/respond")).json(&json!({
        "id":second["id"],"cursor":second["cursor"],"action":"question","reply":{"action":"submit","values":{"q":"second"}},
        "attachments_by_question_id":{(qid):[file]}
    }))
    };
    for invalid in [with_files("unknown", attachment), with_files("q", outside)] {
        assert!(!invalid.send().await.unwrap().status().is_success());
    }
    ok(with_files("q", attachment)).await;
    assert_eq!(sending.await.unwrap()["status"], "completed");
    let disk = fs::read_to_string(native.join("form-answers")).unwrap();
    let receipts: Vec<Value> = disk
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(receipts.len(), 2);
    assert!(
        receipts
            .iter()
            .all(|r| r["session"] == created["info"]["native_id"])
    );
    let value = receipts[1]["answer"]["answers"]["q"]["answers"][0]
        .as_str()
        .unwrap();
    let refs: Vec<Value> = serde_json::from_str(
        value
            .strip_prefix("second\n\nAttached files (JSON):\n")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["name"], "reference.ipynb");
    let extracted = fs::read_to_string(refs[0]["extracted_text_path"].as_str().unwrap()).unwrap();
    assert_eq!(extracted, "## Cell 1 (markdown)\nclient evidence");
    assert_eq!(
        fs::read(refs[0]["path"].as_str().unwrap()).unwrap(),
        notebook
    );
    let turn = Uuid::new_v4();
    let post = a
        .post(format!("{chat}/messages"))
        .json(&json!({"id":turn,"text":"await interrupt"}));
    let sending = tokio::spawn(ok(post));
    while !native.join("interrupt-ready").exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let stop = format!("{chat}/turns/{turn}/interrupt");
    for wrong in [
        format!("{chat}/turns/{}/interrupt", Uuid::new_v4()),
        format!("{stop}?request_id={}", Uuid::new_v4()),
    ] {
        assert_eq!(
            b.post(wrong).send().await.unwrap().status(),
            reqwest::StatusCode::CONFLICT
        );
    }
    ok(b.post(&stop)).await;
    assert_eq!(sending.await.unwrap()["status"], "aborted");
    assert_eq!(
        b.post(&stop).send().await.unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    let forced = Uuid::new_v4();
    let blocked = a
        .post(format!("{chat}/messages"))
        .json(&json!({"id":forced,"text":"ask clients"}));
    let waiting = tokio::spawn(blocked.send());
    let _open_form = pending(&b, &chat).await;
    let release = format!("{chat}/force-stop");
    let wrong = b
        .post(&release)
        .json(&json!({"turn":turn}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::CONFLICT);
    let queue = format!("{chat}/queue");
    ok(a.post(format!("{queue}/commands"))
        .json(&json!({"action":"hold","command_id":Uuid::new_v4()})))
    .await;
    let follow = Uuid::new_v4();
    ok(a.post(&queue)
        .json(&json!({"id":follow,"text":"after forced release"})))
    .await;
    let stopped = ok(b.post(&release).json(&json!({"turn":forced}))).await;
    assert_eq!(stopped["turn"], forced.to_string());
    assert_eq!(stopped["outcome"], "killed");
    let interrupted = waiting.await.unwrap().unwrap();
    assert!(!interrupted.status().is_success());
    let offline = ok(b.post(&release).json(&json!({"turn":forced}))).await;
    assert_eq!(offline["outcome"], "released");
    let snapshot = ok(b.get(format!("{chat}/snapshot"))).await;
    assert_eq!(
        snapshot["history"]["turns"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["status"],
        "interrupted"
    );
    ok(a.post(format!("{queue}/commands"))
        .json(&json!({"action":"resume","command_id":Uuid::new_v4()})))
    .await;
    loop {
        let snapshot = ok(b.get(format!("{chat}/snapshot"))).await;
        if snapshot["history"]["turns"]
            .as_array()
            .unwrap()
            .last()
            .is_some_and(|t| t["id"] == follow.to_string() && t["status"] == "completed")
        {
            assert_eq!(
                snapshot["history"]["chat"]["native_id"],
                created["info"]["native_id"]
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        b.post(&release)
            .json(&json!({"turn":forced}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    server.kill().await.unwrap();

    orphan_and_delivery_error_scenario().await;
}

const MOCK_CODEX_PY: &str = r#"#!/usr/bin/env python3
import json, os, sys, uuid

if "--help" in sys.argv:
    print("Usage: codex app-server [OPTIONS]")
    sys.exit(0)
if "--version" in sys.argv:
    print("codex-cli 0.171.0")
    sys.exit(0)

db = os.environ["FIXTURE_NATIVE_DB"]
cwd = os.getcwd()
session = ""

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    v = json.loads(raw)
    method = v.get("method")
    req_id = v.get("id")
    if method == "initialize":
        emit({"id": req_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "config/read":
        emit({"id": req_id, "result": {"config": {"model": "demo"}}})
    elif method == "thread/start":
        session = str(uuid.uuid4())
        with open(os.path.join(db, session), "w") as f:
            f.write(cwd)
        emit({"id": req_id, "result": {"thread": {"id": session}}})
    elif method == "thread/resume":
        session = v["params"]["threadId"]
        emit({"id": req_id, "result": {"cwd": cwd, "thread": {"id": session, "turns": []}}})
    elif method == "thread/read":
        emit({"id": req_id, "result": {"thread": {"id": session, "turns": []}}})
    elif method == "turn/start":
        turn = str(uuid.uuid4())
        text = v["params"]["input"][0]["text"]
        emit({"id": req_id, "result": {"turn": {"id": turn}}})
        emit({"method": "turn/started", "params": {"threadId": session, "turn": {"id": turn, "status": "inProgress"}}})
        if text == "ask question and permission":
            emit({
                "id": 901,
                "method": "item/tool/requestUserInput",
                "params": {
                    "threadId": session,
                    "turnId": turn,
                    "questions": [{
                        "id": "db",
                        "question": "Which database?",
                        "options": [{"label": "SQLite"}, {"label": "Postgres"}]
                    }]
                }
            })
            emit({
                "id": 902,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "threadId": session,
                    "turnId": turn,
                    "command": "rm -rf /tmp/demo"
                }
            })
            for _ in sys.stdin:
                pass
            sys.exit(0)
        elif text == "ask dismissable":
            emit({
                "id": 903,
                "method": "item/tool/requestUserInput",
                "params": {
                    "threadId": session,
                    "turnId": turn,
                    "questions": [{
                        "id": "env",
                        "question": "Target environment?",
                        "options": [{"label": "staging"}, {"label": "prod"}]
                    }]
                }
            })
            for _ in sys.stdin:
                pass
            sys.exit(0)
        elif text == "ask secret and crash":
            emit({
                "id": 904,
                "method": "item/tool/requestUserInput",
                "params": {
                    "threadId": session,
                    "turnId": turn,
                    "questions": [{
                        "id": "api_secret",
                        "question": "Enter API secret",
                        "isSecret": True
                    }]
                }
            })
            # Read the host's response line and crash immediately before confirming!
            sys.stdin.readline()
            sys.exit(1)
        else:
            with open(os.path.join(db, "resumed-prompts.jsonl"), "a") as f:
                f.write(json.dumps({"session": session, "text": text}) + "\n")
            emit({
                "method": "item/completed",
                "params": {
                    "threadId": session,
                    "turnId": turn,
                    "item": {"id": "msg-1", "type": "agentMessage", "text": "Resumed answer received"}
                }
            })
            emit({
                "method": "turn/completed",
                "params": {
                    "threadId": session,
                    "turn": {"id": turn, "status": "completed"}
                }
            })
"#;

async fn orphan_and_delivery_error_scenario() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let native = temp.path().join("native");
    let state = temp.path().join("state");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&native).unwrap();

    let mock_bin = temp.path().join("mock-codex.py");
    fs::write(&mock_bin, MOCK_CODEX_PY).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mock_bin, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let env = [("KAR_CODEX_BIN", mock_bin.as_os_str())];
    let a = client();
    let b = client();
    let (mut server, url) = serve_with_env(&state, &work, &native, &env).await;
    let project = ok(a
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"orphan-recovery"})))
    .await;
    let p = project["id"].as_str().unwrap().to_owned();
    let id = Uuid::new_v4();
    let created = ok(a
        .post(format!("{url}/projects/{p}/chats"))
        .json(&json!({"id":id,"runtime":"codex","approvals":"host"})))
    .await;
    let native_id = created["info"]["native_id"].as_str().unwrap().to_owned();
    let path = format!("/projects/{p}/chats/{id}");
    let chat = format!("{url}{path}");

    // 1. Ask both a recoverable question (901) and a permission (902), then kill Workbench.
    let _blocked = tokio::spawn(
        a.post(format!("{chat}/messages"))
            .json(&json!({"id":Uuid::new_v4(),"text":"ask question and permission"}))
            .send(),
    );
    let (q_req, perm_req) = loop {
        let snap = ok(b.get(format!("{chat}/snapshot"))).await;
        if let Some(reqs) = snap["history"]["pending_requests"].as_array()
            && reqs.len() == 2
        {
            let q = reqs.iter().find(|r| r["id"] == 901).unwrap().clone();
            let perm = reqs.iter().find(|r| r["id"] == 902).unwrap().clone();
            break (q, perm);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    server.kill().await.unwrap();
    let (mut server, url) = serve_with_env(&state, &work, &native, &env).await;
    let chat = format!("{url}{path}");

    // Two SSE clients reconnect after restart and both observe the exact same orphaned question,
    // while the invalidated permission is NOT converted into orphaned_questions.
    let snap_a = ok(a.get(format!("{chat}/snapshot"))).await;
    let snap_b = ok(b.get(format!("{chat}/snapshot"))).await;
    let mut view_a =
        ChatReplica::from_snapshot(serde_json::from_value::<ChatFeedSnapshot>(snap_a).unwrap());
    let mut view_b =
        ChatReplica::from_snapshot(serde_json::from_value::<ChatFeedSnapshot>(snap_b).unwrap());
    assert!(view_a.pending_requests().is_empty());
    assert_eq!(view_a.orphaned_questions().len(), 1);
    assert_eq!(view_a.orphaned_questions(), view_b.orphaned_questions());
    assert_eq!(view_a.orphaned_questions()[0].id, json!(901));
    assert_eq!(
        serde_json::to_value(&view_a.orphaned_questions()[0].cursor).unwrap(),
        q_req["cursor"]
    );

    let seed_cursor = view_a.cursor().to_string();
    let mut sse_a = SseClient::open_with_client(&a, &chat, &seed_cursor, Some(&seed_cursor)).await;
    let mut sse_b = SseClient::open_with_client(&b, &chat, &seed_cursor, None).await;

    // Answering the invalidated permission after restart is refused.
    let perm_attempt = a
        .post(format!("{chat}/requests/respond"))
        .json(&json!({
            "cursor": perm_req["cursor"],
            "id": perm_req["id"],
            "action": "decide",
            "decision": "allow_once"
        }))
        .send()
        .await
        .unwrap();
    assert!(!perm_attempt.status().is_success());

    // Invalid question payload returns 400 without consuming the orphaned question.
    let bad_attempt = a
        .post(format!("{chat}/requests/respond"))
        .json(&json!({
            "cursor": q_req["cursor"],
            "id": q_req["id"],
            "action": "question",
            "reply": {"action": "submit", "values": {"db": "InvalidChoice"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_attempt.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        ok(b.get(format!("{chat}/snapshot"))).await["history"]["orphaned_questions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Client A and Client B race to answer the orphaned question: exactly one wins, one fails.
    let submit = |cli: &Client, choice: &str| {
        cli.post(format!("{chat}/requests/respond")).json(&json!({
            "cursor": q_req["cursor"],
            "id": q_req["id"],
            "action": "question",
            "reply": {"action": "submit", "values": {"db": choice}}
        }))
    };
    let (res_a, res_b) = tokio::join!(submit(&a, "Postgres").send(), submit(&b, "SQLite").send());
    let statuses = [res_a.unwrap().status(), res_b.unwrap().status()];
    assert_eq!(statuses.iter().filter(|s| s.is_success()).count(), 1);
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == reqwest::StatusCode::BAD_REQUEST)
            .count(),
        1
    );

    // Both SSE clients observe the orphaned question clear and the resumed answer turn complete.
    for (view, feed) in [(&mut view_a, &mut sse_a), (&mut view_b, &mut sse_b)] {
        while !view.orphaned_questions().is_empty()
            || view.turns().len() < 2
            || view.turns().last().unwrap().status != kar::chat::TurnStatus::Completed
        {
            view.apply(feed.next().await).unwrap();
        }
    }
    assert_eq!(view_a.cursor(), view_b.cursor());
    let resumed_lines: Vec<Value> = fs::read_to_string(native.join("resumed-prompts.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(resumed_lines.len(), 1);
    assert_eq!(resumed_lines[0]["session"], native_id);
    let sent_text = resumed_lines[0]["text"].as_str().unwrap();
    assert!(sent_text.contains("Which database?"));
    assert!(sent_text.contains("Postgres") || sent_text.contains("SQLite"));
    drop((sse_a, sse_b));

    // 2. Dismiss subcase: two clients race to dismiss an orphaned question after restart.
    let _blocked = tokio::spawn(
        a.post(format!("{chat}/messages"))
            .json(&json!({"id":Uuid::new_v4(),"text":"ask dismissable"}))
            .send(),
    );
    let dismiss_req = pending(&b, &chat).await;
    assert_eq!(dismiss_req["id"], 903);
    server.kill().await.unwrap();
    let (mut server, url) = serve_with_env(&state, &work, &native, &env).await;
    let chat = format!("{url}{path}");

    let snap = ok(a.get(format!("{chat}/snapshot"))).await;
    assert_eq!(snap["history"]["orphaned_questions"][0]["id"], 903);
    let dismiss = |cli: &Client| {
        cli.post(format!("{chat}/requests/dismiss"))
            .json(&json!({"cursor": dismiss_req["cursor"], "id": 903}))
    };
    let (d1, d2) = tokio::join!(dismiss(&a).send(), dismiss(&b).send());
    let d_statuses = [d1.unwrap().status(), d2.unwrap().status()];
    assert_eq!(
        d_statuses
            .iter()
            .filter(|s| **s == reqwest::StatusCode::NO_CONTENT)
            .count(),
        1
    );
    assert_eq!(
        d_statuses
            .iter()
            .filter(|s| **s == reqwest::StatusCode::BAD_REQUEST)
            .count(),
        1
    );
    assert!(
        ok(b.get(format!("{chat}/snapshot"))).await["history"]["orphaned_questions"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );

    // 3. Secret answer delivery failure: runtime disconnects before confirming response.
    // Durable Item::RequestError persists across restart without leaking the secret value.
    let secret_turn = Uuid::new_v4();
    let _secret_send = tokio::spawn(
        a.post(format!("{chat}/messages"))
            .json(&json!({"id":secret_turn,"text":"ask secret and crash"}))
            .send(),
    );
    let secret_req = pending(&b, &chat).await;
    assert_eq!(secret_req["id"], 904);
    let secret_value = "my-ultra-secret-token-9999";
    let _ = a
        .post(format!("{chat}/requests/respond"))
        .json(&json!({
            "cursor": secret_req["cursor"],
            "id": 904,
            "action": "question",
            "reply": {"action": "submit", "values": {"api_secret": secret_value}}
        }))
        .send()
        .await
        .unwrap();

    let check_error_turn = |snap: &Value| {
        let raw = serde_json::to_string(snap).unwrap();
        assert!(
            !raw.contains(secret_value),
            "secret leaked into chat snapshot"
        );
        assert!(
            snap["history"]["orphaned_questions"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "secret question must never become an orphaned question"
        );
        let turn = snap["history"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == secret_turn.to_string())
            .unwrap();
        assert!(
            turn["items"].as_array().unwrap().iter().any(|item| {
                item["kind"] == "request_error"
                    && item["error"]["request_id"] == 904
                    && item["error"]["method"] == "item/tool/requestUserInput"
                    && matches!(
                        item["error"]["status"].as_str(),
                        Some("failed" | "unconfirmed")
                    )
            }),
            "expected durable Item::RequestError in turn items: {turn}"
        );
    };

    loop {
        let snap = ok(b.get(format!("{chat}/snapshot"))).await;
        let settled = snap["history"]["turns"].as_array().is_some_and(|turns| {
            turns
                .iter()
                .find(|t| t["id"] == secret_turn.to_string())
                .is_some_and(|t| t["status"] != "running")
        });
        if settled {
            check_error_turn(&snap);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    server.kill().await.unwrap();
    let (mut server, url) = serve_with_env(&state, &work, &native, &env).await;
    let chat = format!("{url}{path}");
    let after_restart = ok(a.get(format!("{chat}/snapshot"))).await;
    check_error_turn(&after_restart);

    let karchat_raw = fs::read_to_string(state.join(format!("{id}.karchat"))).unwrap();
    assert!(
        !karchat_raw.contains(secret_value),
        "secret leaked into durable .karchat file"
    );
    server.kill().await.unwrap();
}
