//! Real Kar HTTP/state/queue, local stdio peer. No remote model credentials are used.
use super::{code, guest, ok, server, share};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use uuid::Uuid;

async fn post(public: &Client, base: &str, s: &str, token: &str, body: &Value) -> Value {
    ok(public
        .post(format!("{base}/guest/{s}/proposals"))
        .bearer_auth(token)
        .json(body))
    .await
}

fn proposal(text: &str, data: Option<&str>) -> Value {
    let files: Vec<_> = data
        .into_iter()
        .map(|s| json!({"name":"file.txt","base64":STANDARD.encode(s)}))
        .collect();
    json!({"request_id":Uuid::new_v4(),"text":text,"attachments":files})
}

fn decide(
    host: &Client,
    base: &str,
    s: &str,
    msg: &Value,
    accept: bool,
) -> reqwest::RequestBuilder {
    host.put(format!(
        "{base}/access/shares/{s}/proposals/{}",
        msg["id"].as_str().unwrap()
    ))
    .json(&json!({"accept":accept}))
}

async fn eventually(mut check: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(45), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    })
    .await
    .expect("expected durable guest state");
}

async fn decided(
    host: &Client,
    base: &str,
    s: &str,
    msg: &Value,
    status: &str,
    delivery: &str,
) -> bool {
    let rows = ok(host.get(format!("{base}/access/shares/{s}/proposals"))).await;
    rows.as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == msg["id"] && p["status"] == status && p["delivery"] == delivery)
}

fn calls(native: &Path, text: &str) -> usize {
    fs::read_to_string(native.join("calls"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["text"].as_str().is_some_and(|s| {
                s == text
                    || s.strip_prefix(text).is_some_and(|v| {
                        v.starts_with("\n\nAttachment: ")
                            || v.starts_with("\n\nAttached files (JSON):")
                    })
            })
        })
        .count()
}

async fn setup(
    state: &Path,
    root: &Path,
    native: &Path,
) -> (tokio::process::Child, String, String, Uuid) {
    fs::create_dir_all(root).unwrap();
    fs::create_dir_all(native).unwrap();
    let (server, base) = server::serve(state, root, native).await;
    let host = server::client();
    let project = ok(host
        .post(format!("{base}/projects"))
        .json(&json!({"name":"Guest tests","root":root})))
    .await;
    let p = project["id"].as_str().unwrap().to_owned();
    let chat = Uuid::new_v4();
    ok(host
        .post(format!("{base}/projects/{p}/chats"))
        .json(&json!({"id":chat,"runtime":"codex"})))
    .await;
    (server, base, p, chat)
}

