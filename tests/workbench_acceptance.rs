//! One macOS workspace and one restarted Kar service. Git, PTY, preview, the
//! document engines and workflow scripts are real; native CLIs/forge are protocol peers.
#[path = "workbench_acceptance/forge.rs"]
mod forge;
#[path = "workbench_acceptance/office.rs"]
mod office;
#[path = "workbench_acceptance/preview.rs"]
mod preview;
#[path = "support/handoff_runtime.rs"]
mod runtime;
#[allow(dead_code)] // Shared fixture also offers a launch without environment overrides.
#[path = "support/workbench_server.rs"]
mod server;
use kar::chat::{ChatReplica, HttpChatFeed};
use reqwest::Client;
use serde_json::{Value, json};
use server::json as ok;
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use uuid::Uuid;
const BASE_TEXT: &str = "date=original\nsection=middle\nauthor=original\n";
const MERGED: &str = "date=human\nsection=middle\nauthor=assistant\n";

fn git(cwd: &Path, args: &[&str]) -> String {
    let r = Command::new("git")
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .unwrap();
    assert!(
        r.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    String::from_utf8(r.stdout).unwrap().trim_end().into()
}
struct Host {
    dir: tempfile::TempDir,
    root: PathBuf,
    native: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
    server: Option<tokio::process::Child>,
    base: String,
    project: String,
    worktree: Uuid,
}
impl Host {
    async fn new() -> (Self, forge::Peer) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("roots");
        let native = base.join("native");
        let config = base.join("forge.toml");
        fs::create_dir_all(&native).unwrap();
        for name in ["codex", "qwen"] {
            let dst = native.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            let exe = std::env::current_exe().unwrap();
            fs::hard_link(&exe, &dst)
                .or_else(|_| fs::copy(&exe, dst).map(|_| ()))
                .unwrap();
        }
        let checkout = root.join("base");
        let bare = base.join("origin.git");
        forge::seed(&checkout, &bare);
        let api = forge::serve(&bare).await;
        fs::write(&config,format!("[[hosts]]\nhost='review.example'\nkind='github'\napi_base='http://{}'\ntoken_env='KAR_FIXTURE_FORGE_TOKEN'\n",api.addr)).unwrap();
        let mut f = Self {
            dir,
            root: root.clone(),
            native,
            config,
            workspace: root.join("review"),
            server: None,
            base: String::new(),
            project: String::new(),
            worktree: Uuid::new_v4(),
        };
        f.start().await;
        let client = server::client();
        let project = ok(client
            .post(format!("{}/projects", f.base))
            .json(&json!({"root":checkout,"name":"seed"})))
        .await;
        let source = format!(
            "{}/projects/{}/forge/pulls/1",
            f.base,
            project["id"].as_str().unwrap()
        );
        let head = git(&bare, &["rev-parse", "refs/heads/feature"]);
        let id = f.worktree;
        let req = json!({"id":id,"head":head,"directory":f.workspace});
        let managed = ok(client.post(format!("{source}/worktree")).json(&req)).await;
        assert_eq!(managed["id"], id.to_string());
        assert_eq!(managed["start_commit"], head);
        assert_eq!(
            managed,
            ok(client.post(format!("{source}/worktree")).json(&req)).await
        );
        assert_eq!(git(&f.workspace, &["rev-parse", "HEAD"]), head);
        let project = ok(client
            .post(format!("{}/projects", f.base))
            .json(&json!({"root":f.workspace,"name":"review"})))
        .await;
        f.project = project["id"].as_str().unwrap().into();
        (f, api)
    }
    fn scope(&self) -> String {
        format!("{}/projects/{}", self.base, self.project)
    }
    fn bare(&self) -> PathBuf {
        self.dir.path().join("origin.git")
    }
    async fn start(&mut self) {
        let codex = self
            .native
            .join(format!("codex{}", std::env::consts::EXE_SUFFIX));
        let qwen = self
            .native
            .join(format!("qwen{}", std::env::consts::EXE_SUFFIX));
        let vars = [
            ("KAR_CODEX_BIN", codex.as_os_str()),
            ("KAR_QWEN_BIN", qwen.as_os_str()),
            ("KAR_HANDOFF_FIXTURE", self.native.as_os_str()),
            ("KAR_HANDOFF_REAL_CODEX", OsStr::new("")),
            ("KAR_HANDOFF_REAL_QWEN", OsStr::new("")),
            ("KAR_FORGE_CONFIG", self.config.as_os_str()),
            ("KAR_FIXTURE_FORGE_TOKEN", OsStr::new("fixture-only")),
            ("GIT_ALLOW_PROTOCOL", OsStr::new("file")),
        ];
        let (server, ready) = server::serve_with_env(
            &self.dir.path().join("state"),
            &self.root,
            &self.native,
            &vars,
        )
        .await;
        self.server = Some(server);
        self.base = ready;
    }
    async fn restart(&mut self) {
        self.server.take().unwrap().kill().await.unwrap();
        self.start().await;
    }
    async fn done(&mut self) {
        self.server.take().unwrap().kill().await.unwrap();
    }
}
async fn catchup(root: &str, client: &Client, replica: &mut ChatReplica) {
    let mut feed = HttpChatFeed::with_client(root, client.clone()).unwrap();
    let end = feed.snapshot().await.unwrap().cursor;
    while replica.cursor() != end {
        let event = feed.recv(replica.cursor()).await.unwrap();
        replica.apply(event).unwrap();
    }
}
async fn turn(c: &Client, chat: &str, id: Uuid, text: &str) -> Value {
    let t = ok(c
        .post(format!("{chat}/messages"))
        .json(&json!({"id":id,"text":text})))
    .await;
    assert_eq!(t["status"], "completed");
    t
}
fn wire(dir: &Path) -> Vec<Value> {
    fs::read_to_string(dir.join("inputs"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
async fn coedit(f: &Host, owner: &Client, ai: &Client) -> Value {
    let scope = f.scope();
    let file = format!("{scope}/file?path=notes.md");
    let docs = format!("{scope}/documents");
    let doc = |cmd: &str| format!("{docs}/{cmd}?path=notes.md");
    assert_eq!(ok(owner.get(&file)).await["revision"], 0);
    fs::write(f.native.join("hold-document"), []).unwrap();
    let request = json!({"operation_id":"md-native","base_revision":0,"runtime":"codex","instructions":"kar-acceptance-edit-notes: replace author=original with author=assistant; preserve the rest."});
    let native = tokio::spawn(ai.post(doc("native")).json(&request).send());
    tokio::time::timeout(Duration::from_secs(15), async {
        while !f.native.join("entered-document").is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("native editor did not reach the private candidate");
    let human=ok(owner.put(&file).json(&json!({"text":BASE_TEXT.replace("date=original","date=human"),"base_revision":0,"operation_id":"md-human"}))).await;
    assert_eq!(human["revision"], 1);
    fs::remove_file(f.native.join("hold-document")).unwrap();
    let response = native.await.unwrap().unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    let applied: Value = response.json().await.unwrap();
    assert_eq!(applied["revision"], 2);
    assert_eq!(
        fs::read_to_string(f.workspace.join("notes.md")).unwrap(),
        MERGED
    );
    assert_eq!(ok(ai.post(doc("native")).json(&request)).await, applied);
    let staged = ok(ai
        .post(doc("proposals"))
        .body(BASE_TEXT.replace("date=original", "date=conflicting")))
    .await;
    let conflict_req = json!({"author":"assistant","operation_id":"md-conflict","base_revision":0,"action":{"type":"save","sha256":staged["sha256"]}});
    let r = ai
        .post(doc("operations"))
        .json(&conflict_req)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let conflict: Value = r.json().await.unwrap();
    assert_eq!(conflict["revision"], 2);
    assert_eq!(
        fs::read_to_string(f.workspace.join("notes.md")).unwrap(),
        MERGED
    );
    json!({"request":conflict_req,"sha":staged["sha256"],"receipt":conflict,"native":applied})
}
fn signed(body: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use std::fmt::Write;
    let mut mac = Hmac::<sha2_mac::Sha256>::new_from_slice(b"kar-acceptance-webhook").unwrap();
    mac.update(body);
    let mut out = "sha256=".to_owned();
    for b in mac.finalize().into_bytes() {
        write!(out, "{b:02x}").unwrap();
    }
    out
}
async fn hook(f: &Host, c: &Client, w: Uuid, body: &[u8]) -> Value {
    ok(
        c.post(format!("{}/hooks/workflows/{}/{w}", f.base, f.project))
            .header("x-kar-signature", signed(body))
            .body(body.to_vec()),
    )
    .await
}
async fn workflow(f: &Host, c: &Client, w: Uuid, body: &[u8]) -> Value {
    ok(c.put(format!("{}/workflows/{w}",f.scope())).json(&json!({"name":"cross-module","enabled":true,"schedule":{"type":"webhook"},"steps":[{"type":"script","command":"printf H >> webhook-count"}],"webhook_secret":"kar-acceptance-webhook"}))).await;
    let receipt = hook(f, &Client::new(), w, body).await;
    assert_eq!(hook(f, &Client::new(), w, body).await, receipt);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let run = ok(c.get(format!(
                "{}/workflow-runs/{}",
                f.scope(),
                receipt["run_id"].as_str().unwrap()
            )))
            .await;
            match run["status"].as_str() {
                Some("completed") => break,
                Some("failed" | "reconcile" | "cancelled") => panic!("workflow: {run}"),
                _ => tokio::time::sleep(Duration::from_millis(40)).await,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(fs::read(f.workspace.join("webhook-count")).unwrap(), b"H");
    receipt
}
async fn scenario() {
    let (mut host, peer) = Host::new().await;
    let scope = host.scope();
    let (owner, second, guest) = (server::client(), server::client(), Client::new());
    let id = Uuid::new_v4();
    let issue_req = json!({"id":id,"runtime":"codex"});
    let opened = ok(owner
        .post(format!("{scope}/forge/issues/7/chat"))
        .json(&issue_req))
    .await;
    assert_eq!(opened["chat"]["info"]["id"], id.to_string());
    assert_eq!(
        opened["chat"]["info"]["workspace"],
        host.workspace.to_str().unwrap()
    );
    let chat = format!("{scope}/chats/{id}");
    let start = |c: &Client| HttpChatFeed::with_client(&chat, c.clone()).unwrap();
    let mut left = ChatReplica::from_snapshot(start(&owner).snapshot().await.unwrap());
    let mut right = ChatReplica::from_snapshot(start(&second).snapshot().await.unwrap());
    let first = Uuid::new_v4();
    fs::write(host.native.join("hold-turn"), []).unwrap();
    let pending = tokio::spawn(
        owner
            .post(format!("{chat}/messages"))
            .json(&json!({"id":first,"text":"fact-one and fact-two"}))
            .send(),
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        while !host.native.join("entered-turn").exists() {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    tokio::join!(
        catchup(&chat, &owner, &mut left),
        catchup(&chat, &second, &mut right)
    );
    for view in [&left, &right] {
        assert_eq!(view.turns()[0].id, first);
        assert_eq!(view.turns()[0].status, kar::chat::TurnStatus::Running);
    }
    fs::remove_file(host.native.join("hold-turn")).unwrap();
    assert_eq!(
        pending
            .await
            .unwrap()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["status"],
        "completed"
    );
    turn(&owner, &chat, Uuid::new_v4(), "collect evidence").await;
    catchup(&chat, &owner, &mut left).await;
    catchup(&chat, &second, &mut right).await;
    assert_eq!(left.turns(), right.turns());
    let before = right.cursor();
    let handoff = json!({"id":Uuid::new_v4(),"runtime":"qwen"});
    let switched = ok(owner.post(format!("{chat}/handoffs")).json(&handoff)).await;
    assert_eq!(switched["status"], "ready");
    let bturn = Uuid::new_v4();
    let b = turn(&second, &chat, bturn, "continue from source").await;
    assert!(b.to_string().contains("received both facts and proof"));
    assert_eq!(b, turn(&owner, &chat, bturn, "continue from source").await);
    let (pty, grant) = preview::open(&owner, &scope, &host.workspace).await;
    let md = coedit(&host, &owner, &second).await;
    office::seed(&host.workspace);
    let documents = office::submit(&owner, &second, &scope, &host.workspace).await;
    let stage = json!({"op":"stage","paths":["notes.md","plan.docx","plan.xlsx","plan.pptx"]});
    for op in [
        stage,
        json!({"op":"commit","message":"Merge owner and assistant document edits"}),
        json!({"op":"push"}),
    ] {
        ok(owner.post(format!("{scope}/git")).json(&op)).await;
    }
    let branch = git(&host.workspace, &["branch", "--show-current"]);
    let head = git(&host.workspace, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(
            &host.bare(),
            &["rev-parse", &format!("refs/heads/{branch}")]
        ),
        head
    );
    assert_eq!(
        git(&host.bare(), &["show", &format!("{head}:notes.md")]),
        MERGED.trim_end()
    );
    assert!(
        !fs::read_to_string(host.workspace.join("notes.md"))
            .unwrap()
            .contains("conflicting")
    );
    let create = json!({"id":Uuid::new_v4(),"op":"create_pull","chat":id,"head":branch,"base":"main","title":"Shared edits","body":"Issue 7"});
    let pr = ok(owner
        .post(format!("{scope}/forge/operations"))
        .json(&create))
    .await;
    assert_eq!(pr["number"], 2);
    assert_eq!(pr["head_sha"], head);
    let review = format!("{scope}/forge/pulls/2");
    let mark = json!({"head":head,"path":"notes.md","reviewed":true});
    ok(owner.put(format!("{review}/reviewed")).json(&mark)).await;
    let diff = ok(second.get(format!("{review}/diff?path=notes.md"))).await;
    assert!(
        diff["patch"]
            .as_str()
            .unwrap()
            .contains("+author=assistant")
    );
    let w = Uuid::new_v4();
    let event =
        serde_json::to_vec(&json!({"id":"release-7","input":{"chat":id,"head":head}})).unwrap();
    let run = workflow(&host, &owner, w, &event).await;
    let share = ok(owner
        .post(format!("{chat}/shares"))
        .json(&json!({"ttl_seconds":600})))
    .await;
    let sid = share["id"].as_str().unwrap();
    let guests = |base: &str| format!("{base}/guest/{sid}");
    let token = ok(guest
        .post(format!("{}/redeem", guests(&host.base)))
        .json(&json!({"credential":share["credential"],"name":"reviewer"})))
    .await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let draft = json!({"request_id":Uuid::new_v4(),"text":"guest release approval"});
    let propose = |base: &str| {
        guest
            .post(format!("{}/proposals", guests(base)))
            .bearer_auth(&token)
            .json(&draft)
    };
    let guest_proposal = ok(propose(&host.base)).await;
    assert_eq!(ok(propose(&host.base)).await, guest_proposal);
    assert!(
        guest
            .get(format!("{}/projects", host.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    let accept = |base: &str| {
        owner
            .put(format!(
                "{base}/access/shares/{sid}/proposals/{}",
                guest_proposal["id"].as_str().unwrap()
            ))
            .json(&json!({"accept":true}))
    };
    let admitted = ok(accept(&host.base)).await;
    assert_eq!(admitted["status"], "accepted");
    catchup(&chat, &owner, &mut left).await;
    assert_eq!(right.cursor(), before);
    let committed_office = office::DOCUMENTS.map(|file| {
        let disk = git(&host.workspace, &["hash-object", file]);
        assert_eq!(
            git(&host.bare(), &["rev-parse", &format!("{head}:{file}")]),
            disk
        );
        (file, disk)
    });
    host.restart().await;
    let scope = host.scope();
    let chat = format!("{scope}/chats/{id}");
    catchup(&chat, &owner, &mut left).await;
    catchup(&chat, &second, &mut right).await;
    assert_eq!(left.turns(), right.turns());
    assert_eq!(left.turns().len(), 4);
    assert_eq!(left.turns()[0].id, first);
    assert_eq!(left.turns()[2].id, bturn);
    assert_eq!(left.handoffs().len(), 1);
    assert_eq!(left.handoffs()[0].input, Some(bturn));
    assert_eq!(
        left.chat().native_id,
        switched["target"]["native_id"].as_str().unwrap()
    );
    assert_eq!(ok(accept(&host.base)).await, admitted);
    assert_eq!(ok(propose(&host.base)).await, admitted);
    let guest_snapshot = ok(guest
        .get(format!("{}/snapshot", guests(&host.base)))
        .bearer_auth(&token))
    .await;
    assert!(guest_snapshot.get("workspace").is_none());
    assert!(
        guest
            .get(format!("{scope}/file?path=notes.md"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    assert_eq!(
        ok(owner
            .post(format!("{scope}/forge/operations"))
            .json(&create))
        .await,
        pr
    );
    let links = ok(second.get(format!("{scope}/forge/links"))).await;
    assert_eq!(links.as_array().unwrap().len(), 2);
    let review = ok(second.get(format!("{scope}/forge/pulls/2/diff"))).await;
    assert_eq!(review["reviewed"], json!(["notes.md"]));
    assert_eq!(
        peer.created.lock().unwrap().len(),
        2,
        "one seeded PR and one actual creation"
    );
    assert_eq!(hook(&host, &Client::new(), w, &event).await, run);
    assert_eq!(
        ok(owner.get(format!("{scope}/workflows/{w}/runs")))
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fs::read(host.workspace.join("webhook-count")).unwrap(),
        b"H"
    );
    let old_pty = ok(second.get(format!("{scope}/terminals/{pty}"))).await;
    assert_eq!(old_pty["status"]["state"], "interrupted");
    let route = format!("{scope}/documents/operations?path=notes.md");
    let again = second
        .post(&route)
        .json(&md["request"])
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);
    assert_eq!(again.json::<Value>().await.unwrap(), md["receipt"]);
    assert_eq!(
        fs::read_to_string(host.workspace.join("notes.md")).unwrap(),
        MERGED
    );
    let candidate = second
        .get(format!(
            "{scope}/documents/proposals/{}?path=notes.md",
            md["sha"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        candidate,
        BASE_TEXT.replace("date=original", "date=conflicting")
    );
    let resolution=ok(owner.post(&route).json(&json!({"author":"owner","operation_id":"md-resolve","base_revision":2,"action":{"type":"resolve","conflict":md["receipt"]["conflict_id"],"decision":{"type":"keep_current"}}}))).await;
    let office = office::finish(&owner, &scope, &host.workspace, &documents).await;
    assert_eq!(
        git(&host.bare(), &["show", &format!("{head}:notes.md")]),
        MERGED.trim_end()
    );
    for (file, blob) in committed_office {
        assert_eq!(
            git(&host.bare(), &["rev-parse", &format!("{head}:{file}")]),
            blob
        );
    }
    let continued = Uuid::new_v4();
    turn(
        &second,
        &chat,
        continued,
        "continue after restart in this workspace",
    )
    .await;
    tokio::join!(
        catchup(&chat, &owner, &mut left),
        catchup(&chat, &second, &mut right)
    );
    assert_eq!(left.turns(), right.turns());
    assert_eq!(left.turns().last().unwrap().id, continued);
    assert_eq!(left.chat().workspace, host.workspace);
    assert_eq!(left.chat().worktree_id, Some(host.worktree));
    let projects = ok(second.get(format!("{}/projects", host.base))).await;
    assert!(
        projects
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == host.project && p["root"] == host.workspace.to_str().unwrap())
    );
    let calls = wire(&host.native);
    assert_eq!(
        calls
            .iter()
            .filter(|v| v["text"] == "collect evidence")
            .count(),
        1
    );
    assert_eq!(
        git(
            &host.bare(),
            &["rev-parse", &format!("refs/heads/{branch}")]
        ),
        head
    );
    assert_eq!(
        git(
            &host.bare(),
            &["rev-list", "--count", &format!("main..{branch}")]
        ),
        "2"
    );
    let last = calls.last().unwrap();
    assert_eq!(last["session"], switched["target"]["native_id"]);
    assert_eq!(last["cwd"], host.workspace.to_str().unwrap());
    assert_eq!(
        calls
            .iter()
            .filter(|x| x["text"]
                .as_str()
                .is_some_and(|s| s.contains("kar-acceptance-edit-notes")))
            .count(),
        1
    );
    assert_eq!(calls.iter().filter(|x| x["runtime"] == "qwen").count(), 3);
    let proof = json!({"mode":"protocol peers and local bare Git remote","project":host.project,"worktree":host.worktree,"branch":branch,"source_pull":1,"issue":7,"chat":id,"native_source":opened["chat"]["info"]["native_id"],"native_target":left.chat().native_id,"turns":left.turns().len(),"cursor":right.cursor(),"pty":pty,"preview_grant":grant["id"],"markdown_revision":md["native"]["revision"],"markdown_resolution":resolution["resolves"],"office":office,"head":head,"pull":pr["number"],"reviewed":review["reviewed"],"source_tool_calls":1,"native_document_edits":1,"native_target_inputs":3,"created_remote_pulls":1,"workflow":w,"run":run["run_id"],"script_executions":1,"accepted_proposal":guest_proposal["id"]});
    let listings = ok(second.get(format!("{scope}/previews"))).await;
    assert!(
        listings
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != grant["id"])
    );
    ok(owner.delete(format!("{scope}/terminals/{pty}"))).await;
    println!("{proof}");
    host.done().await;
}
fn main() {
    if runtime::child() {
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("preview-server") {
        return preview::service();
    }
    if !cfg!(target_os = "macos") {
        println!("The combined PTY and native document acceptance requires macOS.");
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(180), scenario())
            .await
            .expect("cross-module acceptance timed out");
    });
}
