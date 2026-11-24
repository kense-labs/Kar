//! Scheduled for the combined workbench acceptance run. The current test binary supplies Codex stdio.
use kar::chat::{ChatFeedSnapshot, ChatReplica, HttpChatFeed};
use reqwest::{Client, StatusCode};
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

#[path = "project_lifecycle.rs"]
mod lifecycle;

use super::workbench_server::{client, json, serve};

#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(120), lifecycle::run())
        .await
        .unwrap();
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
        let refs = format!("{scope}/git/refs");
        assert_eq!(json(b.get(&refs)).await["is_repo"], false);
        json(a.post(format!("{scope}/git")).json(&json!({"op":"init"}))).await;
        assert_eq!(json(b.get(&refs)).await["is_at_repository_root"], true);
        assert!(path.join(".git/HEAD").is_file());
        let file_url = format!("{scope}/file?path=notes%2Fsame.txt");
        json(
            a.put(&file_url)
                .json(&json!({"text":format!("only-{name}\ncommon-marker")})),
        )
        .await;
        let text = json(b.get(&file_url)).await;
        assert_eq!(text["text"], format!("only-{name}\ncommon-marker"));
        let absent = json(b.get(format!("{scope}/file?path=missing&allow_missing=true"))).await;
        assert_eq!(absent["exists"], false);
        let bom = path.join("bom");
        std::fs::write(&bom, b"\xef\xbb\xbfhello").unwrap();
        let absolute = json(b.get(format!("{scope}/file")).query(&[("path", &bom)])).await;
        assert_eq!(absolute["text"], "hello");
        assert_eq!(absolute["byte_length"], 8);
        json(
            a.put(format!("{scope}/file?path=bom"))
                .json(&json!({"text":"saved"})),
        )
        .await;
        assert_eq!(std::fs::read(&bom).unwrap(), b"\xef\xbb\xbfsaved");
        let mut split = vec![b'x'; 1024 * 1024 - 1];
        split.extend_from_slice("界".as_bytes());
        std::fs::write(path.join("limit"), &split).unwrap();
        let preview = json(b.get(format!("{scope}/file?path=limit"))).await;
        assert_eq!(preview["truncated"], true);
        assert_eq!(preview["byte_length"], split.len());
        assert_eq!(preview["text"].as_str().unwrap().len(), 1024 * 1024 - 1);
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
        std::fs::write(path.join(".gitignore"), "*.local\ncache/\n").unwrap();
        std::fs::write(path.join("MiXeD.local"), b"123").unwrap();
        std::fs::create_dir(path.join("cache")).unwrap();
        std::fs::write(path.join("cache/deep.local"), "ignored directory").unwrap();
        let direct = json(b.get(format!("{scope}/entries?directory="))).await;
        let entries = direct["entries"].as_array().unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e["path"] == "MiXeD.local" && e["ignored"] == true)
        );
        assert!(
            !entries
                .iter()
                .any(|e| e["path"] == ".git" || e["path"] == "outside")
        );
        let ignored = json(b.get(format!("{scope}/ignored-files?query=LoCaL"))).await;
        assert_eq!(
            ignored["files"],
            json!([{"path":"MiXeD.local","byte_length":3}])
        );
        let dirs = json(a.get(format!("{scope}/browse?partial_path=NO"))).await;
        assert_eq!(dirs["entries"][0]["name"], "notes");
        std::fs::write(path.join("notes/find.txt"), "🦀 KAr Karpet Kar [\n").unwrap();
        std::fs::write(path.join("cover.PNG"), b"image").unwrap();
        let images = json(
            a.get(format!("{scope}/search/entries"))
                .query(&[("image_only", "true"), ("query", "cov")]),
        )
        .await;
        assert_eq!(images["entries"][0]["path"], "cover.PNG");
        let found = json(
            b.get(format!("{scope}/search/contents"))
                .query(&[("query", "Kar"), ("whole_word", "true")]),
        )
        .await;
        let hit = found["matches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"] == "notes/find.txt")
            .unwrap();
        assert_eq!(
            hit["match_ranges"],
            json!([{"start":3,"end":6},{"start":14,"end":17}])
        );
        let literal = json(b.get(format!("{scope}/search/contents")).query(&[
            ("query", "["),
            ("use_regex", "true"),
            ("limit", "1"),
        ]))
        .await;
        assert!(literal["regex_fallback_error"].is_string());
        assert!(
            literal["matches"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["path"] == "notes/find.txt")
        );
        let magic = path.join("report");
        std::fs::write(&magic, b"%PDF-1.7").unwrap();
        let info = json(
            a.post(format!("{base}/filesystem/metadata"))
                .json(&json!({"paths":[&magic,&native,path.join("missing"),&magic,&path]})),
        )
        .await;
        assert_eq!(
            info["entries"][0],
            json!({"kind":"file","byte_length":8,"mime_type":"application/pdf"})
        );
        assert_eq!(info["entries"][1], serde_json::Value::Null);
        assert_eq!(info["entries"][2], serde_json::Value::Null);
        assert_eq!(info["entries"][3], info["entries"][0]);
        assert_eq!(info["entries"][4]["kind"], "directory");
        let picker = json(a.get(format!("{scope}/models/codex"))).await;
        assert_eq!(picker["status"], "live");
        assert_eq!(picker["models"][0]["id"], "demo");
        assert_eq!(picker["models"][0]["default"], true);
        assert_eq!(picker["models"][0]["effortLevels"], json!(["high"]));
        let outside = json!({"id":Uuid::new_v4(),"runtime":"codex","settings":{"additional_directories":[native]}});
        let rejected = a
            .post(format!("{scope}/chats"))
            .json(&outside)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        for _ in 0..2 {
            let id = Uuid::new_v4();
            let create =
                json!({"id":id,"runtime":"codex","settings":{"end_turn_processes_on_abort":true}});
            let entry = json(a.post(format!("{scope}/chats")).json(&create)).await;
            let retried = json(b.post(format!("{scope}/chats")).json(&create)).await;
            assert_eq!(entry, retried);
            assert_eq!(
                entry["info"]["settings"]["end_turn_processes_on_abort"],
                true
            );
            let revised =
                json!({"id":id,"runtime":"codex","settings":{"end_turn_processes_on_abort":false}});
            let conflicting = b
                .post(format!("{scope}/chats"))
                .json(&revised)
                .send()
                .await
                .unwrap();
            assert_eq!(conflicting.status(), StatusCode::BAD_REQUEST);
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