pub async fn recovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let native = dir.path().join("native");
    let state = dir.path().join("state");
    let (mut server, base, project, chat) = setup(&state, &root, &native).await;
    let host = server::client();
    let public = Client::new();
    let share = share(&host, &base, &project, chat, 600).await;
    let s = share["id"].as_str().unwrap();
    let token = guest(&public, &base, &share, "Guest").await;
    let first_body = proposal("recover before enqueue", Some("first bytes"));
    let first = post(&public, &base, s, &token, &first_body).await;
    // The owner decision is durable before the workspace file can be installed.
    fs::write(root.join(".kar-attachments"), b"occupied").unwrap();
    assert!(
        !code(decide(&host, &base, s, &first, true))
            .await
            .is_success()
    );
    assert_eq!(calls(&native, "recover before enqueue"), 0);
    let queue = ok(host.get(format!("{base}/projects/{project}/chats/{chat}/queue"))).await;
    assert_eq!(queue["inputs"], json!([]));
    let rows = ok(host.get(format!("{base}/access/shares/{s}/proposals"))).await;
    assert_eq!(rows[0]["status"], "sending");
    server.kill().await.unwrap();
    fs::remove_file(root.join(".kar-attachments")).unwrap();
    let (mut server, base) = server::serve(&state, &root, &native).await;
    eventually(async || decided(&host, &base, s, &first, "accepted", "sent").await).await;
    assert_eq!(calls(&native, "recover before enqueue"), 1);
    assert_eq!(
        post(&public, &base, s, &token, &first_body).await["id"],
        first["id"]
    );

    // Reserve the private accepted-file name, so chat delivery completes but the guest
    // registry cannot commit Accepted. Drop the original HTTP future after the native call.
    let body = proposal("recover sent receipt", Some("second bytes"));
    let second = post(&public, &base, s, &token, &body).await;
    let blob = state.join("workbench").join(format!(
        "guest-{}.attachment",
        second["attachments"][0]["id"].as_str().unwrap()
    ));
    fs::write(&blob, b"occupied").unwrap();
    let req = decide(&host, &base, s, &second, true);
    let http = tokio::spawn(async move { req.send().await });
    eventually(async || calls(&native, "recover sent receipt") == 1).await;
    http.abort();
    let queue_url = format!("{base}/projects/{project}/chats/{chat}/queue");
    eventually(async || {
        ok(host.get(&queue_url)).await["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| q["id"] == second["id"] && q["status"] == "sent")
    })
    .await;
    let rows = ok(host.get(format!("{base}/access/shares/{s}/proposals"))).await;
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == second["id"] && p["status"] == "sending")
    );
    server.kill().await.unwrap();
    fs::remove_file(blob).unwrap();
    let (mut server, base) = server::serve(&state, &root, &native).await;
    eventually(async || decided(&host, &base, s, &second, "accepted", "sent").await).await;
    assert_eq!(
        ok(decide(&host, &base, s, &second, true)).await["id"],
        second["id"]
    );
    assert_eq!(calls(&native, "recover sent receipt"), 1);

    // The peer received turn/start, then withheld its response and native history evidence.
    let held = post(
        &public,
        &base,
        s,
        &token,
        &proposal("hold without reply", None),
    )
    .await;
    let req = decide(&host, &base, s, &held, true);
    let http = tokio::spawn(async move { req.send().await });
    eventually(async || calls(&native, "hold without reply") == 1).await;
    http.abort();
    let waiting = post(
        &public,
        &base,
        s,
        &token,
        &proposal("queued while held", None),
    )
    .await;
    let req = decide(&host, &base, s, &waiting, true);
    let http = tokio::spawn(async move { req.send().await });
    eventually(async || {
        ok(host.get(format!("{base}/projects/{project}/chats/{chat}/queue"))).await["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == waiting["id"] && v["status"] == "pending")
    })
    .await;
    http.abort();
    server.kill().await.unwrap();
    let (mut server, base) = server::serve(&state, &root, &native).await;
    eventually(async || decided(&host, &base, s, &held, "sending", "uncertain").await).await;
    assert_eq!(calls(&native, "queued while held"), 0);
    ok(host
        .post(format!("{base}/projects/{project}/chats/{chat}/queue/commands"))
        .json(&json!({
            "command_id": Uuid::new_v4(),
            "action": "acknowledge_uncertain",
            "input": held["id"]
        })))
    .await;
    eventually(async || decided(&host, &base, s, &waiting, "accepted", "sent").await).await;
    let ambiguous = ok(decide(&host, &base, s, &held, true)).await;
    assert_eq!(ambiguous["delivery"], "uncertain");
    assert_eq!(calls(&native, "hold without reply"), 1);
    assert_eq!(calls(&native, "queued while held"), 1);
    let snapshot = ok(public
        .get(format!("{base}/guest/{s}/snapshot"))
        .bearer_auth(&token))
    .await;
    for msg in [&first, &second, &held, &waiting] {
        assert_eq!(
            snapshot["turns"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| t["id"] == msg["id"])
                .count(),
            1
        );
    }
    server.kill().await.unwrap();
}

