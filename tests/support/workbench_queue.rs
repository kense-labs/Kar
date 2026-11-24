//! A restarted workbench must drain the original durable queue before any new user message.
use super::workbench_server::{client, json as ok, serve};
use kar::chat::ChatStore;
use serde_json::{Value, json};
use std::{fs, time::Duration};
use uuid::Uuid;

#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(90), scenario())
        .await
        .unwrap()
}
async fn scenario() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let native = tmp.path().join("native");
    let state = tmp.path().join("state");
    for dir in [&work, &native] {
        fs::create_dir(dir).unwrap()
    }
    let (mut server, url) = serve(&state, &work, &native).await;
    let client = client();
    let p = ok(client
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"queue"})))
    .await;
    let p = p["id"].as_str().unwrap();
    let chat = Uuid::new_v4();
    let binding = ok(client
        .post(format!("{url}/projects/{p}/chats"))
        .json(&json!({"id":chat,"runtime":"codex"})))
    .await;
    let native_id = binding["info"]["native_id"].as_str().unwrap();
    let path = format!("/projects/{p}/chats/{chat}");
    let first = Uuid::new_v4();
    ok(client
        .post(format!("{url}{path}/queue"))
        .json(&json!({"id":first,"text":"delivered before restart"})))
    .await;
    let wait = |url: String, client: reqwest::Client, count: usize| {
        let path = path.clone();
        async move {
            loop {
                let q = ok(client.get(format!("{url}{path}/queue"))).await;
                let snap = ok(client.get(format!("{url}{path}/snapshot"))).await;
                assert_eq!(q["delivery_error"], Value::Null);
                if q["inputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|v| v["status"] == "sent")
                    .count()
                    == count
                    && snap["history"]["turns"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|t| t["status"] == "completed")
                        .count()
                        == count
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }
    };
    wait(url, client.clone(), 1).await;
    server.kill().await.unwrap();
    let offline = ChatStore::open(&state).unwrap();
    offline
        .enqueue(chat, first, "delivered before restart")
        .unwrap();
    let next = Uuid::new_v4();
    let last = Uuid::new_v4();
    offline.enqueue(chat, next, "first pending").unwrap();
    offline.enqueue(chat, last, "second pending").unwrap();
    let withdrawn = Uuid::new_v4();
    offline
        .enqueue(chat, withdrawn, "withdrawn offline")
        .unwrap();
    offline.withdraw(chat, withdrawn).unwrap();
    let (mut server, url) = serve(&state, &work, &native).await;
    wait(url.clone(), client.clone(), 3).await;
    let retry = ok(client
        .post(format!("{url}{path}/queue"))
        .json(&json!({"id":next,"text":"first pending"})))
    .await;
    assert_eq!(retry["status"], "sent");
    let calls: Vec<Value> = fs::read_to_string(native.join("calls"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let prompts: Vec<_> = calls.iter().map(|c| c["text"].as_str().unwrap()).collect();
    assert_eq!(
        prompts,
        [
            "delivered before restart",
            "first pending",
            "second pending"
        ]
    );
    assert!(calls.iter().all(|c| c["id"] == native_id
        && c["cwd"].as_str().map(std::path::Path::new)
            == Some(work.canonicalize().unwrap().as_path())));
    server.kill().await.unwrap();
}
