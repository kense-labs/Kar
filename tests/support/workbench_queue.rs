//! A restarted workbench must drain the original durable queue before any new user message.
use super::workbench_server::{client, json as ok, serve};
use kar::chat::ChatStore;
use serde_json::{Value, json};
use std::{fs, time::Duration};
use uuid::Uuid;

const IMG: &[u8] = include_bytes!("../../crates/kar-documents/tests/pixel.png");

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
    let empty = Uuid::new_v4();
    offline.enqueue(chat, empty, "").unwrap();
    offline.enqueue(chat, next, "first pending").unwrap();
    offline.enqueue(chat, last, "second pending").unwrap();
    let withdrawn = Uuid::new_v4();
    offline
        .enqueue(chat, withdrawn, "withdrawn offline")
        .unwrap();
    offline.withdraw(chat, withdrawn).unwrap();
    let (mut server, url) = serve(&state, &work, &native).await;
    wait(url.clone(), client.clone(), 3).await;
    let invalid = ok(client
        .post(format!("{url}{path}/queue"))
        .json(&json!({"id":empty,"text":""})))
    .await;
    assert_eq!(invalid["status"], "refused");
    assert!(invalid["refusal"].as_str().unwrap().contains("empty input"));
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
    let endpoint = format!("{url}{path}/queue");
    let commands = format!("{endpoint}/commands");
    let hold = json!({"command_id":Uuid::new_v4(),"action":"hold"});
    assert_eq!(ok(client.post(&commands).json(&hold)).await["held"], true);
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for (id, text) in [(a, "original A"), (b, "B"), (c, "C")] {
        assert_eq!(
            ok(client.post(&endpoint).json(&json!({"id":id,"text":text}))).await["status"],
            "pending"
        );
    }
    let ticket = ok(client
        .post(format!("{url}/projects/{p}/attachments/uploads"))
        .json(&json!({"name":"review.png","mime":"image/png","kind":"image","bytes":IMG.len()})))
    .await;
    let picture = ticket["id"].as_str().unwrap();
    let public = reqwest::Client::new();
    let upload = format!("{url}{}", ticket["upload_path"].as_str().unwrap());
    assert!(
        !public
            .put(&upload)
            .body("bad")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    ok(public.put(&upload).body(IMG)).await;
    ok(public.put(&upload).body(IMG)).await;
    let saved = ok(client
        .post(format!("{url}{path}/attachments"))
        .json(&json!({"attachment":picture})))
    .await;
    assert_eq!(saved["bytes"], IMG.len());
    assert!(
        !public
            .put(&upload)
            .body(IMG)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let read = ok(client.post(format!("{url}{path}/attachments/{picture}/url"))).await;
    let read_path = read["read_path"].as_str().unwrap();
    assert_eq!(
        public
            .get(format!("{url}{read_path}"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        IMG
    );
    let edited = json!({"command_id":Uuid::new_v4(),"action":"replace","input":a,"text":"edited A","attachments":[picture]});
    let moved = json!({"command_id":Uuid::new_v4(),"action":"reorder","input":c,"before":a});
    let receipt = ok(client.post(&commands).json(&edited)).await;
    ok(client.post(&commands).json(&moved)).await;
    server.kill().await.unwrap();
    let (mut server, url) = serve(&state, &work, &native).await;
    let endpoint = format!("{url}{path}/queue");
    let commands = format!("{endpoint}/commands");
    let list = ok(client.get(&endpoint)).await;
    assert_eq!(list["held"], true);
    let pending: Vec<_> = list["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["status"] == "pending")
        .map(|i| i["text"].as_str().unwrap())
        .collect();
    assert_eq!(pending, ["C", "edited A", "B"]);
    assert_eq!(ok(client.post(&commands).json(&edited)).await, receipt);
    let changed = json!({"command_id":edited["command_id"],"action":"edit","input":a,"text":"duplicate mutation"});
    assert!(
        !client
            .post(&commands)
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let first = json!({"command_id":Uuid::new_v4(),"action":"start","input":b});
    assert_eq!(
        ok(client.post(&commands).json(&first)).await["next"],
        b.to_string()
    );
    wait(url.clone(), client.clone(), 4).await;
    ok(client
        .post(&commands)
        .json(&json!({"command_id":Uuid::new_v4(),"action":"resume"})))
    .await;
    wait(url.clone(), client.clone(), 6).await;
    let duplicate = ok(client
        .post(&endpoint)
        .json(&json!({"id":a,"text":"original A"})))
    .await;
    assert_eq!(duplicate["status"], "sent");
    assert_eq!(duplicate["text"], "edited A");
    assert_eq!(ok(client.post(&commands).json(&edited)).await, receipt);
    let calls = fs::read_to_string(native.join("calls"))
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(&calls[3..], ["B", "C", "edited A"]);
    let timeline = ok(client.get(format!("{url}{path}/snapshot"))).await;
    let delivered = timeline["history"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == a.to_string())
        .unwrap();
    assert_eq!(delivered["attachments"], json!([saved]));
    assert_eq!(
        public
            .get(format!("{url}{read_path}"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        IMG
    );
    let target = Uuid::new_v4();
    let active = tokio::spawn(ok(client
        .post(format!("{url}{path}/messages"))
        .json(&json!({"id":target,"text":"await queue steer"}))));
    while !native.join("steer-ready").exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let follow = Uuid::new_v4();
    let body = json!({"id":follow,"text":"apply inside this turn","attachments":[picture]});
    ok(client.post(&endpoint).json(&body)).await;
    let promote = format!("{endpoint}/{follow}/steer");
    let action = json!({"command_id":Uuid::new_v4(),"turn":target});
    let bad = client
        .post(&promote)
        .json(&json!({"command_id":Uuid::new_v4(),"turn":Uuid::new_v4()}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::CONFLICT);
    let promoted = ok(client.post(&promote).json(&action)).await;
    assert!(
        promoted["promotion"]["native_acknowledged"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(active.await.unwrap()["status"], "completed");
    server.kill().await.unwrap();
    let (mut server, url) = serve(&state, &work, &native).await;
    let endpoint = format!("{url}{path}/queue");
    let replay = ok(client
        .post(format!("{endpoint}/{follow}/steer"))
        .json(&action))
    .await;
    assert_eq!(replay["status"], "sent");
    assert_eq!(
        ok(client.post(&endpoint).json(&body)).await["status"],
        "sent"
    );
    assert_eq!(
        fs::read_to_string(native.join("steered-text")).unwrap(),
        "apply inside this turn"
    );
    let calls = fs::read_to_string(native.join("calls")).unwrap();
    assert!(!calls.contains("apply inside this turn"));
    let timeline = ok(client.get(format!("{url}{path}/snapshot"))).await;
    let turn = timeline["history"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == target.to_string())
        .unwrap();
    assert_eq!(
        turn["steered_attachments"][follow.to_string()],
        json!([saved])
    );
    server.kill().await.unwrap();
}
