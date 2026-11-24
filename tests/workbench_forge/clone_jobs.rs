//! Real git clone with a local smudge filter held at a gate, so cancellation has a deterministic cut.
use super::*;
use std::io::{Read, Write};

pub(super) fn filter() {
    let mut bytes = vec![];
    std::io::stdin().read_to_end(&mut bytes).unwrap();
    let gate = std::path::PathBuf::from(std::env::var_os("KAR_TEST_CLONE_GATE").unwrap());
    fs::write(gate.join("active"), "started").unwrap();
    while !gate.join("release").exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::io::stdout().write_all(&bytes).unwrap();
}
pub(super) async fn run() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("roots");
    let src = root.join("source");
    let gate = temp.path().join("gate");
    let native = temp.path().join("native");
    let state = temp.path().join("state");
    for p in [&src, &gate, &native] {
        fs::create_dir_all(p).unwrap();
    }
    git(&src, &["init", "-b", "main"]);
    fs::write(
        src.join(".gitattributes"),
        "tracked filter=kar-clone-gate\n",
    )
    .unwrap();
    fs::write(src.join("tracked"), "same blob").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "seed"]);
    let cfg = temp.path().join("gitconfig");
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .replace('\'', "'\\''");
    git(
        &src,
        &[
            "config",
            "--file",
            cfg.to_str().unwrap(),
            "filter.kar-clone-gate.smudge",
            &format!("'{exe}' clone-filter"),
        ],
    );
    git(
        &src,
        &[
            "config",
            "--file",
            cfg.to_str().unwrap(),
            "filter.kar-clone-gate.required",
            "true",
        ],
    );
    let env = [
        ("GIT_CONFIG_GLOBAL", cfg.as_os_str()),
        ("KAR_TEST_CLONE_GATE", gate.as_os_str()),
    ];
    let (mut proc, base) = server::serve_with_env(&state, &root, &native, &env).await;
    let c = server::client();
    let jobs = format!("{base}/forge/project-clones");
    let id = Uuid::new_v4();
    let dst = root.join("dest");
    let start = json!({"id":id,"title":"target","directory":dst,"source":src});
    let result = ok(c.post(&jobs).json(&start)).await;
    assert_eq!(result["projectId"], json!(id));
    assert_eq!(result["cwd"], json!(dst));
    for _ in 0..200 {
        if gate.join("active").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(gate.join("active").is_file());
    assert!(dst.is_dir() && !dst.join(".git").exists());
    let live = ok(c.get(&jobs)).await;
    assert_eq!(live[0]["phase"], "running");
    let events = c.get(format!("{jobs}/stream")).send().await.unwrap();
    assert!(
        events.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    drop(events);
    assert_eq!(
        c.post(&jobs).json(&start).send().await.unwrap().status(),
        StatusCode::CONFLICT
    );
    let blocked = c
        .post(format!("{base}/projects/{id}/chats"))
        .json(&json!({"id":Uuid::new_v4(),"runtime":"codex"}))
        .send()
        .await
        .unwrap();
    assert!(!blocked.status().is_success());
    let applied = ok(c.post(format!("{jobs}/{id}/cancel"))).await;
    assert_eq!(applied["applied"], true);
    assert_eq!(ok(c.get(&jobs)).await[0]["phase"], "cancelled");
    assert!(!dst.join(".git").exists());
    assert_eq!(
        ok(c.post(format!("{jobs}/{id}/cancel"))).await["applied"],
        false
    );
    proc.kill().await.unwrap();
    fs::write(gate.join("release"), "go").unwrap();
    let (mut proc, base) = server::serve_with_env(&state, &root, &native, &env).await;
    let jobs = format!("{base}/forge/project-clones");
    assert_eq!(ok(c.get(&jobs)).await[0]["projectId"], json!(id));
    assert_eq!(
        ok(c.post(format!("{jobs}/{id}/retry"))).await["applied"],
        true
    );
    let mut row = Value::Null;
    for _ in 0..160 {
        row = ok(c.get(&jobs)).await[0].clone();
        if row["phase"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(row["phase"], "done", "{row}");
    assert_eq!(row["percent"], 100);
    assert_eq!(
        git(&dst, &["rev-parse", "HEAD"]),
        git(&src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        fs::read_to_string(dst.join("tracked")).unwrap(),
        "same blob"
    );
    assert_eq!(
        ok(c.post(format!("{jobs}/{id}/retry"))).await["applied"],
        false
    );
    ok(c.post(format!("{base}/projects/{id}/chats"))
        .json(&json!({"id":Uuid::new_v4(),"runtime":"codex"})))
    .await;
    // Delete while Git owns a temporary checkout. The in-flight attempt cannot publish afterward.
    fs::remove_file(gate.join("release")).unwrap();
    let _ = fs::remove_file(gate.join("active"));
    let removed = Uuid::new_v4();
    let unpublished = root.join("unpublished");
    let creating = json!({"id":removed,"title":"discard me","directory":unpublished,"source":src});
    ok(c.post(&jobs).json(&creating)).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !gate.join("active").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    fs::write(unpublished.join("user-file"), "keep").unwrap();
    let mutation =
        json!({"type":"project.delete","projectId":removed,"commandId":"remove-cloning"});
    let first = ok(c.post(format!("{base}/projects/mutate")).json(&mutation)).await;
    assert_eq!(first["deleted"], true);
    assert_eq!(
        ok(c.post(format!("{base}/projects/mutate")).json(&mutation)).await,
        first
    );
    let remaining = ok(c.get(&jobs)).await;
    assert!(
        remaining
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["projectId"] != removed.to_string())
    );
    assert_eq!(
        fs::read_to_string(unpublished.join("user-file")).unwrap(),
        "keep"
    );
    assert!(!unpublished.join(".git").exists());
    assert!(
        !c.post(&jobs)
            .json(&creating)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    proc.kill().await.unwrap();
}
