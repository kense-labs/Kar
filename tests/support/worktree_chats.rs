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
