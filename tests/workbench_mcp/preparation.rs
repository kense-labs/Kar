use super::*;

async fn stage(client: &Client, uri: &str, expected: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let row = server::json(client.get(uri)).await;
            if row["stage"] == expected {
                return row;
            }
            assert_ne!(row["stage"], "failed", "{row}");
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .expect("preparation timed out")
}
pub(super) async fn scenario() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let roots = dir.join("projects");
    let path = roots.join("source");
    let state = dir.join("state");
    let db = dir.join("native");
    fs::create_dir_all(&path).unwrap();
    fs::create_dir(&db).unwrap();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&path)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=Kar",
        "-c",
        "user.email=kar@example.test",
        "commit",
        "-qm",
        "initial",
        "--allow-empty",
    ]);
    let (mut kar, url) = server::serve(&state, &roots, &db).await;
    let c = server::client();
    let b = server::client();
    let p = server::json(
        c.post(format!("{url}/projects"))
            .json(&json!({"name":"test","root":path})),
    )
    .await;
    let pid = p["id"].as_str().unwrap();
    let chat = Uuid::new_v4();
    let run = Uuid::new_v4();
    let suffix = format!("/projects/{pid}/chats/{chat}/prepared-runs/{run}");
    let uri = format!("{url}{suffix}");
    let script = |cmd: &str| json!({"type":"project.update","projectId":pid,"commandId":Uuid::new_v4(),"scripts":[{"id":"init","icon":"configure","name":"Initialize","command":cmd,"runOnWorktreeCreate":true}]});
    server::json(
        c.post(format!("{url}/projects/mutate"))
            .json(&script("exit 13")),
    )
    .await;
    let spec = json!({"launch":{"runtime":"codex"},"title":"prepare","message":"one initial turn","requested_output_schema":{"type":"object"},"workspace":{"type":"worktree","directory":roots.join("new-tree"),"branch":"prepared"},"defer_release":true});
    server::json(c.put(&uri).json(&spec)).await;
    let failed = stage(&b, &uri, "failed").await;
    assert!(
        failed["event"]["detail"].as_str().unwrap().contains("13"),
        "{failed}"
    );
    assert_eq!(failed["attempt"], 1);
    let cwd = std::path::PathBuf::from(failed["workspace"].as_str().unwrap());
    kar.kill().await.unwrap();
    kar.wait().await.unwrap();
    let (mut kar, url) = server::serve(&state, &roots, &db).await;
    let uri = format!("{url}{suffix}");
    assert_eq!(
        server::json(c.put(&uri).json(&spec)).await["stage"],
        "failed"
    );
    assert!(
        c.put(&uri)
            .json(&json!({"message":"different","launch":{"runtime":"codex"}}))
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    server::json(
        c.post(format!("{url}/projects/mutate"))
            .json(&script("echo prepared >> setup-count")),
    )
    .await;
    let command = json!({"command_id":Uuid::new_v4()});
    let (a, d) = tokio::join!(
        server::json(c.post(format!("{uri}/retry")).json(&command)),
        server::json(b.post(format!("{uri}/retry")).json(&command))
    );
    assert_eq!(a["attempt"], 2);
    assert_eq!(d["attempt"], 2);
    stage(&c, &uri, "ready").await;
    assert_eq!(
        fs::read_to_string(cwd.join("setup-count"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    kar.kill().await.unwrap();
    kar.wait().await.unwrap();
    let (mut kar, url) = server::serve(&state, &roots, &db).await;
    let uri = format!("{url}{suffix}");
    assert_eq!(server::json(c.get(&uri)).await["stage"], "ready");
    let a = c
        .post(format!("{uri}/release"))
        .json(&json!({"command_id":Uuid::new_v4()}));
    let d = b
        .post(format!("{uri}/release"))
        .json(&json!({"command_id":Uuid::new_v4()}));
    let _ = tokio::join!(server::json(a), server::json(d));
    let released = stage(&c, &uri, "released").await;
    assert_eq!(released["result"]["input"]["id"], json!(run));
    let queue = server::json(c.get(format!("{url}/projects/{pid}/chats/{chat}/queue"))).await;
    assert_eq!(
        queue["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["id"] == json!(run))
            .count(),
        1
    );
    let events = b
        .get(format!("{uri}/events"))
        .header(
            "Last-Event-ID",
            failed["sequence"].as_u64().unwrap().to_string(),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        events.contains("\"stage\":\"ready\"")
            && events.contains("\"stage\":\"released\"")
            && !events.contains("\"stage\":\"failed\""),
        "{events}"
    );
    kar.kill().await.unwrap();
}
