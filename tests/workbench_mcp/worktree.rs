use super::*;
use std::path::{Path, PathBuf};
use tokio::net::UdpSocket;

async fn bearer(observer: &UdpSocket) -> String {
    let mut bytes = [0u8; 192];
    let n = tokio::time::timeout(Duration::from_secs(15), observer.recv(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes[..n].to_vec()).unwrap()
}

pub(super) struct Moved {
    chat: Uuid,
    command: Uuid,
    continuation: String,
    workspace: PathBuf,
    source: Value,
    native: Value,
}
impl Moved {
    pub(super) async fn check(&self, host: &Client, url: &str, project: &str) {
        let snap = server::json(host.get(format!(
            "{url}/projects/{project}/chats/{}/snapshot",
            self.chat
        )))
        .await;
        let h = &snap["history"];
        assert_eq!(h["chat"]["id"], json!(self.chat));
        assert_eq!(h["chat"]["workspace"], json!(self.workspace));
        assert_eq!(h["chat"]["native_id"], self.native);
        let records = h["handoffs"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["request"]["id"], json!(self.command));
        assert_eq!(records[0]["from"]["native_id"], self.source);
        assert_eq!(records[0]["target"]["native_id"], self.native);
        assert_eq!(
            h["turns"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| t["id"] == self.continuation)
                .count(),
            1
        );
    }
}

pub(super) async fn exercise(
    url: &str,
    host: &Client,
    observer: &UdpSocket,
    project: &str,
    checkouts: &Path,
    db: &Path,
) -> Moved {
    let chat = Uuid::new_v4();
    let base = format!("{url}/projects/{project}/chats");
    let opened = server::json(host.post(&base).json(&json!({"id":chat,"runtime":"codex"}))).await;
    let source = opened["info"]["native_id"].clone();
    let root = Path::new(opened["info"]["workspace"].as_str().unwrap());
    // Use a real file-transport upstream at a newer commit while keeping the issue tracker remote intact.
    let upstream = db.join("branch-source");
    super::issues::git(
        root,
        &["clone", "-q", "--no-local", ".", upstream.to_str().unwrap()],
    );
    fs::write(upstream.join("remote-base"), "remote commit").unwrap();
    super::issues::git(&upstream, &["add", "remote-base"]);
    super::issues::git(
        &upstream,
        &[
            "-c",
            "user.name=Kar",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-qm",
            "upstream",
        ],
    );
    super::issues::git(
        root,
        &["remote", "add", "kar-upstream", upstream.to_str().unwrap()],
    );
    super::issues::git(root, &["config", "branch.main.remote", "kar-upstream"]);
    super::issues::git(root, &["config", "branch.main.merge", "refs/heads/main"]);
    let old = bearer(observer).await;
    let client = Client::new();
    let original = open(&client, url, &old).await;
    let call = |name: &str, args: Value| {
        rpc(
            &client,
            url,
            &old,
            "tools/call",
            json!({"name":name,"arguments":args}),
        )
        .header("mcp-session-id", &original)
    };
    let status = server::json(call("kar_worktree_status", json!({}))).await;
    assert_eq!(status["result"]["structuredContent"]["attached"], false);
    let list = server::json(call("kar_worktree_list", json!({"kind":"local"}))).await;
    assert!(
        list["result"]["structuredContent"]["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "main" && r["current"] == true)
    );
    assert_eq!(
        server::json(call(
            "kar_worktree_list",
            json!({"thread_id":Uuid::new_v4()})
        ))
        .await["result"]["isError"],
        true
    );
    let id = Uuid::new_v4();
    let draft =
        json!({"command_id":id,"branch":"kar-move","continuation_prompt":"continue in checkout"});
    let handoff = || call("kar_worktree_handoff", draft.clone());
    assert_eq!(server::json(handoff()).await["result"]["isError"], true);
    server::json(
        host.put(format!("{base}/{chat}/model-tools/permissions"))
            .json(&json!({"mode":"execute","worktree_directory":checkouts})),
    )
    .await;
    assert_eq!(
        server::json(handoff().header("kar-mode-limit", "auto/plan")).await["result"]["isError"],
        true
    );

    // The model is still taking the old turn when its own scoped MCP credential requests the move.
    fs::write(db.join("hold-parent-turn"), []).unwrap();
    let send = host
        .post(format!("{base}/{chat}/messages"))
        .json(&json!({"id":id,"text":"shared-next"}));
    let active = tokio::spawn(server::json(send));
    while !db.join("parent-turn-running").is_file() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queue = format!("{base}/{chat}/queue");
    let before = Uuid::new_v4();
    let after = Uuid::new_v4();
    server::json(
        host.post(&queue)
            .json(&json!({"id":before,"text":"admitted before move"})),
    )
    .await;
    let first = server::json(handoff()).await;
    let record = &first["result"]["structuredContent"]["handoff"];
    assert_eq!(record["status"], "pending");
    let continuation = record["continuation_id"].as_str().unwrap().to_owned();
    assert_ne!(continuation, id.to_string());
    server::json(
        host.post(&queue)
            .json(&json!({"id":after,"text":"admitted after move"})),
    )
    .await;
    let q = server::json(host.get(&queue)).await;
    assert_eq!(
        q["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].clone())
            .collect::<Vec<_>>(),
        vec![json!(before), json!(continuation), json!(after)]
    );
    assert_eq!(
        &server::json(handoff()).await["result"]["structuredContent"]["handoff"],
        record
    );
    let different = json!({"command_id":id,"branch":"different"});
    assert_eq!(
        server::json(call("kar_worktree_handoff", different)).await["result"]["isError"],
        true
    );
    fs::remove_file(db.join("hold-parent-turn")).unwrap();
    assert_eq!(active.await.unwrap()["id"], json!(id));

    let current = bearer(observer).await;
    assert_ne!(current, old);
    // The native program receives config during launch, just before its lease becomes active.
    let history = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if let Ok(res) = host.get(format!("{base}/{chat}/snapshot")).send().await
                && res.status().is_success()
            {
                let snap = res.json::<Value>().await.unwrap();
                let h = &snap["history"];
                if h["turns"].as_array().is_some_and(|ts| {
                    ts.iter()
                        .any(|t| t["id"] == json!(after) && t["finished_ms"].is_number())
                }) {
                    break h.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let sid = open(&client, url, &current).await;
    let invoke = |name: &str, args: Value| {
        rpc(
            &client,
            url,
            &current,
            "tools/call",
            json!({"name":name,"arguments":args}),
        )
        .header("mcp-session-id", &sid)
    };
    let moved_status = server::json(invoke("kar_worktree_status", json!({}))).await;
    let checkout = &moved_status["result"]["structuredContent"];
    let workspace = checkouts.join(format!("kar-{id}"));
    assert_eq!(checkout["workspace"], json!(workspace));
    assert_eq!(checkout["attached"], true);
    assert_eq!(checkout["branch"], "kar-move");
    assert_eq!(checkout["handoffs"][0]["status"], "attached");
    assert_eq!(
        checkout["handoffs"][0]["remote_base"],
        "refs/remotes/kar-upstream/main"
    );
    assert!(workspace.join("remote-base").is_file());
    assert!(!root.join("remote-base").is_file());
    let again = server::json(invoke("kar_worktree_handoff", draft)).await;
    assert_eq!(
        again["result"]["structuredContent"]["handoff"],
        checkout["handoffs"][0]
    );
    assert_eq!(
        rpc(&client, url, &old, "ping", json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let native = history["chat"]["native_id"].clone();
    assert_ne!(source, native);
    let calls: Vec<Value> = fs::read_to_string(db.join("calls"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let delivered: Vec<_> = calls.iter().filter(|v| v["id"] == native).collect();
    assert_eq!(delivered.len(), 3);
    assert_eq!(delivered[0]["cwd"], json!(workspace));
    let text = delivered[0]["text"].as_str().unwrap();
    assert!(text.contains("shared-next") && text.contains("admitted before move"));
    assert_eq!(delivered[1]["text"], "continue in checkout");
    assert_eq!(delivered[2]["text"], "admitted after move");
    let result = Moved {
        chat,
        command: id,
        source,
        native,
        workspace,
        continuation,
    };
    result.check(host, url, project).await;
    result
}

pub(super) struct Interrupted {
    chat: Uuid,
    input: Uuid,
    continuation: Value,
    original: Value,
    cwd: Value,
}
impl Interrupted {
    pub(super) async fn prepare(
        host: &Client,
        url: &str,
        project: &str,
        observer: &UdpSocket,
        checkouts: &Path,
        db: &Path,
    ) -> Self {
        let chat = Uuid::new_v4();
        let base = format!("{url}/projects/{project}/chats");
        let opened =
            server::json(host.post(&base).json(&json!({"id":chat,"runtime":"codex"}))).await;
        let original = opened["info"]["native_id"].clone();
        let cwd = opened["info"]["workspace"].clone();
        let token = bearer(observer).await;
        let client = Client::new();
        let sid = open(&client, url, &token).await;
        server::json(
            host.put(format!("{base}/{chat}/model-tools/permissions"))
                .json(&json!({"mode":"execute","worktree_directory":checkouts})),
        )
        .await;
        let input = Uuid::new_v4();
        server::json(
            host.post(format!("{base}/{chat}/queue"))
                .json(&json!({"id":input,"text":"hold without reply"})),
        )
        .await;
        while !fs::read_to_string(db.join("calls"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .any(|v| v["id"] == original && v["text"] == "hold without reply")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let value=server::json(rpc(&client,url,&token,"tools/call",json!({"name":"kar_worktree_handoff","arguments":{"command_id":Uuid::new_v4(),"branch":"kar-uncertain","continuation_prompt":"only in new checkout"}})).header("mcp-session-id",&sid)).await;
        let row = &value["result"]["structuredContent"]["handoff"];
        assert_eq!(row["status"], "pending");
        Self {
            chat,
            input,
            continuation: row["continuation_id"].clone(),
            original,
            cwd,
        }
    }
    pub(super) async fn check(self, host: &Client, url: &str, project: &str, db: &Path) {
        let base = format!("{url}/projects/{project}/chats/{}", self.chat);
        // The source provider has no native receipt. The new workspace must not receive a retry.
        tokio::time::timeout(Duration::from_secs(35), async {
            loop {
                let q = server::json(host.get(format!("{base}/queue"))).await;
                let entries = q["inputs"].as_array().unwrap();
                if entries
                    .iter()
                    .any(|v| v["id"] == self.continuation && v["status"] == "withdrawn")
                {
                    assert!(
                        entries
                            .iter()
                            .any(|v| v["id"] == json!(self.input) && v["status"] == "uncertain")
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = server::json(host.get(format!("{base}/snapshot"))).await;
        let info = &snapshot["history"]["chat"];
        assert_eq!(info["native_id"], self.original);
        assert_eq!(info["workspace"], self.cwd);
        assert!(
            snapshot["history"]["handoffs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let calls = fs::read_to_string(db.join("calls")).unwrap();
        assert_eq!(
            calls
                .lines()
                .filter(|s| s.contains("hold without reply"))
                .count(),
            1
        );
        assert!(!calls.contains("only in new checkout"));
    }
}