pub async fn retention() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let state = dir.path().join("state");
    let native = dir.path().join("native");
    let (mut server, base, project, chat) = setup(&state, &root, &native).await;
    let host = server::client();
    let public = Client::new();
    let info = share(&host, &base, &project, chat, 600).await;
    let s = info["id"].as_str().unwrap();
    let who = guest(&public, &base, &info, "Guest").await;
    let submit_url = format!("{base}/guest/{s}/proposals");
    let draft =
        |files: Value| json!({"request_id":Uuid::new_v4(),"text":"limits","attachments":files});
    for (mime, encoded) in [
        ("text/plain", ""),
        ("image/svg+xml", "YQ=="),
        ("image/png", "YQ=="),
    ] {
        let body = draft(json!([{"name":"a","base64":encoded,"content_type":mime}]));
        assert_eq!(
            code(public.post(&submit_url).bearer_auth(&who).json(&body)).await,
            StatusCode::BAD_REQUEST
        );
    }
    let image = json!({"name":"a.png","content_type":"image/png","base64":STANDARD.encode(b"\x89PNG\r\n\x1a\n")});
    let cap = draft(json!(vec![image.clone(); 4]));
    let max_count = post(&public, &base, s, &who, &cap).await;
    assert_eq!(max_count["attachments"].as_array().unwrap().len(), 4);
    assert_eq!(
        code(
            public
                .post(&submit_url)
                .bearer_auth(&who)
                .json(&draft(json!(vec![image; 5])))
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    ok(decide(&host, &base, s, &max_count, false)).await;
    let full = STANDARD.encode(vec![23; 10 * 1024 * 1024]);
    let first = draft(json!([{"name":"full","base64":full}]));
    let second = draft(json!([{"name":"full","base64":full}]));
    let one = post(&public, &base, s, &who, &first).await;
    post(&public, &base, s, &who, &second).await;
    assert_eq!(
        code(
            public
                .post(&submit_url)
                .bearer_auth(&who)
                .json(&proposal("extra byte", Some("x")))
        )
        .await,
        StatusCode::CONFLICT
    );
    let three = post(&public, &base, s, &who, &proposal("third", None)).await;
    assert_eq!(
        code(
            public
                .post(&submit_url)
                .bearer_auth(&who)
                .json(&proposal("fourth", None))
        )
        .await,
        StatusCode::CONFLICT
    );
    assert_eq!(post(&public, &base, s, &who, &first).await, one);
    ok(decide(&host, &base, s, &three, false)).await;
    ok(host.delete(format!("{base}/access/shares/{s}"))).await;
    assert!(
        !fs::read_to_string(state.join("workbench/access.json"))
            .unwrap()
            .contains(&full[..128])
    );

    let info = share(&host, &base, &project, chat, 600).await;
    let s = info["id"].as_str().unwrap();
    let who = guest(&public, &base, &info, "Author").await;
    let peer = guest(&public, &base, &info, "Peer").await;
    let keep = proposal("accepted file", Some("original bytes"));
    let msg = post(&public, &base, s, &who, &keep).await;
    let file = msg["attachments"][0]["id"].as_str().unwrap();
    let url = format!("{base}/guest/{s}/attachments/{file}");
    assert_eq!(
        code(public.get(&url).bearer_auth(&peer)).await,
        StatusCode::NOT_FOUND
    );
    let ack = ok(decide(&host, &base, s, &msg, true)).await;
    assert_eq!(ack["status"], "accepted");
    let draft = proposal("dismissed file", Some("unpublished bytes"));
    let dropped = post(&public, &base, s, &who, &draft).await;
    let dropped_url = format!(
        "{base}/guest/{s}/attachments/{}",
        dropped["attachments"][0]["id"].as_str().unwrap()
    );
    ok(decide(&host, &base, s, &dropped, false)).await;
    assert_eq!(
        code(public.get(dropped_url).bearer_auth(&who)).await,
        StatusCode::NOT_FOUND
    );
    for i in 0..21 {
        let p = post(
            &public,
            &base,
            s,
            &peer,
            &proposal(&format!("unused {i}"), None),
        )
        .await;
        ok(decide(&host, &base, s, &p, false)).await;
    }
    let rows = ok(host.get(format!("{base}/access/shares/{s}/proposals"))).await;
    assert_eq!(rows.as_array().unwrap().len(), 20);
    assert!(
        !rows
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == msg["id"] || p["id"] == dropped["id"])
    );
    assert_eq!(post(&public, &base, s, &who, &keep).await, ack);
    assert_eq!(
        post(&public, &base, s, &who, &draft).await["status"],
        "dismissed"
    );
    assert_eq!(
        ok(decide(&host, &base, s, &msg, true)).await["status"],
        "accepted"
    );
    assert_eq!(calls(&native, "accepted file"), 1);
    let mut changed = keep.clone();
    changed["attachments"][0]["base64"] = json!(STANDARD.encode("different bytes"));
    assert_eq!(
        code(
            public
                .post(format!("{base}/guest/{s}/proposals"))
                .bearer_auth(&who)
                .json(&changed)
        )
        .await,
        StatusCode::CONFLICT
    );
    let registry = fs::read_to_string(state.join("workbench/access.json")).unwrap();
    assert!(!registry.contains(&STANDARD.encode("unpublished bytes")));
    let original = root
        .join(".kar-attachments")
        .join(msg["id"].as_str().unwrap())
        .join(format!("{file}-file.txt"));
    fs::write(original, b"locally edited").unwrap();
    let bytes = public
        .get(&url)
        .bearer_auth(&peer)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), b"original bytes");
    let next_chat = Uuid::new_v4();
    ok(host
        .post(format!("{base}/projects/{project}/chats"))
        .json(&json!({"id":next_chat,"runtime":"codex"})))
    .await;
    let other = share(&host, &base, &project, next_chat, 600).await;
    let token = guest(&public, &base, &other, "Other chat").await;
    let cross = format!(
        "{base}/guest/{}/attachments/{file}",
        other["id"].as_str().unwrap()
    );
    assert_eq!(
        code(public.get(&cross).bearer_auth(&token)).await,
        StatusCode::NOT_FOUND
    );
    let new = share(&host, &base, &project, chat, 3).await;
    let token = guest(&public, &base, &new, "New share").await;
    let snapshot = ok(public
        .get(format!(
            "{base}/guest/{}/snapshot",
            new["id"].as_str().unwrap()
        ))
        .bearer_auth(&token))
    .await;
    let original_turn = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == msg["id"])
        .unwrap();
    assert_eq!(original_turn["attachments"], msg["attachments"]);
    let url_new = format!(
        "{base}/guest/{}/attachments/{file}",
        new["id"].as_str().unwrap()
    );
    assert_eq!(
        code(public.get(&url_new).bearer_auth(&token)).await,
        StatusCode::OK
    );
    assert_eq!(
        code(public.get(&url).bearer_auth(&who)).await,
        StatusCode::UNAUTHORIZED
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        code(public.get(&url_new).bearer_auth(&token)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        code(public.get(&cross).bearer_auth(&token)).await,
        StatusCode::UNAUTHORIZED
    );
    server.kill().await.unwrap();
}

