//! Scheduled for the combined workbench acceptance run. The current test binary supplies Codex stdio.
use kar::chat::{ChatFeedSnapshot, ChatReplica, HttpChatFeed};
use reqwest::{Client, StatusCode, header};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use uuid::Uuid;

async fn serve(state: &Path, projects: &Path, native: &Path) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["workbench", "--state"])
        .arg(state)
        .arg("--root")
        .arg(projects)
        .env("KAR_CODEX_BIN", std::env::current_exe().unwrap())
        .env("KAR_WORKBENCH_TOKEN", "kar-workbench-fixture")
        .env("FIXTURE_NATIVE_DB", native)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let line = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    (child, ready["url"].as_str().unwrap().to_owned())
}

fn client() -> Client {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer kar-workbench-fixture".parse().unwrap(),
    );
    Client::builder().default_headers(headers).build().unwrap()
}

async fn json(req: reqwest::RequestBuilder) -> Value {
    let r = req.send().await.unwrap().error_for_status().unwrap();
    r.json().await.unwrap()
}

#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(120), scenario())
        .await
        .unwrap();
}

async fn scenario() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let root = dir.path().join("workspaces");
    let native = dir.path().join("native");
    std::fs::create_dir_all(&native).unwrap();
    let (a, b) = (client(), client());
    std::fs::create_dir_all(&root).unwrap();
    let (mut server, base) = serve(&state, &root, &native).await;
    let result = Client::new()
        .get(format!("{base}/projects"))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::UNAUTHORIZED);
    let mut projects = vec![];
    let mut chats = vec![];
    for name in ["alpha", "beta"] {
        let path = root.join(name);
        std::fs::create_dir(&path).unwrap();
        let registered = json(
            a.post(format!("{base}/projects"))
                .json(&json!({"name":name,"root":path})),
        )
        .await;
        let project = registered["id"].as_str().unwrap().to_string();
        let scope = format!("{base}/projects/{project}");
        let file_url = format!("{scope}/file?path=notes%2Fsame.txt");
        json(
            a.put(&file_url)
                .json(&json!({"text":format!("only-{name}\ncommon-marker")})),
        )
        .await;
        let text = json(b.get(&file_url)).await;
        assert_eq!(text["text"], format!("only-{name}\ncommon-marker"));
        for (kind, needle) in [("paths", "same"), ("content", "common-marker")] {
            let hits = json(
                b.get(format!("{scope}/search"))
                    .query(&[("kind", kind), ("q", needle)]),
            )
            .await;
            assert_eq!(hits.as_array().unwrap().len(), 1);
            assert_eq!(hits[0]["path"], "notes/same.txt");
            assert_eq!(hits[0]["project"], project);
        }
        let escape = b
            .get(format!("{scope}/file"))
            .query(&[("path", "../beta/notes/same.txt")])
            .send()
            .await
            .unwrap();
        assert_eq!(escape.status(), StatusCode::FORBIDDEN);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&native, path.join("outside")).unwrap();
            let result = a
                .put(format!("{scope}/file"))
                .query(&[("path", "outside/new.txt")])
                .json(&json!({"text":"wrong project"}))
                .send()
                .await
                .unwrap();
            assert_eq!(result.status(), StatusCode::FORBIDDEN);
            assert!(!native.join("new.txt").exists());
        }
        for _ in 0..2 {
            let id = Uuid::new_v4();
            let create = json!({"id":id,"runtime":"codex"});
            let entry = json(a.post(format!("{scope}/chats")).json(&create)).await;
            let retried = json(b.post(format!("{scope}/chats")).json(&create)).await;
            assert_eq!(entry, retried);
            assert_eq!(entry["info"]["id"], id.to_string());
            chats.push(id);
        }
        assert_eq!(
            json(b.get(format!("{scope}/chats")))
                .await
                .as_array()
                .unwrap()
                .len(),
            2
        );
        projects.push(project);
    }
    let chat = chats[0];
    let path = format!("/projects/{}/chats/{chat}", projects[0]);
    let mut first = HttpChatFeed::with_client(&format!("{base}{path}"), a.clone()).unwrap();
    let start = first.snapshot().await.unwrap();
    let second = HttpChatFeed::with_client(&format!("{base}{path}"), b.clone()).unwrap();
    let mut left = ChatReplica::from_snapshot(start);
    let mut right = ChatReplica::from_snapshot(second.snapshot().await.unwrap());
    let id = Uuid::new_v4();
    let message = json!({"id":id,"text":"project-message-marker"});
    let turn = json(a.post(format!("{base}{path}/messages")).json(&message)).await;
    assert_eq!(turn["id"], id.to_string());
    let committed = first.snapshot().await.unwrap().cursor;
    while left.cursor() != committed {
        left.apply(first.recv(left.cursor()).await.unwrap())
            .unwrap();
    }
    let other = format!("{base}/projects/{}/chats/{chat}/snapshot", projects[1]);
    assert_eq!(
        b.get(other).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    drop(first);
    server.kill().await.unwrap();
    let (mut restarted, base) = serve(&state, &root, &native).await;
    let same = json(b.post(format!("{base}{path}/messages")).json(&message)).await;
    assert_eq!(same, turn);
    let mut after = HttpChatFeed::with_client(&format!("{base}{path}"), b.clone()).unwrap();
    let snapshot: ChatFeedSnapshot = after.snapshot().await.unwrap();
    for replica in [&mut left, &mut right] {
        while replica.cursor() != snapshot.cursor {
            replica
                .apply(after.recv(replica.cursor()).await.unwrap())
                .unwrap();
        }
        assert_eq!(replica.turns(), snapshot.history.turns);
    }
    let hits = json(a.get(format!("{base}/search/chats?q=project-message-marker"))).await;
    assert!(
        hits.as_array()
            .unwrap()
            .iter()
            .any(|h| h["chat"] == chat.to_string() && h["turn"] == id.to_string())
    );
    let archived = json(
        b.put(format!("{base}{path}/archive"))
            .json(&json!({"archived":true})),
    )
    .await;
    assert_eq!(archived["archived"], true);
    restarted.kill().await.unwrap();
    let (mut restarted, base) = serve(&state, &root, &native).await;
    let entries = json(b.get(format!("{base}/projects/{}/chats", projects[0]))).await;
    assert_eq!(entries.as_array().unwrap().len(), 1);
    json(
        b.put(format!("{base}{path}/archive"))
            .json(&json!({"archived":false})),
    )
    .await;
    assert_eq!(
        json(a.get(format!("{base}/projects/{}/chats", projects[0])))
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    restarted.kill().await.unwrap();
}
