use super::{
    workbench_fork::git,
    workbench_server::{client, json as ok, serve},
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use uuid::Uuid;

async fn archive(client: &Client, chat: &str, archived: bool) {
    assert_eq!(
        ok(client
            .put(format!("{chat}/archive"))
            .json(&json!({"archived":archived})))
        .await["archived"],
        archived
    );
}
async fn del(client: &Client, chat: &str, command: Uuid, only: bool) -> Value {
    ok(client
        .delete(chat)
        .json(&json!({"command_id":command,"onlyIfUnused":only})))
    .await
}
async fn snapshot(client: &Client, chat: &str) -> StatusCode {
    client
        .get(format!("{chat}/snapshot"))
        .send()
        .await
        .unwrap()
        .status()
}
async fn project(client: &Client, url: &str, path: &Path) -> Uuid {
    serde_json::from_value(
        ok(client
            .post(format!("{url}/projects"))
            .json(&json!({"root":path,"name":"Kar"})))
        .await["id"]
            .clone(),
    )
    .unwrap()
}

#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(120), scenario())
        .await
        .unwrap();
}

async fn scenario() {
    let temp = tempfile::tempdir().unwrap();
    let allowed = temp.path().join("roots");
    let root = allowed.join("repo");
    let other = allowed.join("other");
    let state = temp.path().join("state");
    let native = temp.path().join("native");
    for p in [&root, &native, &other] {
        fs::create_dir_all(p).unwrap();
    }
    git(&root, &["init", "-b", "main"]);
    fs::write(root.join("shared"), "keep").unwrap();
    git(&root, &["add", "shared"]);
    git(&root, &["commit", "-m", "base"]);
    let (mut server, url) = serve(&state, &allowed, &native).await;
    let a = client();
    let b = client();
    let p = project(&a, &url, &root).await;
    let other_p = project(&a, &url, &other).await;
    let chats = format!("{url}/projects/{p}/chats");
    let parent = Uuid::new_v4();
    let root_chat = format!("{chats}/{parent}");
    let create = json!({"id":parent,"runtime":"codex"});
    ok(a.post(&chats).json(&create)).await;
    let draft = Uuid::new_v4();
    let blank = json!({"id":draft,"side_chat":true});
    let empty = ok(a.post(format!("{root_chat}/forks")).json(&blank)).await;
    assert_eq!(empty["history"]["turns"], json!([]));
    let cut = Uuid::new_v4();
    ok(a.post(format!("{root_chat}/messages"))
        .json(&json!({"id":cut,"text":"initial context"})))
    .await;
    let empty = ok(b.post(format!("{root_chat}/forks")).json(&blank)).await;
    assert_eq!(empty["history"]["turns"], json!([]));
    assert_eq!(
        del(&a, &format!("{chats}/{draft}"), Uuid::new_v4(), true).await["deleted"],
        json!([draft])
    );
    let [unused, used, prior, regular] = std::array::from_fn(|_| Uuid::new_v4());
    for child in [unused, used, prior, regular] {
        let args = json!({"id":child,"turn":cut,"sideChat":child!=regular});
        let h = ok(a.post(format!("{root_chat}/forks")).json(&args)).await;
        assert_eq!(h["history"]["chat"]["id"], child.to_string());
        assert_eq!(h["history"]["turns"][0]["id"], cut.to_string());
        if child != regular {
            assert_eq!(h["history"]["chat"]["forked_from"]["context_only"], true);
        }
    }
    assert!(
        !a.post(format!("{chats}/{used}/forks"))
            .json(&json!({"id":Uuid::new_v4(),"turn":cut,"side_chat":true}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let saved = del(&b, &format!("{chats}/{unused}"), Uuid::new_v4(), true).await;
    assert_eq!(saved["deleted"], json!([unused]));
    let unused_private = state.join(format!("{unused}.karchat"));
    assert!(!unused_private.exists());

    // A held input protects an otherwise empty chat. Retrying its old no-op
    // command cannot delete it after the client withdraws the input.
    let queued = Uuid::new_v4();
    let queued_url = format!("{chats}/{queued}");
    ok(a.post(&chats).json(&json!({"id":queued,"runtime":"codex"}))).await;
    ok(a.post(format!("{queued_url}/queue/commands"))
        .json(&json!({"command_id":Uuid::new_v4(),"action":"hold"})))
    .await;
    let pending = Uuid::new_v4();
    ok(a.post(format!("{queued_url}/queue"))
        .json(&json!({"id":pending,"text":"pending"})))
    .await;
    let conditional = Uuid::new_v4();
    assert_eq!(
        del(&b, &queued_url, conditional, true).await["deleted"],
        json!([])
    );
    ok(a.delete(format!("{queued_url}/queue/{pending}"))).await;
    assert_eq!(
        del(&b, &queued_url, conditional, true).await["deleted"],
        json!([])
    );
    assert_eq!(
        del(&b, &queued_url, Uuid::new_v4(), true).await["deleted"],
        json!([queued])
    );

    let used_chat = format!("{chats}/{used}");
    let prior_chat = format!("{chats}/{prior}");
    ok(a.post(format!("{used_chat}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"own turn"})))
    .await;
    let no_op = Uuid::new_v4();
    assert_eq!(del(&b, &used_chat, no_op, true).await["deleted"], json!([]));
    let list = || async { ok(b.get(&chats).query(&[("include_archived", true)])).await };
    let flags = |rows: &Value, id: Uuid| -> bool {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["info"]["id"] == id.to_string())
            .unwrap()["archived"]
            .as_bool()
            .unwrap()
    };
    archive(&a, &prior_chat, true).await;
    archive(&a, &root_chat, true).await;
    let rows = list().await;
    assert!(flags(&rows, parent) && flags(&rows, used) && flags(&rows, prior));
    assert!(!flags(&rows, regular));
    archive(&a, &root_chat, false).await;
    let rows = list().await;
    assert!(!flags(&rows, parent) && !flags(&rows, used) && flags(&rows, prior));

    ok(a.post(format!("{used_chat}/queue/commands"))
        .json(&json!({"command_id":Uuid::new_v4(),"action":"hold"})))
    .await;
    let msg = Uuid::new_v4();
    ok(a.post(format!("{used_chat}/queue"))
        .json(&json!({"id":msg,"text":"held input"})))
    .await;
    let cmd = Uuid::new_v4();
    let req = json!({"commandId":cmd});
    fs::write(native.join("hold-parent-turn"), []).unwrap();
    let send = a
        .post(format!("{root_chat}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"shared-next"}));
    let running = tokio::spawn(async move { ok(send).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !native.join("parent-turn-running").exists() {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        b.delete(&root_chat)
            .json(&req)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    fs::remove_file(native.join("hold-parent-turn")).unwrap();
    running.await.unwrap();
    assert_eq!(
        b.delete(&root_chat)
            .json(&req)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(snapshot(&a, &root_chat).await, StatusCode::OK);
    ok(a.delete(format!("{used_chat}/queue/{msg}"))).await;
    assert_eq!(
        a.delete(format!("{url}/projects/{other_p}/chats/{parent}"))
            .json(&req)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let receipt = ok(a.delete(&root_chat).json(&req)).await;
    let mut ids: Vec<Uuid> = serde_json::from_value(receipt["deleted"].clone()).unwrap();
    ids.sort();
    let mut expected = vec![parent, prior, used];
    expected.sort();
    assert_eq!(ids, expected);
    assert_eq!(snapshot(&b, &root_chat).await, StatusCode::NOT_FOUND);
    assert_eq!(snapshot(&b, &used_chat).await, StatusCode::NOT_FOUND);
    assert_eq!(
        snapshot(&b, &format!("{chats}/{regular}")).await,
        StatusCode::OK
    );
    assert_eq!(fs::read(root.join("shared")).unwrap(), b"shared-next");
    fs::write(root.join("shared"), "keep").unwrap();
    assert!(state.join(format!("{parent}.ancestry")).is_file());
    server.kill().await.unwrap();
    let (mut server, url) = serve(&state, &allowed, &native).await;
    let chats = format!("{url}/projects/{p}/chats");
    let parent_chat = format!("{chats}/{parent}");
    assert_eq!(ok(a.delete(&parent_chat).json(&req)).await, receipt);
    assert_eq!(
        del(&b, &format!("{chats}/{used}"), no_op, true).await["deleted"],
        json!([])
    );
    assert!(
        !a.post(&chats)
            .json(&create)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(snapshot(&b, &parent_chat).await, StatusCode::NOT_FOUND);
    let live = format!("{chats}/{regular}");
    let nested = Uuid::new_v4();
    ok(a.post(format!("{live}/forks"))
        .json(&json!({"id":nested,"turn":cut,"side_chat":true})))
    .await;
    ok(a.post(format!("{live}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"continue independently"})))
    .await;
    del(&b, &live, Uuid::new_v4(), false).await;
    assert!(!state.join(format!("{parent}.ancestry")).exists());
    assert!(root.join(".git").is_dir());
    server.kill().await.unwrap();
}
