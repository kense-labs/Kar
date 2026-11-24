use super::*;
use reqwest::Client;

async fn execute(client: &Client, url: &str, input: &Value) -> Value {
    let started = ok(client.post(url).json(input)).await;
    let operation = format!("{url}/{}", started["id"].as_str().unwrap());
    let stream = client
        .get(format!("{operation}/events"))
        .send()
        .await
        .unwrap();
    assert!(stream.status().is_success());
    let payload = tokio::time::timeout(Duration::from_secs(20), stream.text())
        .await
        .unwrap()
        .unwrap();
    let row = ok(client.get(&operation)).await;
    let events = row["events"].as_array().unwrap();
    assert!(row["terminal"].as_bool().unwrap());
    assert_eq!(payload.matches("event: git_action").count(), events.len());
    assert!(payload.contains(&format!("id: {}", events.len())));
    row
}
fn result(r: &Value) -> &Value {
    let end = r["events"].as_array().unwrap().last().unwrap();
    assert_eq!(end["kind"], "action_finished", "{end}");
    &end["result"]
}
pub(super) async fn run(
    client: &Client,
    project: &str,
    repo: &Path,
    bare: &Path,
    chat: Uuid,
    mock: &Mock,
) {
    let endpoint = format!("{project}/git/actions");
    fs::write(repo.join("selected"), "one").unwrap();
    fs::write(repo.join("unrelated"), "not selected").unwrap();
    git(repo, &["add", "unrelated"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hook = repo.join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\necho kar-local-hook >&2\n").unwrap();
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let op = json!({"id":Uuid::new_v4(),"action":"commit","commit_message":"selected change","files":["selected"]});
    let committed = execute(client, &endpoint, &op).await;
    assert_eq!(result(&committed)["commit"]["status"], "created");
    assert_eq!(git(repo, &["diff", "--cached", "--name-only"]), "unrelated");
    assert_eq!(
        git(
            repo,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
        ),
        "selected"
    );
    assert_eq!(execute(client, &endpoint, &op).await, committed);
    #[cfg(unix)]
    {
        let events = committed["events"].as_array().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "hook_started" && e["hook_name"] == "pre-commit")
        );
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "hook_output" && e["text"] == "kar-local-hook")
        );
    }
    fs::write(repo.join("selected"), "two").unwrap();
    let before = mock.lock().unwrap().mutations;
    let op = json!({"id":Uuid::new_v4(),"chat":chat,"action":"commit_push_pr","commit_message":"publish selected","files":["selected"],"feature_branch":true,"branch_name":"stacked-topic","title":"Stacked PR"});
    let r = execute(client, &endpoint, &op).await;
    let value = result(&r);
    assert_eq!(value["branch"]["name"], "stacked-topic");
    assert_eq!(value["push"]["status"], "pushed");
    assert_eq!(value["pr"]["status"], "created");
    assert_eq!(git(bare, &["show", "stacked-topic:selected"]), "two");
    assert_eq!(git(repo, &["diff", "--cached", "--name-only"]), "unrelated");
    for action in ["push", "create_pr", "commit_push"] {
        let op = json!({"id":Uuid::new_v4(),"action":action,"files":["selected"],"commit_message":"no changes","chat":chat});
        let r = execute(client, &endpoint, &op).await;
        let value = result(&r);
        match action {
            "push" => assert_eq!(value["push"]["status"], "skipped_up_to_date"),
            "create_pr" => assert_eq!(value["pr"]["status"], "opened_existing"),
            _ => assert_eq!(value["commit"]["status"], "skipped_no_changes"),
        }
    }
    assert_eq!(mock.lock().unwrap().mutations, before + 1);
    let peer = tempfile::tempdir().unwrap();
    git(
        peer.path(),
        &[
            "clone",
            "--branch",
            "stacked-topic",
            bare.to_str().unwrap(),
            ".",
        ],
    );
    fs::write(peer.path().join("peer"), "unrelated remote commit").unwrap();
    git(peer.path(), &["add", "peer"]);
    git(peer.path(), &["commit", "-m", "peer"]);
    git(peer.path(), &["push", "origin", "HEAD"]);
    let ahead = git(bare, &["rev-parse", "stacked-topic"]);
    let r = execute(
        client,
        &endpoint,
        &json!({"id":Uuid::new_v4(),"action":"push"}),
    )
    .await;
    assert_eq!(
        r["events"].as_array().unwrap().last().unwrap()["kind"],
        "action_failed"
    );
    assert_eq!(git(bare, &["rev-parse", "stacked-topic"]), ahead);
}
