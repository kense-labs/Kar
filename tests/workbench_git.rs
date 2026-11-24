//! CLI acceptance against local repositories. The same binary can gate one native Git
//! subprocess to stop the Kar controller on either side of a durable operation.
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use uuid::Uuid;

fn tool(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}
fn wait_file(path: &Path) {
    let start = Instant::now();
    while !path.is_file() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn git_wrapper() -> ! {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let dir = PathBuf::from(std::env::var_os("KAR_GIT_TEST_GATE").unwrap());
    let action = std::env::var_os("KAR_GIT_TEST_ACTION").unwrap();
    let gated = args.windows(2).any(|a| {
        (a[0] == "worktree" && a[1] == action)
            || (action == "reserve" && a[0] == "update-ref" && a[1] == "--stdin")
    });
    let before = std::env::var_os("KAR_GIT_TEST_BEFORE").is_some();
    let run = || {
        Command::new(std::env::var_os("KAR_GIT_TEST_REAL").unwrap())
            .args(&args)
            .status()
            .unwrap()
            .code()
            .unwrap_or(1)
    };
    let code = if gated && before { 0 } else { run() };
    if gated {
        fs::write(dir.join("ready"), []).unwrap();
        wait_file(&dir.join("release"));
        fs::write(dir.join("done"), []).unwrap();
    }
    std::process::exit(code)
}
fn output(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
fn reject(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(!out.status.success(), "unexpected success: {cmd:?}");
    String::from_utf8(out.stderr).unwrap()
}
fn string(o: Output) -> String {
    String::from_utf8(o.stdout).unwrap().trim_end().to_owned()
}
fn json(o: Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap()
}
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    real: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join(tool("git")))
            .find(|p| p.is_file())
            .unwrap()
            .canonicalize()
            .unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::hard_link(
            std::env::current_exe().unwrap(),
            root.join("bin").join(tool("git")),
        )
        .or_else(|_| {
            fs::copy(
                std::env::current_exe().unwrap(),
                root.join("bin").join(tool("git")),
            )
            .map(|_| ())
        })
        .unwrap();
        fs::write(root.join("gitconfig"), "[user]\n name = Kar Test\n email = test@example.invalid\n[init]\n defaultBranch = main\n").unwrap();
        let f = Self {
            _tmp: tmp,
            root,
            real,
        };
        output(f.raw(".").args(["init", "--bare", "origin.git"]));
        output(f.raw(".").args(["clone", "origin.git", "seed"]));
        fs::write(f.p("seed/file"), "base\n").unwrap();
        fs::write(f.p("seed/.gitignore"), "*.ignored\n").unwrap();
        output(f.raw("seed").args(["add", "."]));
        output(f.raw("seed").args(["commit", "-m", "initial"]));
        output(f.raw("seed").args(["push", "origin", "HEAD"]));
        output(f.kar(".").args(["clone", "origin.git", "local"]));
        f
    }
    fn p(&self, v: &str) -> PathBuf {
        self.root.join(v)
    }
    fn env(&self, cmd: &mut Command) {
        cmd.env("GIT_CONFIG_GLOBAL", self.p("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }
    fn raw(&self, dir: &str) -> Command {
        let mut cmd = Command::new(&self.real);
        self.env(&mut cmd);
        cmd.current_dir(self.p(dir));
        cmd
    }
    fn kar(&self, dir: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
        self.env(&mut cmd);
        cmd.current_dir(&self.root)
            .arg("git")
            .arg("--repo")
            .arg(self.p(dir));
        cmd
    }
    fn add(&self, name: &str, id: Uuid) -> Command {
        let mut cmd = self.kar("local");
        cmd.arg("worktree-add")
            .arg(id.to_string())
            .arg(self.p(name))
            .arg(name);
        cmd
    }
    fn remove(&self, id: Uuid) -> Command {
        let mut cmd = self.kar("local");
        cmd.arg("worktree-remove").arg(id.to_string());
        cmd
    }
    fn commit(&self, dir: &str, content: &str) -> String {
        fs::write(self.p(dir).join("file"), content).unwrap();
        output(self.kar(dir).args(["stage", "file"]));
        json(output(self.kar(dir).args(["commit", "-m", content])))
            .as_str()
            .unwrap()
            .into()
    }
    fn interrupt(&self, mut cmd: Command, action: &str, before: bool) {
        let gate = self.p(&Uuid::new_v4().to_string());
        fs::create_dir(&gate).unwrap();
        let mut paths = vec![self.p("bin")];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        cmd.env("PATH", std::env::join_paths(paths).unwrap())
            .env("KAR_GIT_TEST_REAL", &self.real)
            .env("KAR_GIT_TEST_GATE", &gate)
            .env("KAR_GIT_TEST_ACTION", action);
        if before {
            cmd.env("KAR_GIT_TEST_BEFORE", "1");
        }
        let mut child = cmd.stdout(Stdio::null()).spawn().unwrap();
        wait_file(&gate.join("ready"));
        child.kill().unwrap();
        child.wait().unwrap();
        fs::write(gate.join("release"), []).unwrap();
        wait_file(&gate.join("done"));
    }
}

fn remote_and_status() {
    let f = Fixture::new();
    let id = Uuid::new_v4();
    let tree = json(output(&mut f.add("topic", id)));
    assert_eq!(tree["id"], id.to_string());
    let sha = f.commit("topic", "from chat");
    output(f.kar("topic").arg("push"));
    output(
        f.raw(".")
            .args(["clone", "--branch", "topic", "origin.git", "peer"]),
    );
    assert_eq!(
        string(output(f.raw("peer").args(["rev-parse", "HEAD"]))),
        sha
    );
    let tree = |dir| string(output(f.raw(dir).args(["rev-parse", "HEAD^{tree}"])));
    assert_eq!(tree("topic"), tree("peer"));
    assert_eq!(fs::read(f.p("peer/file")).unwrap(), b"from chat");

    let mut watcher = f
        .kar("topic")
        .args(["watch", "--interval-ms", "20"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = watcher.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx
                .send(serde_json::from_str::<Value>(&line).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let next = || rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(next()["head"], sha);
    let remote = f.commit("peer", "remote change");
    output(f.kar("peer").arg("push"));
    fs::write(f.p("topic/file"), "keep my edit").unwrap();
    let dirty = next();
    assert_eq!(dirty["files"][0]["worktree"], "M");
    reject(f.kar("topic").arg("pull"));
    assert_eq!(fs::read(f.p("topic/file")).unwrap(), b"keep my edit");
    output(f.raw("topic").args(["restore", "--", "file"]));
    assert_eq!(json(output(f.kar("topic").arg("pull"))), remote);
    loop {
        if next()["head"] == remote {
            break;
        }
    }
    watcher.kill().unwrap();
    watcher.wait().unwrap();
    reader.join().unwrap();
    assert_eq!(tree("peer"), tree("topic"));
    fs::write(f.p("peer/settings.ignored"), "tracked upstream").unwrap();
    output(f.raw("peer").args(["add", "-f", "settings.ignored"]));
    output(f.kar("peer").args(["commit", "-m", "track settings"]));
    output(f.kar("peer").arg("push"));
    fs::write(f.p("topic/settings.ignored"), "my settings").unwrap();
    reject(f.kar("topic").arg("pull"));
    assert_eq!(
        fs::read(f.p("topic/settings.ignored")).unwrap(),
        b"my settings"
    );
    fs::remove_file(f.p("topic/settings.ignored")).unwrap();
    output(f.kar("topic").arg("pull"));
    let local = f.commit("topic", "local divergence");
    let upstream = f.commit("peer", "remote divergence");
    output(f.kar("peer").arg("push"));
    reject(f.kar("topic").arg("push"));
    reject(f.kar("topic").arg("pull"));
    assert_eq!(json(output(f.kar("topic").arg("status")))["head"], local);
    assert_eq!(
        string(output(
            f.raw("origin.git").args(["rev-parse", "refs/heads/topic"])
        )),
        upstream
    );
    assert_eq!(fs::read(f.p("topic/file")).unwrap(), b"local divergence");
    reject(f.raw("topic").args(["merge", "origin/topic"]));
    let unmerged = json(output(f.kar("topic").arg("status")));
    assert_eq!(unmerged["files"][0]["index"], "U");
    assert_eq!(unmerged["files"][0]["worktree"], "U");
    assert!(reject(&mut f.remove(id)).contains("cleanup refused"));
}

fn ownership_and_cleanup() {
    let f = Fixture::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    output(&mut f.add("alpha", a));
    let old = kar::workbench_git::Repository::open(&f.p("alpha")).unwrap();
    output(&mut f.add("beta", b));
    assert_eq!(json(output(&mut f.add("alpha", a)))["branch"], "alpha");
    reject(&mut f.add("beta", a));
    reject(&mut f.add("alpha", Uuid::new_v4()));
    for (name, content) in [("alpha", "A"), ("beta", "B")] {
        fs::write(f.p(name).join("file"), content).unwrap();
    }
    output(f.kar("alpha").args(["stage", "file"]));
    assert_eq!(string(output(f.raw("alpha").args(["show", ":file"]))), "A");
    assert_eq!(
        string(output(f.raw("beta").args(["show", ":file"]))),
        "base"
    );
    assert!(reject(&mut f.remove(a)).contains("cleanup refused"));
    assert!(reject(&mut f.remove(b)).contains("cleanup refused"));
    output(
        f.raw("alpha")
            .args(["restore", "--staged", "--worktree", "file"]),
    );
    for name in ["keep.ignored", "untracked"] {
        fs::write(f.p("alpha").join(name), "irreplaceable").unwrap();
        assert!(reject(&mut f.remove(a)).contains("cleanup refused"));
        assert_eq!(fs::read(f.p("alpha").join(name)).unwrap(), b"irreplaceable");
        fs::remove_file(f.p("alpha").join(name)).unwrap();
    }
    output(
        f.raw("alpha")
            .args(["update-index", "--assume-unchanged", "file"]),
    );
    fs::write(f.p("alpha/file"), "hidden").unwrap();
    assert!(reject(&mut f.remove(a)).contains("cleanup refused"));
    output(
        f.raw("alpha")
            .args(["update-index", "--no-assume-unchanged", "file"]),
    );
    output(f.raw("alpha").args(["restore", "file"]));
    output(&mut f.remove(a));
    output(&mut f.remove(a));
    assert!(!f.p("alpha").exists());
    assert!(f.p("beta").is_dir());
    assert_eq!(fs::read(f.p("beta/file")).unwrap(), b"B");
    output(
        f.raw("local")
            .args(["show-ref", "--verify", "refs/heads/alpha"]),
    );
    fs::create_dir(f.p("alpha")).unwrap();
    fs::write(f.p("alpha/user"), "mine").unwrap();
    reject(&mut f.add("alpha", a));
    output(&mut f.remove(a));
    assert!(f.p("alpha/user").exists());
    assert!(old.stage(&["user".into()]).is_err());
    #[cfg(unix)]
    {
        fs::write(f.p("beta/:(glob)*.rs"), "literal").unwrap();
        output(f.kar("beta").args(["stage", ":(glob)*.rs"]));
        assert_eq!(
            string(output(f.raw("beta").args(["show", ":./:(glob)*.rs"]))),
            "literal"
        );
        fs::write(f.p("beta/name\nwith tab\t"), "odd").unwrap();
        assert!(
            json(output(f.kar("beta").arg("status")))["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path"] == "name\nwith tab\t")
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let odd = OsString::from_vec(vec![b'x', 0xff, b'\n']);
        fs::write(f.p("beta").join(odd), "odd").unwrap();
        let status = json(output(f.kar("beta").arg("status")));
        assert!(
            status["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path"]["base64"].is_string())
        );
    }
}

fn branch_actions() {
    let f = Fixture::new();
    fs::create_dir(f.p("plain")).unwrap();
    fs::write(f.p("plain/keep"), "existing").unwrap();
    assert_eq!(json(output(f.kar("plain").arg("refs")))["is_repo"], false);
    for _ in 0..2 {
        assert_eq!(
            json(output(f.kar("plain").arg("init")))["head"],
            Value::Null
        );
        assert!(f.p("plain/.git/HEAD").is_file());
    }
    assert_eq!(fs::read(f.p("plain/keep")).unwrap(), b"existing");
    assert_eq!(json(output(f.kar("plain").arg("refs")))["total_count"], 0);
    let branch = |args: &[&str]| json(output(f.kar("local").args(args)))["branch"].clone();
    assert_eq!(branch(&["create-branch", "topic-one"]), "main");
    assert_eq!(
        branch(&["create-branch", "topic-two", "--switch"]),
        "topic-two"
    );
    f.commit("local", "topic contents");
    assert_eq!(branch(&["switch-branch", "main"]), "main");
    fs::write(f.p("local/file"), "local changes").unwrap();
    reject(f.kar("local").args(["switch-branch", "topic-two"]));
    assert_eq!(fs::read(f.p("local/file")).unwrap(), b"local changes");
    assert_eq!(branch(&["status"]), "main");
    output(f.raw("local").args(["restore", "file"]));
    for name in ["topic-one", "topic-three", "four"] {
        output(
            f.raw("local")
                .args(["update-ref", &format!("refs/remotes/origin/{name}"), "HEAD"]),
        );
    }
    let page = |extra: &[&str]| {
        json(output(
            f.kar("local")
                .args(["refs", "--query", "ToPiC"])
                .args(extra),
        ))
    };
    let first = page(&["--limit", "2"]);
    assert_eq!(first["total_count"], 3);
    assert_eq!(first["next_cursor"], 2);
    let last = page(&["--limit", "2", "--cursor", "2", "--refresh"]);
    assert_eq!(last["refs"][0]["name"], "origin/topic-three");
    assert_eq!(last["next_cursor"], Value::Null);
    assert_eq!(
        page(&["--kind", "remote", "--include-matching-remote-refs"])["total_count"],
        2
    );
    assert_eq!(
        branch(&["switch-branch", "origin/topic-three"]),
        "topic-three"
    );
    assert_eq!(branch(&["switch-branch", "four"]), "four");
    assert_eq!(
        string(output(f.raw("local").args([
            "rev-parse",
            "--abbrev-ref",
            "@{upstream}"
        ]))),
        "origin/four"
    );
    let id = Uuid::new_v4();
    let created = json(output(&mut f.add("managed", id)));
    output(f.kar("managed").args(["switch-branch", "topic-one"]));
    assert_eq!(json(output(&mut f.add("managed", id))), created);
    reject(f.kar("local").args(["switch-branch", "topic-one"]));
    output(&mut f.remove(id));
    output(
        f.raw("local")
            .args(["show-ref", "--verify", "refs/heads/topic-one"]),
    );
}

fn interruption() {
    let f = Fixture::new();
    for (name, before) in [("early", true), ("late", false)] {
        let id = Uuid::new_v4();
        f.interrupt(f.add(name, id), "add", before);
        reject(&mut f.add(name, Uuid::new_v4()));
        let a = json(output(&mut f.add(name, id)));
        let b = json(output(&mut f.add(name, id)));
        assert_eq!(a, b);
        assert_eq!(fs::read(f.p(name).join("file")).unwrap(), b"base\n");
        f.interrupt(f.remove(id), "remove", before);
        if before {
            fs::write(f.p(name).join("rescued"), "written during restart").unwrap();
            reject(&mut f.remove(id));
            assert!(f.p(name).join("rescued").exists());
            fs::remove_file(f.p(name).join("rescued")).unwrap();
        }
        output(&mut f.remove(id));
        assert!(!f.p(name).exists());
        output(
            f.raw("local")
                .args(["show-ref", "--verify", &format!("refs/heads/{name}")]),
        );
    }
    let id = Uuid::new_v4();
    f.interrupt(f.add("external", id), "reserve", true);
    output(f.raw("local").args(["branch", "external", "HEAD"]));
    reject(&mut f.add("external", id));
    assert!(!f.p("external").exists());
    output(
        f.raw("local")
            .args(["show-ref", "--verify", "refs/heads/external"]),
    );
}
fn main() {
    if std::env::var_os("KAR_GIT_TEST_GATE").is_some() {
        git_wrapper();
    }
    remote_and_status();
    branch_actions();
    println!("remote verification, watch, conflicting pull and non-fast-forward push passed");
    ownership_and_cleanup();
    println!("isolated indices, paths, ownership and protected cleanup passed");
    interruption();
    println!("controller termination before/after checkout creation and deletion passed");
}
