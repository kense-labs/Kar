use super::workbench_server::{client, json as ok, serve};
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
    store
        .persist_attachment(
            id,
            attachment,
            "reference.txt",
            "text/plain",
            kar::chat::AttachmentKind::File,
            b"client evidence",
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
    assert_eq!(refs[0]["name"], "reference.txt");
    assert_eq!(
        fs::read(refs[0]["path"].as_str().unwrap()).unwrap(),
        b"client evidence"
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
}
