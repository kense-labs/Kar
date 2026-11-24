//! Native Git and HTTP workbench with the same Codex protocol fixture as the projects suite.
use super::workbench_server::{client, json, serve};
use serde_json::json;
use std::{path::Path, process::Command, time::Duration};
use uuid::Uuid;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    std::fs::write(path.join("readme"), "source").unwrap();
    git(path, &["add", "readme"]);
    git(
        path,
        &[
            "-c",
            "user.name=Kar",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
    );
}
#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(90), scenario())
        .await
        .unwrap();
}
async fn scenario() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let root = temp.path().join("allowed");
    let native = temp.path().join("native");
    std::fs::create_dir_all(&native).unwrap();
    let project = root.join("project");
    let api = project.join("api");
    let web = project.join("wrapper/web");
    let extra = root.join("extra");
    repo(&api);
    repo(&web);
    repo(&extra);
    let project = project.canonicalize().unwrap();
    let api = api.canonicalize().unwrap();
    let web = web.canonicalize().unwrap();
    std::fs::write(project.join("plain.txt"), "shared").unwrap();
    std::fs::write(
        project.join("demo.code-workspace"),
        r#"{ "folders": [{"name":"API", "path":"api"},],}"#,
    )
    .unwrap();
    let task = root.join("kar-task");
    let (mut server, base) = serve(&state, &root, &native).await;
    let c = client();
    let id = Uuid::new_v4();
    let p = json(
        c.post(format!("{base}/projects"))
            .json(&json!({"root":project,"name":"multi"})),
    )
    .await;
    let scope = format!("{base}/projects/{}", p["id"].as_str().unwrap());
    let rows = json(c.get(format!("{scope}/folders/scan"))).await;
    assert_eq!(rows["folders"][0]["source"], "code-workspace");
    assert_eq!(rows["folders"][1]["source"], "child");
    assert_eq!(
        json(c.get(format!("{scope}/folders")))
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let registered = json(
        c.post(format!("{base}/projects"))
            .json(&json!({"root":&api,"name":"single"})),
    )
    .await;
    let sid = Uuid::new_v4();
    let dir = root.join("single-task");
    let single = format!("{base}/projects/{}", registered["id"].as_str().unwrap());
    let worktree=json(c.post(format!("{single}/git")).json(&json!({"op":"worktree_add","id":sid,"directory":dir,"branch":"single-demo","start":"HEAD"}))).await;
    assert_eq!(worktree["id"], sid.to_string());
    let mut updates = c
        .get(format!("{single}/tasks/{sid}/setup/events"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&updates.chunk().await.unwrap().unwrap())
            .contains("\"phase\":\"done\"")
    );
    drop(updates);
    let not_multi = json(
        c.get(format!("{single}/folders/containing"))
            .query(&[("path", dir.to_str().unwrap())]),
    )
    .await;
    assert!(not_multi.is_null());
    json(
        c.post(format!("{single}/git"))
            .json(&json!({"op":"worktree_remove","id":sid})),
    )
    .await;
    git(&api, &["branch", "task-sample"]);
    let q = json!({"directory":task,"branch":"task-sample","repositories":[api]});
    let endpoint = format!("{scope}/tasks/{id}/folder");
    let created = json(c.put(&endpoint).json(&q)).await;
    let cwd = std::path::PathBuf::from(created["project_path"].as_str().unwrap());
    assert_eq!(
        created["repositories"][0]["selected_branch"],
        "task-sample-2"
    );
    assert!(cwd.join("api/.git").is_file());
    assert_eq!(std::fs::read(cwd.join("plain.txt")).unwrap(), b"shared");
    let child = api.strip_prefix(&project).unwrap();
    std::fs::write(cwd.join(child).join("readme"), "isolated").unwrap();
    assert_eq!(std::fs::read(api.join("readme")).unwrap(), b"source");
    let append = format!("{endpoint}/repositories");
    let added = json(c.post(&append).json(&json!({"repository_path":web}))).await;
    assert_eq!(added["repositories"].as_array().unwrap().len(), 2);
    assert!(cwd.join("wrapper/web/.git").is_file());
    for path in [&web, &extra] {
        assert!(
            !c.post(&append)
                .json(&json!({"repository_path":path}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    assert_eq!(
        json(c.put(&endpoint).json(&q)).await["root"],
        created["root"]
    );
    let blocked = project.join("blocked");
    repo(&blocked);
    std::fs::create_dir(cwd.join("blocked")).unwrap();
    std::fs::write(cwd.join("blocked/keep"), "user bytes").unwrap();
    assert!(
        !c.post(&append)
            .json(&json!({"repository_path":blocked}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        std::fs::read(cwd.join("blocked/keep")).unwrap(),
        b"user bytes"
    );
    cancellation(&c, &scope, &root, &project).await;
    let chat = json(
        c.post(format!("{scope}/chats"))
            .json(&json!({"id":id,"runtime":"codex"})),
    )
    .await;
    assert_eq!(chat["info"]["workspace"], created["project_path"]);
    let infos = json(c.get(format!("{scope}/chats"))).await;
    assert!(
        infos
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["info"]["id"] == id.to_string())
    );
    let setup = format!("{scope}/tasks/{id}/setup");
    assert_eq!(
        json(c.post(format!("{setup}/cancel"))).await,
        json!({"cancelled":false})
    );
    let mut events = c.get(format!("{setup}/events")).send().await.unwrap();
    let first = events.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("\"phase\":\"done\""));
    drop(events);
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    let (mut restored, newbase) = serve(&state, &root, &native).await;
    let suffix = scope.strip_prefix(&base).unwrap();
    let found = json(
        c.get(format!("{newbase}{suffix}/folders/containing"))
            .query(&[("path", cwd.join("wrapper/web").to_str().unwrap())]),
    )
    .await;
    assert_eq!(found["root"], created["root"]);
    assert_eq!(found["repositories"], added["repositories"]);
    assert!(std::fs::read_to_string(cwd.join("api/readme")).unwrap() == "isolated");
    restored.kill().await.unwrap();
    restored.wait().await.unwrap();
}

#[cfg(unix)]
async fn cancellation(c: &reqwest::Client, scope: &str, root: &Path, project: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let source = project.join("cancel-source");
    repo(&source);
    let hook = source.join(".git/hooks/post-checkout");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho running > \"$0.started\"\nexec sleep 90\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let target = root.join("cancel-target");
    let id = Uuid::new_v4();
    let req = c
        .put(format!("{scope}/tasks/{id}/folder"))
        .json(&json!({"directory":target,"branch":"cancel-demo","repositories":[source]}));
    let put = tokio::spawn(async move { req.send().await.unwrap().status() });
    let marker = source.join(".git/hooks/post-checkout.started");
    for _ in 0..250 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        marker.exists(),
        "the checkout did not enter its native hook"
    );
    let setup = format!("{scope}/tasks/{id}/setup");
    let stopped = tokio::time::timeout(
        Duration::from_secs(12),
        json(c.post(format!("{setup}/cancel"))),
    )
    .await
    .unwrap();
    assert_eq!(stopped, json!({"cancelled":true}));
    assert!(!put.await.unwrap().is_success());
    assert_eq!(
        json(c.post(format!("{setup}/cancel"))).await,
        json!({"cancelled":false})
    );
    let mut stream = c
        .get(format!("{setup}/events"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&stream.chunk().await.unwrap().unwrap())
            .contains("\"phase\":\"cancelled\"")
    );
    assert!(!target.join("cancel-source/.git").exists());
    assert!(source.join(".git").is_dir());
    assert!(marker.exists()); // user-owned files in the source repository survive.
}
#[cfg(not(unix))]
async fn cancellation(_: &reqwest::Client, _: &str, _: &Path, _: &Path) {}
