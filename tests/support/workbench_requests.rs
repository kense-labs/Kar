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
    ok(answer(&a, &chat, &second, "second")).await;
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
    assert_eq!(
        receipts[1]["answer"]["answers"]["q"]["answers"][0],
        "second"
    );
    server.kill().await.unwrap();
}
