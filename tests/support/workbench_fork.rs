use super::workbench_server::{client, json as ok, serve};
use reqwest::Client;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command, time::Duration};
use uuid::Uuid;

pub(super) fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Kar",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn project(client: &Client, url: &str, path: &Path, name: &str) -> String {
    ok(client
        .post(format!("{url}/projects"))
        .json(&json!({"root":path,"name":name})))
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn say(client: &Client, url: &str, text: &str) -> Value {
    ok(client
        .post(format!("{url}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":text})))
    .await
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
    let main = allowed.join("repo");
    let target = allowed.join("child");
    let state = temp.path().join("state");
    let native = temp.path().join("native");
    fs::create_dir_all(&main).unwrap();
    fs::create_dir(&native).unwrap();
    git(&main, &["init", "-b", "main"]);
    fs::write(main.join("shared"), "base").unwrap();
    git(&main, &["add", "shared"]);
    git(&main, &["commit", "-m", "base"]);

    let (mut server, url) = serve(&state, &allowed, &native).await;
    let a = client();
    let b = client();
    let p = project(&a, &url, &main, "parent").await;
    let worktree_id = Uuid::new_v4();
    ok(a.post(format!("{url}/projects/{p}/git")).json(&json!({
        "op":"worktree_add","id":worktree_id,"directory":target,"branch":"forked","start":"main"
    })))
    .await;
    let target_project = project(&b, &url, &target, "forked").await;
    let chat = Uuid::new_v4();
    let child = Uuid::new_v4();
    let created = ok(a
        .post(format!("{url}/projects/{p}/chats"))
        .json(&json!({"id":chat,"runtime":"codex"})))
    .await;
    let parent_path = format!("/projects/{p}/chats/{chat}");
    let child_path = format!("/projects/{target_project}/chats/{child}");
    let parent = format!("{url}{parent_path}");
    let first = say(&a, &parent, "parent context").await;
    let cut = say(&a, &parent, "shared-child").await;
    let latest = say(&b, &parent, "shared-next").await;
    let request = json!({"id":child,"turn":cut["id"],"destination_project":target_project});
    let receipt = ok(a.post(format!("{parent}/forks")).json(&request)).await;
    assert_eq!(receipt["project"], target_project);
    let h = &receipt["history"];
    assert_eq!(h["chat"]["id"], child.to_string());
    assert_ne!(h["chat"]["native_id"], created["info"]["native_id"]);
    assert_eq!(
        h["chat"]["forked_from"]["native_source"],
        created["info"]["native_id"]
    );
    assert_eq!(h["chat"]["worktree_id"], worktree_id.to_string());
    assert_eq!(h["turns"].as_array().unwrap().len(), 2);
    assert_eq!(h["turns"][0]["id"], first["id"]);
    assert_eq!(h["turns"][1]["id"], cut["id"]);
    assert_eq!(fs::read(target.join("shared")).unwrap(), b"shared-child");
    assert_eq!(fs::read(main.join("shared")).unwrap(), b"shared-next");
    let fork_count = fs::read_to_string(native.join("forks")).unwrap();
    assert_eq!(fork_count.lines().count(), 1);
    let child_turn = say(&b, &format!("{url}{child_path}"), "shared-next").await;
    let replay = ok(a.post(format!("{parent}/forks")).json(&request)).await;
    assert_eq!(replay["history"]["turns"][2]["id"], child_turn["id"]);
    assert_eq!(fs::read(target.join("shared")).unwrap(), b"shared-next");
    server.kill().await.unwrap();
    let (mut next, url) = serve(&state, &allowed, &native).await;
    let child_find = format!("{url}{child_path}/find");
    let find = ok(b.post(&child_find).json(&json!({"query":"ha","index":1}))).await;
    // Two distinct native replies have identical text. Both remain searchable after a fork.
    assert!(find["total_matches"].as_u64().unwrap() >= 4);
    let occurrence = &find["match"];
    let detail = ok(b.get(format!(
        "{url}{child_path}/items/{}",
        occurrence["entry_id"].as_str().unwrap()
    )))
    .await;
    assert_eq!(detail["item"]["turn"], occurrence["turn"]);
    let next = ok(b
        .post(&child_find)
        .json(&json!({"query":"HA","start":occurrence,"offset":1})))
    .await;
    assert_eq!(next["active_index"], 2);
    let raw = b
        .post(format!("{child_find}/events"))
        .json(&json!({"query":"ha"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let frames: Vec<Value> = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|text| serde_json::from_str(text).unwrap())
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["complete"], false);
    assert_eq!(frames[0]["cursor"], frames[1]["cursor"]);
    assert_eq!(frames[1]["total_matches"], find["total_matches"]);
    let root_diff = format!("{url}{parent_path}/checkpoints/diff");
    let full = ok(b.get(format!("{root_diff}?to=3"))).await;
    assert!(full["diff"].as_str().unwrap().contains("+shared-next"));
    assert!(full["diff"].as_str().unwrap().contains("-base"));
    let single = ok(b.get(format!("{root_diff}?from=1&to=2"))).await;
    assert!(single["diff"].as_str().unwrap().contains("+shared-child"));
    let inherited = ok(b.get(format!("{url}{child_path}/checkpoints/diff?from=1&to=2"))).await;
    assert_eq!(inherited["diff"], single["diff"]);
    assert_eq!(
        ok(b.get(format!("{root_diff}?from=2&to=2"))).await["diff"],
        ""
    );
    assert!(
        !b.get(format!("{root_diff}?from=4&to=2"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        !b.get(format!("{root_diff}?to=5"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let forks = format!("{url}{parent_path}/forks");
    let replay = ok(b.post(&forks).json(&request)).await;
    assert_eq!(
        replay["history"]["chat"]["native_id"],
        h["chat"]["native_id"]
    );
    let mut wrong = request.clone();
    wrong["turn"] = latest["id"].clone();
    assert!(
        !a.post(&forks)
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        !a.post(&forks)
            .json(&json!({"id":child,"turn":cut["id"]}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let parent_snapshot = ok(b.get(format!("{url}{parent_path}/snapshot"))).await;
    assert_eq!(parent_snapshot["history"]["turns"][2]["id"], latest["id"]);
    say(
        &b,
        &format!("{url}{child_path}"),
        "continue on original fork",
    )
    .await;
    assert_eq!(
        fs::read_to_string(native.join("forks")).unwrap(),
        fork_count
    );
    next.kill().await.unwrap();
}
