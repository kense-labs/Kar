use super::{cli, success};
use kar::workbench_git::Repository;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
};
use uuid::Uuid;

pub(super) fn run() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["main", "elsewhere", "native"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=Kar Test",
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
        String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
    };
    let main = base.join("main");
    git(&main, &["init", "-b", "main"]);
    git(&main, &["config", "user.name", "Kar Test"]);
    git(&main, &["config", "user.email", "test@example.invalid"]);
    git(&main, &["config", "commit.gpgSign", "false"]);
    fs::write(main.join("shared"), "base").unwrap();
    git(&main, &["add", "shared"]);
    git(&main, &["commit", "-m", "initial"]);
    let repo = Repository::open(&main).unwrap();
    let make = |name: &str| {
        let tree = repo
            .create_worktree(Uuid::new_v4(), &base.join(name), name, "HEAD")
            .unwrap();
        let h = success(
            cli(&base)
                .args(["new", "codex", "--bin"])
                .arg(std::env::current_exe().unwrap())
                .arg("--cwd")
                .arg(&tree.path)
                .output()
                .unwrap(),
        );
        assert_eq!(h["chat"]["worktree_id"], tree.id.to_string());
        (
            tree,
            h["chat"]["id"].as_str().unwrap().to_owned(),
            h["chat"]["native_id"].as_str().unwrap().to_owned(),
        )
    };
    let (a, ai, an) = make("a");
    let (b, bi, bn) = make("b");
    let run = |args: &[&str]| success(cli(&base).args(args).output().unwrap());
    let (first, _) = std::thread::scope(|s| {
        let left = s.spawn(|| run(&["resume", "--chat", &ai, "shared-A"]));
        let right = run(&["resume", "--chat", &bi, "shared-B"]);
        (left.join().unwrap(), right)
    });
    let first = first["id"].as_str().unwrap();
    for (tree, content, id, native) in [(&a, "shared-A", &ai, &an), (&b, "shared-B", &bi, &bn)] {
        let history = run(&["history", "--chat", id]);
        assert_eq!(history["chat"]["native_id"], native.as_str());
        assert_eq!(history["turns"].as_array().unwrap().len(), 1);
        assert_eq!(
            fs::read(tree.path.join("shared")).unwrap(),
            content.as_bytes()
        );
        assert_eq!(git(&tree.path, &["show", ":shared"]), "base");
        Repository::open(&tree.path)
            .unwrap()
            .stage(&["shared".into()])
            .unwrap();
        Repository::open(&tree.path)
            .unwrap()
            .commit(content)
            .unwrap();
    }
    run(&["resume", "--chat", &ai, "shared-next"]);
    Repository::open(&a.path)
        .unwrap()
        .stage(&["shared".into()])
        .unwrap();
    let dest = repo
        .create_worktree(Uuid::new_v4(), &base.join("forked"), "forked", "HEAD")
        .unwrap();
    let fork = |path: &Path| {
        let mut cmd = cli(&base);
        cmd.args(["fork", first, "--chat", &ai, "--worktree"])
            .arg(path);
        cmd
    };
    let sentinel = dest.path.join("keep");
    fs::write(&sentinel, "mine").unwrap();
    let refused = fork(&dest.path).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("local changes"));
    assert_eq!(fs::read(&sentinel).unwrap(), b"mine");
    fs::remove_file(sentinel).unwrap();
    let refused = fork(&dest.path)
        .env("FIXTURE_FORK_WRONG_CWD", "1")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("did not confirm"));
    assert_eq!(fs::read(dest.path.join("shared")).unwrap(), b"base");

    let mut host = fork(&dest.path)
        .env("FIXTURE_PAUSE_BRANCH", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let db = base.join("native");
    let published = db.join("awaiting-publication");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !published.is_file() {
        assert!(host.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    host.kill().unwrap();
    host.wait().unwrap();
    let issued = fs::read(db.join("forks")).unwrap();
    let origin = run(&["history", "--chat", &ai]);
    assert_eq!(origin["pending_fork"]["turn"], first);
    assert_eq!(fs::read(a.path.join("shared")).unwrap(), b"shared-next");
    assert_eq!(fs::read(dest.path.join("shared")).unwrap(), b"shared-A");
    assert_eq!(git(&dest.path, &["show", ":shared"]), "base");
    let wrong = fork(&b.path).output().unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("original worktree"));
    run(&[
        "resume",
        "--chat",
        &ai,
        "parent continued while child was pending",
    ]);
    // Even without the destination argument a fresh Kar process resumes the staged fork.
    let forked = run(&["fork", first, "--chat", &ai]);
    let fork_id = forked["chat"]["id"].as_str().unwrap();
    assert_eq!(fs::read(db.join("forks")).unwrap(), issued);
    assert_eq!(
        forked["chat"]["native_id"],
        fs::read_to_string(published).unwrap()
    );
    assert_eq!(forked["chat"]["forked_from"]["native_source"], an);
    assert_eq!(forked["chat"]["worktree_id"], dest.id.to_string());
    assert_eq!(forked["chat"]["workspace"].as_str(), dest.path.to_str());
    assert_eq!(forked["turns"].as_array().unwrap().len(), 1);
    assert_eq!(forked["turns"][0]["id"], first);
    std::thread::scope(|s| {
        let parent = s.spawn(|| run(&["resume", "--chat", &ai, "shared-next"]));
        run(&["resume", "--chat", fork_id, "shared-child"]);
        parent.join().unwrap();
    });
    assert_eq!(fs::read(a.path.join("shared")).unwrap(), b"shared-next");
    assert_eq!(fs::read(dest.path.join("shared")).unwrap(), b"shared-child");
    assert_eq!(git(&a.path, &["show", ":shared"]), "shared-next");
    assert_eq!(git(&dest.path, &["show", ":shared"]), "base");

    run(&["rollback", first, "--chat", &ai]);
    assert_eq!(git(&a.path, &["show", ":shared"]), "shared-next");
    let child = run(&["fork", first, "--chat", &ai]);
    assert_eq!(child["chat"]["worktree_id"], a.id.to_string());
    assert_ne!(child["chat"]["native_id"], an);
    assert_eq!(git(&a.path, &["branch", "--show-current"]), "a");
    assert_eq!(git(&a.path, &["show", ":shared"]), "shared-next");
    assert_eq!(fs::read(a.path.join("shared")).unwrap(), b"shared-A");
    assert_eq!(git(&b.path, &["branch", "--show-current"]), "b");
    assert_eq!(git(&b.path, &["show", ":shared"]), "shared-B");
    assert_eq!(fs::read(b.path.join("shared")).unwrap(), b"shared-B");
    assert_eq!(fs::read(main.join("shared")).unwrap(), b"base");

    // An idle, attached chat pins its checkout. The JSON snapshot is its readiness signal.
    let mut attached = cli(&base)
        .args(["resume", "--chat", &bi])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(attached.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let snapshot: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snapshot["chat"]["native_id"], bn);
    assert!(
        repo.remove_worktree(b.id)
            .unwrap_err()
            .to_string()
            .contains("in use")
    );
    drop(attached.stdin.take());
    assert!(attached.wait().unwrap().success());
    repo.remove_worktree(b.id).unwrap();
    let replacement = repo
        .create_worktree(Uuid::new_v4(), &b.path, "replacement", "HEAD")
        .unwrap();
    let refused = cli(&base)
        .args(["resume", "--chat", &bi, "wrong checkout"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("removed or replaced"));
    assert_eq!(fs::read(replacement.path.join("shared")).unwrap(), b"base");
    println!(
        "two concurrent native chat fixtures, restart binding, per-checkout rollback, fork and retired checkout identity passed"
    );
}
