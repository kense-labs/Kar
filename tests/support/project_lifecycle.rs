//! Normal HTTP requests against the Kar process. The native Codex peer is a protocol fixture.
use super::super::workbench_server::{client, json, serve_with_env};
use kar::chat::{ChatStore, TurnStatus};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use uuid::Uuid;

pub(super) async fn run() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let root = temp.path().join("allowed");
    let native = temp.path().join("native");
    for p in [&root, &native] {
        fs::create_dir(p).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let old = root.join("original");
    let new = root.join("moved");
    let tracked = root.join("tracked");
    fs::create_dir(&new).unwrap();
    fs::create_dir(&tracked).unwrap();
    let a = client();
    let (mut server, base) = serve_with_env(
        &state,
        &root,
        &native,
        &[
            ("GIT_AUTHOR_NAME", "".as_ref()),
            ("GIT_COMMITTER_NAME", "".as_ref()),
        ],
    )
    .await;
    let mutate = format!("{base}/projects/mutate");
    let project = Uuid::new_v4();
    let cmd = json!({"type":"project.create","commandId":"create","projectId":project,"workspaceRoot":old,"createWorkspaceRootIfMissing":true,"title":"Before"});
    let created = json(a.post(&mutate).json(&cmd)).await;
    assert_eq!(created, json(a.post(&mutate).json(&cmd)).await);
    let id = Uuid::new_v4();
    let create_chat = json!({"id":id,"runtime":"codex"});
    let url = format!("{base}/projects/{project}/chats");
    let prev = json(a.post(&url).json(&create_chat)).await;
    let update = json!({"type":"project.update","commandId":"move","projectId":project,"title":"After","workspaceRoot":new,"pinned":true,"archived":true,"autoPull":true,"trackFolders":[{"name":"Other","path":tracked}],"scripts":[{"id":"s","name":"Check","command":"pwd","icon":"test","runOnWorktreeCreate":false}]});
    let adjusted = json(a.post(&mutate).json(&update)).await;
    assert_eq!(adjusted["pinned"], true);
    assert_eq!(adjusted["archived"], true);
    assert_eq!(adjusted["settings"]["scripts"][0]["name"], "Check");
    let mut conflict = update.clone();
    conflict["title"] = json!("Collision");
    assert!(
        !a.post(&mutate)
            .json(&conflict)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let newchat = json(a.post(&url).json(&json!({"runtime":"codex"}))).await;
    assert_eq!(newchat["info"]["workspace"], json!(new));
    assert_eq!(
        json(a.post(&url).json(&create_chat)).await["info"]["native_id"],
        prev["info"]["native_id"]
    );
    let recovered=json(a.post(&mutate).json(&json!({"type":"project.update","commandId":"unarchive","projectId":project,"archived":false,"untrackFolders":[tracked]}))).await;
    assert_eq!(recovered["archived"], false);
    let scratch = json(a.post(format!("{base}/projects/ensure-scratch"))).await;
    assert_eq!(
        scratch,
        json(a.post(format!("{base}/projects/ensure-scratch"))).await
    );
    let s = scratch["id"].as_str().unwrap();
    let first = json(
        a.post(format!("{base}/projects/{s}/chats"))
            .json(&json!({"runtime":"codex"})),
    )
    .await;
    let other = json(
        a.post(format!("{base}/projects/{s}/chats"))
            .json(&json!({"runtime":"codex"})),
    )
    .await;
    let first_id = first["info"]["id"].as_str().unwrap();
    let folder = Path::new(first["info"]["workspace"].as_str().unwrap());
    assert_eq!(
        folder.parent(),
        Some(Path::new(scratch["root"].as_str().unwrap()))
    );
    assert_ne!(first["info"]["workspace"], other["info"]["workspace"]);
    fs::write(folder.join("keep"), "original").unwrap();
    let git = super::super::workbench_fork::git;
    git(folder, &["init", "-b", "main"]);
    git(folder, &["add", "keep"]);
    git(folder, &["commit", "-m", "seed"]);
    let fork = Uuid::new_v4();
    let src = format!("{base}/projects/{s}/chats/{first_id}");
    let turn = json(
        a.post(format!("{src}/messages"))
            .json(&json!({"id":Uuid::new_v4(),"text":"Scratch sibling history"})),
    )
    .await;
    let forked = json(
        a.post(format!("{src}/forks"))
            .json(&json!({"id":fork,"turn":turn["id"]})),
    )
    .await;
    json(
        a.put(format!("{base}/projects/{s}/chats/{fork}/archive"))
            .json(&json!({"archived":true})),
    )
    .await;
    let promoted = json(
        a.post(format!("{base}/projects/create-from-thread"))
            .json(&json!({"chat":first_id})),
    )
    .await;
    assert_eq!(promoted["root"], first["info"]["workspace"]);
    assert_eq!(
        json(
            a.post(format!("{base}/projects/create-from-thread"))
                .json(&json!({"chat":first_id}))
        )
        .await,
        promoted
    );
    let promoted_id = promoted["id"].as_str().unwrap();
    let moved = json(a.get(format!("{base}/projects/{promoted_id}/chats"))).await;
    assert_eq!(moved[0]["info"]["native_id"], first["info"]["native_id"]);
    let incl = json(a.get(format!(
        "{base}/projects/{promoted_id}/chats?include_archived=true"
    )))
    .await;
    assert!(
        incl.as_array()
            .unwrap()
            .iter()
            .any(|e| e["info"]["id"] == fork.to_string()
                && e["archived"] == true
                && e["info"]["native_id"] == forked["history"]["chat"]["native_id"])
    );
    let remaining = json(a.get(format!("{base}/projects/{s}/chats"))).await;
    assert_eq!(remaining.as_array().unwrap().len(), 1);
    let named = json(
        a.post(format!("{base}/projects/create-new"))
            .json(&json!({"name":"Named Project"})),
    )
    .await;
    assert!(named["commit_error"].is_string());
    assert!(
        Path::new(named["root"].as_str().unwrap())
            .join("assets/icon.svg")
            .is_file()
    );
    server.kill().await.unwrap();
    let offline = ChatStore::open(&state).unwrap();
    let queued = Uuid::new_v4();
    offline.enqueue(id, queued, "old cwd queue").unwrap();
    let (mut server, base) = serve_with_env(
        &state,
        &root,
        &native,
        &[
            ("GIT_AUTHOR_NAME", "Kar".as_ref()),
            ("GIT_COMMITTER_NAME", "Kar".as_ref()),
            ("GIT_AUTHOR_EMAIL", "fixture@test.invalid".as_ref()),
            ("GIT_COMMITTER_EMAIL", "fixture@test.invalid".as_ref()),
            ("GIT_CONFIG_COUNT", "1".as_ref()),
            ("GIT_CONFIG_KEY_0", "commit.gpgSign".as_ref()),
            ("GIT_CONFIG_VALUE_0", "false".as_ref()),
        ],
    )
    .await;
    // Do not attach either client. The restart pump alone must publish the native reply.
    loop {
        if offline
            .history(id)
            .unwrap()
            .turns
            .iter()
            .any(|t| t.id == queued && t.status == TurnStatus::Completed)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let calls = fs::read_to_string(native.join("calls")).unwrap();
    let matching: Vec<Value> = calls
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["text"] == "old cwd queue")
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0]["cwd"], json!(old));
    assert_eq!(matching[0]["id"], prev["info"]["native_id"]);
    let again = json(
        a.post(format!("{base}/projects/create-new"))
            .json(&json!({"name":"Named Project"})),
    )
    .await;
    assert_ne!(again["root"], named["root"]);
    assert!(again.get("commit_error").is_none());
    let tree = std::process::Command::new("git")
        .arg("-C")
        .arg(again["root"].as_str().unwrap())
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .output()
        .unwrap();
    assert!(tree.status.success());
    assert_eq!(
        String::from_utf8(tree.stdout).unwrap(),
        "README.md\nassets/icon.svg\n"
    );
    let mutate = format!("{base}/projects/mutate");
    assert_eq!(adjusted, json(a.post(&mutate).json(&update)).await);
    let del = json!({"type":"project.delete","commandId":"delete","projectId":promoted_id});
    assert!(
        !a.post(&mutate)
            .json(&del)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let mut force = del;
    force["force"] = json!(true);
    let deleted = json(a.post(&mutate).json(&force)).await;
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted, json(a.post(&mutate).json(&force)).await);
    assert_eq!(fs::read_to_string(folder.join("keep")).unwrap(), "original");
    let reused = json(
        a.post(format!("{base}/projects"))
            .json(&json!({"root":folder,"name":"Reopened"})),
    )
    .await;
    assert!(
        json(a.get(format!(
            "{base}/projects/{}/chats?include_archived=true",
            reused["id"].as_str().unwrap()
        )))
        .await
        .as_array()
        .unwrap()
        .is_empty()
    );
    // A failed native startup reserved an ID, but never published a chat or native fork.
    let rid = reused["id"].as_str().unwrap();
    assert!(
        !a.post(format!("{base}/projects/{rid}/chats"))
            .json(&json!({"runtime":"missing-runtime","id":Uuid::new_v4()}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let removed = json(a.post(&mutate).json(
        &json!({"type":"project.delete","commandId":"remove-failed-creation","projectId":rid}),
    ))
    .await;
    assert_eq!(removed["deleted"], true);
    server.kill().await.unwrap();
}
