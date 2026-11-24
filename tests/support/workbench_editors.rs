use super::workbench_server::{client, json, serve_with_env};
use serde_json::{Value, json};
use std::{fs, time::Duration};
#[tokio::main]
pub(crate) async fn run() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().canonicalize().unwrap();
    let bin = dir.join("bin");
    let root = dir.join("root");
    let db = dir.join("native");
    for p in [&root, &bin, &db] {
        fs::create_dir(p).unwrap();
    }
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), bin.join("code")).unwrap();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let record = dir.join("editor.json");
    let (mut server, url) = serve_with_env(
        &dir.join("state"),
        &root,
        &db,
        &[
            ("PATH", &path),
            ("KAR_TEST_EDITOR_RECORD", record.as_os_str()),
        ],
    )
    .await;
    let c = client();
    let proj = json(
        c.post(format!("{url}/projects"))
            .json(&json!({"root":root,"name":"editors"})),
    )
    .await;
    let api = format!(
        "{url}/projects/{}/editor/open",
        proj["id"].as_str().unwrap()
    );
    let file = "a file (x).rs";
    fs::write(root.join(file), "fn main() {}").unwrap();
    assert!(
        json(c.get(format!("{url}/editors")))
            .await
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == "vscode")
    );
    json(
        c.post(&api)
            .json(&json!({"editor":"vscode","path":file,"position":{"line":4,"column":2}})),
    )
    .await;
    let log = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(data) = fs::read(&record)
                && let Ok(value) = serde_json::from_slice::<Value>(&data)
            {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        log["args"],
        json!(["--goto", format!("{}:4:2", root.join(file).display())])
    );
    assert_eq!(log["owner_env"], false);
    for input in [
        json!({"editor":"vscode","path":"../escape"}),
        json!({"editor":"unregistered","path":"."}),
        json!({"editor":"file-manager","path":"missing","reveal":true}),
    ] {
        assert!(
            !c.post(&api)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    server.kill().await.unwrap();
}
pub(crate) fn peer() {
    fs::write(std::env::var_os("KAR_TEST_EDITOR_RECORD").unwrap(),serde_json::to_vec(&json!({"args":std::env::args().skip(1).collect::<Vec<_>>(),"owner_env":std::env::var_os("KAR_WORKBENCH_TOKEN").is_some()})).unwrap()).unwrap();
}