pub async fn legacy_registry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let state = dir.path().join("state");
    let native = dir.path().join("native");
    let (mut process, base, p, chat) = setup(&state, &root, &native).await;
    let host = server::client();
    let public = Client::new();
    let info = share(&host, &base, &p, chat, 600).await;
    let s = info["id"].as_str().unwrap();
    let who = guest(&public, &base, &info, "Visitor").await;
    let mut items = vec![];
    let path = format!("/projects/{p}/chats/{chat}/queue");
    for text in [
        "previously accepted",
        "received before guest ack",
        "queued before upgrade",
    ] {
        let draft = proposal(text, Some("old bytes"));
        let p = post(&public, &base, s, &who, &draft).await;
        let msg = p["id"].as_str().unwrap();
        let file = p["attachments"][0]["id"].as_str().unwrap();
        // Prior Kar accepted through /messages and retained base64 inline in access.json.
        let wire = format!("{text}\n\nAttachment: .kar-attachments/{msg}/{file}-file.txt");
        if text == "queued before upgrade" {
            ok(host
                .post(format!("{base}{path}/commands"))
                .json(&json!({"command_id":Uuid::new_v4(),"action":"hold"})))
            .await;
            let receipt = ok(host
                .post(format!("{base}{path}"))
                .json(&json!({"id":msg,"text":wire})))
            .await;
            assert_eq!(receipt["status"], "pending");
        } else {
            ok(host
                .post(format!(
                    "{base}/projects/{}/chats/{chat}/messages",
                    info["project"].as_str().unwrap()
                ))
                .json(&json!({"id":msg,"text":wire})))
            .await;
        }
        items.push((p, draft));
    }
    let queue = ok(host.get(format!("{base}{path}"))).await;
    assert_eq!(queue["inputs"].as_array().unwrap().len(), 1);
    process.kill().await.unwrap();
    let file = state.join("workbench/access.json");
    let mut db: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    // Construct only the prior persisted registry shape while the service is stopped.
    let share = db["shares"][s].as_object_mut().unwrap();
    share.remove("published");
    share.remove("retired");
    for (i, (msg, _)) in items.iter().enumerate() {
        let row = share["proposals"][msg["id"].as_str().unwrap()]
            .as_object_mut()
            .unwrap();
        for field in ["sequence", "signature", "delivery"] {
            row.remove(field);
        }
        for f in row["files"].as_array_mut().unwrap() {
            f.as_object_mut().unwrap().remove("mime");
        }
        row.insert(
            "status".into(),
            json!(if i == 0 { "accepted" } else { "sending" }),
        );
    }
    fs::write(&file, serde_json::to_vec(&db).unwrap()).unwrap();
    let (mut process, base) = server::serve(&state, &root, &native).await;
    eventually(async || decided(&host, &base, s, &items[1].0, "accepted", "sent").await).await;
    let before = ok(host.get(format!("{base}{path}"))).await;
    assert_eq!(
        before["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|q| q["id"] == items[2].0["id"] && q["status"] == "pending")
            .count(),
        1
    );
    // The owner can edit a queued input from an earlier version before letting it run.
    let edited = format!("{} (edited)", items[2].0["text"].as_str().unwrap());
    let msg = &items[2].0;
    let wire = format!(
        "{edited}\n\nAttachment: .kar-attachments/{}/{}-file.txt",
        msg["id"].as_str().unwrap(),
        msg["attachments"][0]["id"].as_str().unwrap()
    );
    ok(host
        .post(format!("{base}{path}/commands"))
        .json(&json!({"command_id":Uuid::new_v4(),"action":"edit","input":msg["id"],"text":wire})))
    .await;
    ok(host
        .post(format!("{base}{path}/commands"))
        .json(&json!({"command_id":Uuid::new_v4(),"action":"resume"})))
    .await;
    eventually(async || decided(&host, &base, s, &items[2].0, "accepted", "sent").await).await;
    for (msg, draft) in items {
        let id = msg["attachments"][0]["id"].as_str().unwrap();
        let bytes = public
            .get(format!("{base}/guest/{s}/attachments/{id}"))
            .bearer_auth(&who)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"old bytes");
        assert_eq!(post(&public, &base, s, &who, &draft).await["id"], msg["id"]);
        let original = msg["text"].as_str().unwrap();
        let text = if edited.starts_with(original) {
            &edited
        } else {
            original
        };
        assert_eq!(calls(&native, text), 1);
    }
    assert!(
        !fs::read_to_string(file)
            .unwrap()
            .contains(&STANDARD.encode("old bytes"))
    );
    process.kill().await.unwrap();
}
