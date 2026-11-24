use super::workbench_server::{client, json as ok, serve};
use reqwest::{Client, Response};
use serde_json::{Value, json};
use std::{fs, time::Duration};
use uuid::Uuid;
struct Feed {
    rx: Response,
    buf: Vec<u8>,
}
impl Feed {
    async fn open(c: &Client, url: &str, after: Option<&str>) -> Self {
        let mut req = c.get(format!("{url}/shell/events"));
        if let Some(after) = after {
            req = req.header("last-event-id", after);
        }
        Self {
            rx: req.send().await.unwrap().error_for_status().unwrap(),
            buf: Vec::new(),
        }
    }
    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                while let Some(i) = self.buf.windows(2).position(|p| p == b"\n\n") {
                    let frame = String::from_utf8(self.buf.drain(..i + 2).collect()).unwrap();
                    if let Some(body) = frame.lines().find_map(|s| s.strip_prefix("data: ")) {
                        let v: Value = serde_json::from_str(body).unwrap();
                        assert_eq!(
                            v["cursor"].as_str(),
                            frame.lines().find_map(|s| s.strip_prefix("id: "))
                        );
                        return v;
                    }
                }
                self.buf
                    .extend_from_slice(&self.rx.chunk().await.unwrap().unwrap());
            }
        })
        .await
        .unwrap()
    }
    async fn matching(&mut self, id: &str, accept: impl Fn(&Value) -> bool) -> Value {
        loop {
            let f = self.next().await;
            if f["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == id && accept(&r["row"]))
            {
                return f;
            }
        }
    }
}
#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(90), scenario())
        .await
        .unwrap();
}
async fn scenario() {
    let t = tempfile::tempdir().unwrap();
    let work = t.path().join("root");
    let state = t.path().join("state");
    let db = t.path().join("native");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&db).unwrap();
    let (mut server, url) = serve(&state, &work, &db).await;
    let a = client();
    let b = client();
    let mut left = Feed::open(&a, &url, None).await;
    let mut right = Feed::open(&b, &url, None).await;
    let start = left.next().await;
    assert_eq!(start, right.next().await);
    assert_eq!(start["rows"], json!([]));
    let project = ok(a
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"shell"})))
    .await;
    let pid = project["id"].as_str().unwrap();
    let seen = left.matching(pid, |p| p["name"] == "shell").await;
    assert_eq!(seen, right.matching(pid, |p| p["name"] == "shell").await);
    let id = Uuid::new_v4().to_string();
    ok(b.post(format!("{url}/projects/{pid}/chats"))
        .json(&json!({"id":id,"runtime":"codex"})))
    .await;
    let created = left.matching(&id, |c| c["activity"]["turns"] == 0).await;
    assert_eq!(
        created,
        right.matching(&id, |c| c["activity"]["turns"] == 0).await
    );
    let chat = format!("{url}/projects/{pid}/chats/{id}");
    let turn = Uuid::new_v4();
    let sending = tokio::spawn(ok(b
        .post(format!("{chat}/messages"))
        .json(&json!({"id":turn,"text":"ask clients"}))));
    let pending = |row: &Value| {
        row["activity"]["pending_requests"]
            .as_array()
            .is_some_and(|r| r.len() == 1)
    };
    let waiting = left.matching(&id, pending).await;
    assert_eq!(waiting, right.matching(&id, pending).await);
    let later = jiff::Timestamp::now().as_millisecond() + 60_000;
    ok(a.post(format!("{chat}/metadata"))
        .json(&json!({"command_id":Uuid::new_v4(),"action":"snooze","until_ms":later})))
    .await;
    let sleeping = left
        .matching(&id, |row| row["snoozed"] == true && pending(row))
        .await;
    assert_eq!(
        sleeping,
        right
            .matching(&id, |row| row["snoozed"] == true && pending(row))
            .await
    );
    let request = &ok(a.get(format!("{chat}/snapshot"))).await["history"]["pending_requests"][0];
    ok(a.post(format!("{chat}/requests/respond")).json(&json!({"id":request["id"],"cursor":request["cursor"],"action":"question","reply":{"action":"submit","values":{"q":"ready"}}}))).await;
    assert_eq!(sending.await.unwrap()["status"], "completed");
    let completed = |r: &Value| r["activity"]["latest_turn"]["status"] == "completed";
    let finished = left.matching(&id, completed).await;
    assert_eq!(finished, right.matching(&id, completed).await);
    let next_turn = Uuid::new_v4();
    ok(a.post(format!("{chat}/messages"))
        .json(&json!({"id":next_turn,"text":"wake from snooze"})))
    .await;
    let cleared = |row: &Value| {
        row["metadata"]["snoozed_ms"].is_null()
            && row["activity"]["latest_turn"]["id"] == json!(next_turn)
    };
    let waking_input = left.matching(&id, cleared).await;
    assert_eq!(waking_input, right.matching(&id, cleared).await);
    assert!(ok(a.get(format!("{chat}/metadata"))).await["snoozed_until_ms"].is_null());
    let until = jiff::Timestamp::now().as_millisecond() + 1500;
    ok(a.post(format!("{chat}/metadata"))
        .json(&json!({"command_id":Uuid::new_v4(),"action":"snooze","until_ms":until})))
    .await;
    let snoozed = left.matching(&id, |c| c["snoozed"] == true).await;
    assert_eq!(snoozed, right.matching(&id, |c| c["snoozed"] == true).await);
    let awake = left.matching(&id, |c| c["snoozed"] == false).await;
    assert_eq!(awake, right.matching(&id, |c| c["snoozed"] == false).await);
    drop((right, left));
    ok(a.put(format!("{chat}/archive"))
        .json(&json!({"archived":true})))
    .await;
    // A damaged header in a different CLI chat cannot stop the directory stream.
    let damaged = state.join(format!("{}.karchat", Uuid::new_v4()));
    fs::write(&damaged, b"{").unwrap();
    let snap = ok(a.get(format!("{url}/shell"))).await;
    fs::remove_file(damaged).unwrap();
    server.kill().await.unwrap();
    let (mut next, url) = serve(&state, &work, &db).await;
    let mut resumed = Feed::open(&b, &url, awake["cursor"].as_str()).await;
    let archived = resumed.matching(&id, |c| c["archived"] == true).await;
    assert_eq!(archived["cursor"], snap["cursor"]);
    let stable = ok(b.get(format!("{url}/shell"))).await;
    assert_eq!(stable, snap);
    let bogus = format!("{}:1", Uuid::new_v4());
    assert!(
        !a.get(format!("{url}/shell/events"))
            .header("last-event-id", bogus)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    next.kill().await.unwrap();
}
