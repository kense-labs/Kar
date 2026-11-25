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
    fork_and_base(client, &endpoint, repo, bare, mock).await;
    worktree_links_and_archive_boundaries(client, project, &endpoint, repo, bare, mock).await;
}

async fn fork_and_base(client: &Client, endpoint: &str, repo: &Path, origin: &Path, mock: &Mock) {
    let fork = tempfile::tempdir().unwrap();
    git(fork.path(), &["init", "--bare"]);
    git(
        repo,
        &[
            "remote",
            "add",
            "publication",
            "https://review.example/contributor/renamed.git",
        ],
    );
    git(
        repo,
        &[
            "remote",
            "set-url",
            "--push",
            "publication",
            fork.path().to_str().unwrap(),
        ],
    );
    git(repo, &["switch", "main"]);
    git(repo, &["push", "-u", "origin", "main"]);
    let origin_main = git(origin, &["rev-parse", "main"]);
    fs::write(repo.join("fork-change"), "only for the fork").unwrap();
    git(repo, &["add", "fork-change"]);
    git(
        repo,
        &[
            "commit",
            "--only",
            "-m",
            "Fork-only change",
            "--",
            "fork-change",
        ],
    );
    let fork_head = git(repo, &["rev-parse", "main"]);
    git(repo, &["config", "remote.pushDefault", "origin"]);
    git(repo, &["config", "branch.main.pushRemote", "publication"]);
    // The base repository and another contributor already have a PR with this head branch name.
    {
        let mut db = mock.lock().unwrap();
        for repository in ["team/repo", "unrelated/renamed"] {
            let n = db.pulls.len() + 1;
            let mut other = db.pulls[0].clone();
            other["number"] = json!(n);
            other["head"]["ref"] = json!("main");
            other["head"]["repo"] = json!({"full_name":repository});
            db.pulls.push(other);
        }
    }
    let before = mock.lock().unwrap().mutations;
    let op = || json!({"id":Uuid::new_v4(),"action":"create_pr","base":"main","title":"Fork main"});
    let first = execute(client, endpoint, &op()).await;
    assert_eq!(
        result(&first)["push"]["upstream_branch"],
        "publication/main"
    );
    assert_eq!(result(&first)["pr"]["status"], "created");
    let number = result(&first)["pr"]["pull"]["number"].clone();
    assert_eq!(
        result(&first)["pr"]["pull"]["head_repository"],
        "contributor/renamed"
    );
    assert_eq!(
        git(repo, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/main"
    );
    // Deleting the published head out of band leaves both the local tracking ref and pull base current.
    git(fork.path(), &["update-ref", "-d", "refs/heads/main"]);
    let second = execute(client, endpoint, &op()).await;
    assert_eq!(result(&second)["push"]["status"], "pushed");
    assert_eq!(result(&second)["pr"]["status"], "opened_existing");
    assert_eq!(result(&second)["pr"]["pull"]["number"], number);
    assert_eq!(mock.lock().unwrap().mutations, before + 1);
    assert_eq!(git(fork.path(), &["rev-parse", "main"]), fork_head);

    // A local comparison base publishes the full local name even when it looks like a remote ref.
    git(repo, &["config", "remote.pushDefault", "publication"]);
    git(
        repo,
        &["switch", "--track", "-c", "publication/main", "main"],
    );
    let r = execute(
        client,
        endpoint,
        &json!({"id":Uuid::new_v4(),"action":"create_pr"}),
    )
    .await;
    let r = result(&r);
    assert_eq!(r["push"]["published_branch"], "publication/main");
    assert_eq!(r["pr"]["pull"]["head_ref"], "publication/main");
    assert_eq!(r["pr"]["pull"]["base_ref"], "main");
    assert_eq!(
        git(
            repo,
            &["config", "branch.publication/main.kar-merge-base-ref"]
        ),
        "refs/heads/main"
    );
    assert_eq!(
        git(repo, &["config", "branch.publication/main.remote"]),
        "publication"
    );
    assert_eq!(
        git(repo, &["config", "branch.publication/main.merge"]),
        "refs/heads/publication/main"
    );
    assert_eq!(git(origin, &["rev-parse", "main"]), origin_main);
}

async fn worktree_links_and_archive_boundaries(
    client: &Client,
    project: &str,
    endpoint: &str,
    repo: &Path,
    bare: &Path,
    mock: &Mock,
) {
    let root = repo.parent().unwrap();
    git(repo, &["remote", "remove", "publication"]);
    git(repo, &["config", "remote.pushDefault", "origin"]);
    let _ = Command::new("git")
        .current_dir(repo)
        .args(["config", "--unset", "branch.main.pushRemote"])
        .status();
    git(
        repo,
        &["remote", "set-url", "--push", "origin", bare.to_str().unwrap()],
    );
    git(
        repo,
        &[
            "fetch",
            bare.to_str().unwrap(),
            "stacked-topic:refs/remotes/origin/stacked-topic",
        ],
    );
    git(repo, &["checkout", "-f", "stacked-topic"]);
    git(repo, &["reset", "--hard", "refs/remotes/origin/stacked-topic"]);

    // 1. Chat bound to its own worktree executes git_actions in that worktree, not project root.
    let wt_chat = Uuid::new_v4();
    let wt_run = Uuid::new_v4();
    let wt_dir = root.join("wt-git-action");
    let prep_url = format!("{project}/chats/{wt_chat}/prepared-runs/{wt_run}");
    ok(client
        .put(&prep_url)
        .json(&json!({
            "launch": { "runtime": "codex" },
            "title": "wt-chat",
            "message": "seed worktree",
            "workspace": {
                "type": "worktree",
                "directory": wt_dir,
                "branch": "wt-stacked-branch",
                "base": "main"
            }
        })))
    .await;
    for _ in 0..200 {
        let state = ok(client.get(&prep_url)).await;
        if state["stage"] == "dispatched" {
            break;
        }
        assert_ne!(state["stage"], "failed", "{state}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let mut wt_path = std::path::PathBuf::new();
    for _ in 0..200 {
        let snap = ok(client.get(format!("{project}/chats/{wt_chat}/snapshot"))).await;
        if snap["history"]["turns"]
            .as_array()
            .is_some_and(|t| t.first().is_some_and(|r| r["status"] == "completed"))
        {
            wt_path = std::path::PathBuf::from(
                snap["history"]["chat"]["workspace"].as_str().unwrap(),
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(wt_path, wt_dir);
    assert_ne!(wt_path, repo);

    fs::write(wt_path.join("wt_only.txt"), "from-worktree").unwrap();
    let wt_op = json!({
        "id": Uuid::new_v4(),
        "chat": wt_chat,
        "action": "commit_push_pr",
        "files": ["wt_only.txt"],
        "commit_message": "worktree stacked change",
        "base": "main",
        "title": "Worktree PR"
    });
    let wt_done = execute(client, endpoint, &wt_op).await;
    let wt_res = result(&wt_done);
    assert_eq!(wt_res["commit"]["status"], "created");
    assert_eq!(wt_res["push"]["status"], "pushed");
    assert_eq!(wt_res["pr"]["status"], "created");
    let wt_pr_num = wt_res["pr"]["pull"]["number"].as_u64().unwrap();
    assert!(!repo.join("wt_only.txt").exists());
    assert_eq!(
        git(bare, &["show", "wt-stacked-branch:wt_only.txt"]),
        "from-worktree"
    );

    // 2. opened_existing PR links the requesting chat when not previously linked.
    let second_chat = Uuid::new_v4();
    ok(client
        .post(format!("{project}/chats"))
        .json(&json!({"id": second_chat, "runtime": "codex"})))
    .await;
    let reopen_existing = execute(
        client,
        endpoint,
        &json!({
            "id": Uuid::new_v4(),
            "chat": second_chat,
            "action": "create_pr"
        }),
    )
    .await;
    let existing_res = result(&reopen_existing);
    assert_eq!(existing_res["pr"]["status"], "opened_existing");
    let existing_pr_num = existing_res["pr"]["pull"]["number"].as_u64().unwrap();
    let links_url = format!("{project}/forge/links");
    let all_links = ok(client.get(&links_url)).await;
    assert!(all_links.as_array().unwrap().iter().any(|row| {
        row["binding"]["chat"] == second_chat.to_string()
            && row["binding"]["kind"] == "pull"
            && row["binding"]["number"] == existing_pr_num
    }));
    assert!(all_links.as_array().unwrap().iter().any(|row| {
        row["binding"]["chat"] == wt_chat.to_string()
            && row["binding"]["kind"] == "pull"
            && row["binding"]["number"] == wt_pr_num
    }));

    // 3. Duplicate link deduplication and HTTP unlink.
    for _ in 0..2 {
        ok(client.post(&links_url).json(&json!({
            "id": Uuid::new_v4(),
            "chat": second_chat,
            "kind": "issue",
            "number": 7
        })))
        .await;
    }
    let after_dup = ok(client.get(&links_url)).await;
    assert_eq!(
        after_dup
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| {
                row["binding"]["chat"] == second_chat.to_string()
                    && row["binding"]["kind"] == "issue"
                    && row["binding"]["number"] == 7
            })
            .count(),
        1
    );
    let unlinked = ok(client.delete(&links_url).json(&json!({
        "chat": second_chat,
        "kind": "issue",
        "number": 7
    })))
    .await;
    assert_eq!(unlinked["changes"][0]["previously_linked"], true);
    assert_eq!(unlinked["changes"][0]["linked"], false);
    let after_unlink = ok(client.get(&links_url)).await;
    assert!(
        !after_unlink.as_array().unwrap().iter().any(|row| {
            row["binding"]["chat"] == second_chat.to_string()
                && row["binding"]["kind"] == "issue"
                && row["binding"]["number"] == 7
        })
    );

    // 4. Cross-remote fork origin + upstream base PR resolution.
    let base_url = project.rsplit_once("/projects/").unwrap().0;
    let fork_bare = tempfile::tempdir().unwrap();
    git(fork_bare.path(), &["init", "--bare"]);
    let fork_checkout = root.join("fork-upstream-repo");
    fs::create_dir(&fork_checkout).unwrap();
    git(&fork_checkout, &["init", "-b", "main"]);
    git(&fork_checkout, &["fetch", bare.to_str().unwrap(), "main:refs/remotes/upstream/main"]);
    git(&fork_checkout, &["reset", "--hard", "refs/remotes/upstream/main"]);
    git(
        &fork_checkout,
        &["remote", "add", "origin", "https://review.example/contributor/forked.git"],
    );
    git(
        &fork_checkout,
        &["remote", "set-url", "--push", "origin", fork_bare.path().to_str().unwrap()],
    );
    git(
        &fork_checkout,
        &["remote", "add", "upstream", "https://review.example/team/repo.git"],
    );
    git(
        &fork_checkout,
        &["switch", "--track", "-c", "fork-topic", "refs/remotes/upstream/main"],
    );
    git(&fork_checkout, &["config", "remote.pushDefault", "origin"]);
    fs::write(fork_checkout.join("upstream_pr.txt"), "cross-owner").unwrap();
    git(&fork_checkout, &["add", "upstream_pr.txt"]);
    git(&fork_checkout, &["commit", "-m", "cross-owner change"]);
    mock.lock().unwrap().head = git(&fork_checkout, &["rev-parse", "HEAD"]);
    let fork_proj = ok(client
        .post(format!("{base_url}/projects"))
        .json(&json!({"name": "fork-upstream", "root": fork_checkout})))
    .await;
    let fork_pid = fork_proj["id"].as_str().unwrap();
    let fork_action = execute(
        client,
        &format!("{base_url}/projects/{fork_pid}/git/actions"),
        &json!({"id": Uuid::new_v4(), "action": "create_pr", "title": "Upstream from fork"}),
    )
    .await;
    let fork_res = result(&fork_action);
    assert_eq!(fork_res["pr"]["status"], "created");
    assert_eq!(fork_res["pr"]["pull"]["head_repository"], "contributor/forked");
    assert_eq!(fork_res["pr"]["pull"]["head_ref"], "fork-topic");
    assert_eq!(fork_res["pr"]["pull"]["base_ref"], "main");
    ok(client
        .post(format!("{base_url}/projects/mutate"))
        .json(&json!({
            "type": "project.delete",
            "commandId": Uuid::new_v4().to_string(),
            "projectId": fork_pid,
            "force": true
        })))
    .await;

    // 5. Archived chats refuse git actions, forge operations, and forge link mutations.
    ok(client
        .put(format!("{project}/chats/{second_chat}/archive"))
        .json(&json!({"archived": true})))
    .await;
    assert_eq!(
        client
            .post(endpoint)
            .json(&json!({"id": Uuid::new_v4(), "chat": second_chat, "action": "push"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(format!("{project}/forge/operations"))
            .json(&json!({
                "id": Uuid::new_v4(),
                "chat": second_chat,
                "op": "comment_issue",
                "number": 8,
                "body": "should fail"
            }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(&links_url)
            .json(&json!({
                "id": Uuid::new_v4(),
                "chat": second_chat,
                "kind": "issue",
                "number": 7
            }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}
