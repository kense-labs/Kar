use super::workbench_server::{client, json as ok, serve};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use uuid::Uuid;

struct Feed {
    response: reqwest::Response,
    bytes: Vec<u8>,
}
impl Feed {
    async fn open(client: &reqwest::Client, chat: &str) -> Self {
        Self {
            response: client
                .get(format!("{chat}/prompt-suggestion"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap(),
            bytes: vec![],
        }
    }
    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Some(end) = self.bytes.windows(2).position(|s| s == b"\n\n") {
                    let block = String::from_utf8(self.bytes.drain(..end + 2).collect()).unwrap();
                    if let Some(raw) = block.lines().find_map(|s| s.strip_prefix("data: ")) {
                        return serde_json::from_str(raw).unwrap();
                    }
                } else {
                    self.bytes
                        .extend(self.response.chunk().await.unwrap().unwrap());
                }
            }
        })
        .await
        .unwrap()
    }
}
async fn send(client: &reqwest::Client, chat: &str, n: usize) -> Uuid {
    let id = Uuid::new_v4();
    let out = ok(client
        .post(format!("{chat}/messages"))
        .json(&json!({"id":id,"text":format!("suggestion case {n}")})))
    .await;
    assert_eq!(out["status"], "completed");
    id
}
async fn samples(dir: &Path, expected: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while fs::read_to_string(dir.join("samples"))
            .unwrap_or_default()
            .lines()
            .count()
            != expected
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
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
    fs::create_dir(&work).unwrap();
    fs::create_dir(&native).unwrap();
    let a = client();
    let b = client();
    let (mut server, url) = serve(&state, &work, &native).await;
    let p = ok(a
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"suggestions"})))
    .await;
    let path = format!("/projects/{}/chats", p["id"].as_str().unwrap());
    let id = Uuid::new_v4();
    ok(a.post(format!("{url}{path}"))
        .json(&json!({"id":id,"runtime":"codex"})))
    .await;
    let chat = format!("{url}{path}/{id}");
    let setting = format!("{url}/settings/prompt-suggestions");
    let mut left = Feed::open(&a, &chat).await;
    let mut right = Feed::open(&b, &chat).await;
    assert_eq!(left.next().await, Value::Null);
    assert_eq!(right.next().await, Value::Null);
    assert_eq!(ok(a.get(&setting)).await, json!({"enabled":false}));
    send(&a, &chat, 0).await;
    assert!(!native.join("samples").exists());
    ok(b.put(&setting).json(&json!({"enabled":true}))).await;
    fs::write(native.join("sample-mode"), "first").unwrap();
    let one = send(&a, &chat, 1).await;
    let expected = json!({"turn_id":one,"text":"suggested first"});
    assert_eq!(left.next().await, expected);
    assert_eq!(right.next().await, expected);
    assert_eq!(Feed::open(&b, &chat).await.next().await, expected);

    fs::write(native.join("sample-mode"), "late").unwrap();
    send(&b, &chat, 2).await;
    samples(&native, 2).await;
    assert_eq!(left.next().await, Value::Null);
    assert_eq!(right.next().await, Value::Null);
    for (n, mode) in [(3, "wrong"), (4, "unsupported"), (5, "last")] {
        fs::write(native.join("sample-mode"), mode).unwrap();
        let turn = send(&a, &chat, n).await;
        samples(&native, n).await;
        if mode == "last" {
            let expected = json!({"turn_id":turn,"text":"suggested last"});
            assert_eq!(left.next().await, expected);
            assert_eq!(right.next().await, expected);
        }
    }
    let snapshot = ok(b.get(format!("{chat}/snapshot"))).await;
    assert!(!snapshot.to_string().contains("suggested last"));
    ok(a.put(&setting).json(&json!({"enabled":false}))).await;
    assert_eq!(right.next().await, Value::Null);
    ok(a.put(&setting).json(&json!({"enabled":true}))).await;
    let last = send(&a, &chat, 6).await;
    assert_eq!(
        right.next().await,
        json!({"turn_id":last,"text":"suggested last"})
    );
    drop((left, right));
    server.kill().await.unwrap();
    let (mut server, url) = serve(&state, &work, &native).await;
    assert_eq!(
        ok(b.get(format!("{url}/settings/prompt-suggestions"))).await,
        json!({"enabled":true})
    );
    assert_eq!(
        Feed::open(&a, &format!("{url}{path}/{id}"))
            .await
            .next()
            .await,
        Value::Null
    );
    server.kill().await.unwrap();
}
