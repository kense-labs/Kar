//! Scheduled for the unified macOS run. Native model turns here use the stdio protocol fixture.
#[path = "support/codex_chat_peer.rs"]
mod codex_chat_peer;
#[path = "support/workbench_guest_recovery.rs"]
mod guest_cases;
#[path = "support/workbench_host_storage.rs"]
mod host_storage;
#[path = "support/workbench_server.rs"]
mod server;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use server::{json as ok, serve};
use std::{fs, time::Duration};
use uuid::Uuid;

async fn code(req: reqwest::RequestBuilder) -> StatusCode {
    req.send().await.unwrap().status()
}
async fn share(host: &Client, base: &str, p: &str, chat: Uuid, ttl: u32) -> Value {
    ok(host
        .post(format!("{base}/projects/{p}/chats/{chat}/shares"))
        .json(&json!({"ttl_seconds":ttl})))
    .await
}
async fn guest(public: &Client, base: &str, share: &Value, name: &str) -> String {
    let id = share["id"].as_str().unwrap();
    ok(public
        .post(format!("{base}/guest/{id}/redeem"))
        .json(&json!({"credential":share["credential"],"name":name})))
    .await["token"]
        .as_str()
        .unwrap()
        .to_owned()
}
async fn scenario() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    let state = dir.path().join("state");
    let native = dir.path().join("native");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&native).unwrap();
    let (mut server, base) = serve(&state, &root, &native).await;
    let host = server::client();
    let remote = Client::new();
    let project = ok(host
        .post(format!("{base}/projects"))
        .json(&json!({"root":root,"name":"local"})))
    .await;
    let p = project["id"].as_str().unwrap();
    let grant = ok(host.post(format!("{base}/access/pairings"))).await;
    let request = json!({"credential":grant["credential"],"label":"second Mac"});
    let device = ok(remote.post(format!("{base}/access/pair")).json(&request)).await;
    let token = device["token"].as_str().unwrap();
    assert_eq!(
        code(remote.post(format!("{base}/access/pair")).json(&request)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        code(
            remote
                .post(format!("{base}/access/pairings"))
                .bearer_auth(token)
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ok(remote.get(format!("{base}/projects")).bearer_auth(token)).await[0]["id"],
        p
    );
    for surface in ["terminals", "previews"] {
        let url = format!("{base}/projects/{p}/{surface}");
        assert_eq!(code(remote.get(&url)).await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            code(remote.get(&url).bearer_auth(token)).await,
            StatusCode::OK
        );
    }
    let mut chats = Vec::new();
    for _ in 0..2 {
        let id = Uuid::new_v4();
        ok(remote
            .post(format!("{base}/projects/{p}/chats"))
            .bearer_auth(token)
            .json(&json!({"id":id,"runtime":"codex"})))
        .await;
        chats.push(id);
    }
    let info = share(&host, &base, p, chats[0], 300).await;
    let other = share(&host, &base, p, chats[1], 300).await;
    let who = guest(&remote, &base, &info, "Visitor").await;
    let second = guest(&remote, &base, &info, "Other visitor").await;
    let id = info["id"].as_str().unwrap();
    let guest_root = format!("{base}/guest/{id}");
    assert_eq!(
        code(remote.get(format!("{base}/projects")).bearer_auth(&who)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        code(
            remote
                .get(format!(
                    "{base}/guest/{}/snapshot",
                    other["id"].as_str().unwrap()
                ))
                .bearer_auth(&who)
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let draft = json!({"request_id":Uuid::new_v4(),"text":"approved public message","attachments":[{"name":"notes.txt","base64":"aGVsbG8="}]});
    let msg = ok(remote
        .post(format!("{guest_root}/proposals"))
        .bearer_auth(&who)
        .json(&draft))
    .await;
    assert_eq!(
        ok(remote
            .post(format!("{guest_root}/proposals"))
            .bearer_auth(&who)
            .json(&draft))
        .await,
        msg
    );
    let file = format!(
        "{guest_root}/attachments/{}",
        msg["attachments"][0]["id"].as_str().unwrap()
    );
    assert_eq!(
        code(remote.get(&file).bearer_auth(&second)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        &remote
            .get(&file)
            .bearer_auth(&who)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()[..],
        b"hello"
    );
    let excess = json!({"request_id":Uuid::new_v4(),"text":"too many","attachments":vec![json!({"name":"a","base64":"YQ=="});5]});
    assert_eq!(
        code(
            remote
                .post(format!("{guest_root}/proposals"))
                .bearer_auth(&who)
                .json(&excess)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let dismiss = ok(remote
        .post(format!("{guest_root}/proposals"))
        .bearer_auth(&second)
        .json(&json!({"request_id":Uuid::new_v4(),"text":"never send this suggestion"})))
    .await;
    let dismissed = ok(host
        .put(format!(
            "{base}/access/shares/{id}/proposals/{}",
            dismiss["id"].as_str().unwrap()
        ))
        .json(&json!({"accept":false})))
    .await;
    assert_eq!(dismissed["status"], "dismissed");
    let oversized = json!({"request_id":Uuid::new_v4(),"text":"","attachments":[{"name":"big","base64":"AAAA".repeat((10*1024*1024)/3+1)}]});
    assert_eq!(
        code(
            remote
                .post(format!("{guest_root}/proposals"))
                .bearer_auth(&who)
                .json(&oversized)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let snap = ok(remote
        .get(format!("{guest_root}/snapshot"))
        .bearer_auth(&who))
    .await;
    assert_eq!(snap["turns"], json!([]));
    assert!(snap.get("workspace").is_none());
    server.kill().await.unwrap();
    let (mut server, base) = serve(&state, &root, &native).await;
    assert_eq!(
        ok(remote.get(format!("{base}/projects")).bearer_auth(token)).await[0]["id"],
        p
    );
    let route = format!(
        "{base}/access/shares/{id}/proposals/{}",
        msg["id"].as_str().unwrap()
    );
    let accepted = ok(host.put(&route).json(&json!({"accept":true}))).await;
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(
        ok(host.put(&route).json(&json!({"accept":true}))).await,
        accepted
    );
    let snapshot = ok(remote
        .get(format!("{base}/guest/{id}/snapshot"))
        .bearer_auth(&who))
    .await;
    assert_eq!(snapshot["turns"].as_array().unwrap().len(), 1);
    let first = &snapshot["turns"][0];
    assert_eq!(first["id"], msg["id"]);
    assert!(first["messages"].as_array().unwrap().iter().any(|m| {
        m["role"] == "user"
            && m["text"]
                .as_str()
                .unwrap()
                .contains("approved public message")
    }));
    // A picture-only proposal stays an image on the native wire across a repeated decision.
    use base64::{Engine, engine::general_purpose::STANDARD};
    let png = STANDARD.encode(include_bytes!("../crates/kar-documents/tests/pixel.png"));
    let picture = json!({"request_id":Uuid::new_v4(),"text":"","attachments":[{"name":"screen.png","content_type":"image/png","base64":png}]});
    let add = format!("{base}/guest/{id}/proposals");
    let proposal = ok(remote.post(&add).bearer_auth(&who).json(&picture)).await;
    let mut as_file = picture.clone();
    as_file["attachments"][0]["content_type"] = json!("application/octet-stream");
    assert_eq!(
        code(remote.post(&add).bearer_auth(&who).json(&as_file)).await,
        StatusCode::CONFLICT
    );
    let decide = format!(
        "{base}/access/shares/{id}/proposals/{}",
        proposal["id"].as_str().unwrap()
    );
    let accepted = ok(host.put(&decide).json(&json!({"accept":true}))).await;
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(
        ok(host.put(&decide).json(&json!({"accept":true}))).await,
        accepted
    );
    let native_inputs: Vec<Value> = fs::read_to_string(native.join("calls"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .filter(|row| row["input"] == proposal["id"])
        .collect();
    assert_eq!(native_inputs.len(), 1);
    assert_eq!(native_inputs[0]["text"], "");
    assert_eq!(
        native_inputs[0]["images"],
        json!([format!("data:image/png;base64,{png}")])
    );
    let mut live = remote
        .get(format!("{base}/guest/{id}/events"))
        .bearer_auth(&who)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(live.chunk().await.unwrap().is_some());
    ok(host.delete(format!("{base}/access/shares/{id}"))).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while live.chunk().await.unwrap().is_some() {}
    })
    .await
    .unwrap();
    assert_eq!(
        code(
            remote
                .get(format!("{base}/guest/{id}/snapshot"))
                .bearer_auth(&who)
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let short = share(&host, &base, p, chats[1], 1).await;
    let short_guest = guest(&remote, &base, &short, "short").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        code(
            remote
                .get(format!(
                    "{base}/guest/{}/snapshot",
                    short["id"].as_str().unwrap()
                ))
                .bearer_auth(short_guest)
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let mut device_feed = remote
        .get(format!("{base}/projects/{p}/chats/{}/events", chats[0]))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    ok(host.delete(format!(
        "{base}/access/devices/{}",
        device["id"].as_str().unwrap()
    )))
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while device_feed.chunk().await.unwrap().is_some() {}
    })
    .await
    .unwrap();
    server.kill().await.unwrap();
    let (mut server, base) = serve(&state, &root, &native).await;
    for path in [
        "/projects".to_owned(),
        format!("/projects/{p}/chats/{}/snapshot", chats[0]),
        format!("/projects/{p}/terminals"),
        format!("/projects/{p}/previews"),
    ] {
        assert_eq!(
            code(remote.get(format!("{base}{path}")).bearer_auth(token)).await,
            StatusCode::UNAUTHORIZED
        );
    }
    let registry = fs::read_to_string(state.join("workbench/access.json")).unwrap();
    assert!(
        !registry.contains(token)
            && !registry.contains(&who)
            && !registry.contains(grant["credential"].as_str().unwrap())
    );
    server.kill().await.unwrap();
}
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("app-server") => codex_chat_peer::agent(),
        Some("--version") => println!("codex 0.169.0"),
        Some("host-storage") => tokio::runtime::Runtime::new().unwrap().block_on(host_storage::exercise()),
        _ => tokio::runtime::Runtime::new().unwrap().block_on(async {
            host_storage::exercise().await;
            tokio::time::timeout(Duration::from_secs(90), scenario())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(200), guest_cases::recovery())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(90), guest_cases::legacy_registry())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(200), guest_cases::retention())
                .await
                .unwrap();
        }),
    }
}
