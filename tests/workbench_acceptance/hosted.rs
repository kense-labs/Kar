//! Opt-in macOS run with installed CLIs, a dedicated GitHub repository, and one Kar chat.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::FutureExt;
use std::{io::Write, panic::AssertUnwindSafe};

struct Remote {
    name: String,
    source: u64,
    token: String,
    api: Client,
    intent: Option<Intent>,
}

struct Intent {
    operation: Uuid,
    branch: String,
    base: String,
    head: String,
    number: Option<u64>,
}

impl Remote {
    fn selected() -> Self {
        let name =
            std::env::var("KAR_FORGE_TEST_REPO").expect("select a dedicated GitHub OWNER/REPO");
        let valid = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        assert!(
            name.split_once('/')
                .is_some_and(|(owner, repo)| valid(owner) && valid(repo))
        );
        let source = std::env::var("KAR_FORGE_SOURCE_PR")
            .expect("select an existing, read-only source PR")
            .parse::<u64>()
            .unwrap();
        assert!(source > 0);
        let token = std::env::var("GH_TOKEN").expect("set GH_TOKEN for the dedicated repository");
        assert!(!token.is_empty());
        Self {
            name,
            source,
            token,
            api: Client::builder()
                .user_agent("Kar hosted acceptance")
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(40))
                .build()
                .unwrap(),
            intent: None,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("https://api.github.com/repos/{}/{}", self.name, path)
    }
    async fn get(&self, path: &str) -> Value {
        ok(self.api.get(self.url(path)).bearer_auth(&self.token)).await
    }

    async fn run_pulls(&self, intent: &Intent) -> Value {
        let head = format!("{}:{}", self.name.split_once('/').unwrap().0, intent.branch);
        ok(self
            .api
            .get(self.url("pulls"))
            .bearer_auth(&self.token)
            .query(&[("state", "all"), ("head", &head), ("base", &intent.base)]))
        .await
    }

    async fn close_own_pull(&self) -> Option<Value> {
        let intent = self.intent.as_ref()?;
        let list = match intent.number {
            Some(n) => json!([self.get(&format!("pulls/{n}")).await]),
            None => {
                // A timed-out create can still have been received by GitHub.
                let mut found = json!([]);
                for attempt in 0..4 {
                    if attempt > 0 {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    found = self.run_pulls(intent).await;
                    if !found.as_array().unwrap().is_empty() {
                        break;
                    }
                }
                found
            }
        };
        let marker = format!("<!-- kar-operation:{} -->", intent.operation);
        let matches: Vec<_> = list
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| {
                p["head"]["repo"]["full_name"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&self.name))
                    && p["base"]["repo"]["full_name"]
                        .as_str()
                        .is_some_and(|r| r.eq_ignore_ascii_case(&self.name))
                    && p["base"]["ref"] == intent.base
                    && p["head"]["ref"] == intent.branch
                    && p["head"]["sha"] == intent.head
                    && p["body"].as_str().is_some_and(|b| b.contains(&marker))
            })
            .collect();
        if matches.is_empty() {
            return None;
        }
        assert_eq!(matches.len(), 1, "ambiguous marked pull request");
        let n = matches[0]["number"].as_u64().unwrap();
        assert_ne!(n, self.source);
        let path = format!("pulls/{n}");
        let pull = if matches[0]["state"] == "closed" {
            self.get(&path).await
        } else {
            ok(self
                .api
                .patch(self.url(&path))
                .bearer_auth(&self.token)
                .json(&json!({"state":"closed"})))
            .await
        };
        assert_eq!(pull["state"], "closed");
        Some(pull)
    }

    async fn remote_files(&self, workspace: &Path, commit: &str) -> Value {
        let pinned = self.get(&format!("git/commits/{commit}")).await;
        assert_eq!(pinned["sha"], commit);
        let tree_sha = pinned["tree"]["sha"].as_str().unwrap();
        let tree = self.get(&format!("git/trees/{tree_sha}")).await;
        assert_eq!(tree["sha"], tree_sha);
        assert_eq!(tree["truncated"], false);
        let temp = tempfile::tempdir().unwrap();
        let mut out = serde_json::Map::new();
        for file in ["notes.md"].into_iter().chain(office::DOCUMENTS) {
            let node = tree["tree"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["path"] == file && v["type"] == "blob")
                .expect("remote document missing");
            let sha = node["sha"].as_str().unwrap();
            assert_eq!(git(workspace, &["hash-object", file]), sha);
            let blob = self.get(&format!("git/blobs/{sha}")).await;
            assert_eq!(blob["encoding"], "base64");
            let b64: String = blob["content"].as_str().unwrap().lines().collect();
            let bytes = STANDARD.decode(b64).unwrap();
            assert_eq!(
                bytes,
                fs::read(workspace.join(file)).unwrap(),
                "{file}: remote bytes"
            );
            if file == "notes.md" {
                assert_eq!(bytes, MERGED.as_bytes());
            } else {
                fs::write(temp.path().join(file), bytes).unwrap();
                office::verify(temp.path(), file, "human", "model");
            }
            out.insert(file.into(), json!(sha));
        }
        Value::Object(out)
    }
}

fn programs() -> ([PathBuf; 2], Value) {
    let paths = ["KAR_CODEX_BIN", "KAR_QWEN_BIN"].map(|key| {
        let path = PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("provide {key}")));
        assert!(
            path.is_absolute() && path.is_file(),
            "{key} must point to an installed executable"
        );
        path
    });
    let versions = paths.each_ref().map(|p| {
        let result = Command::new(p).arg("--version").output().unwrap();
        assert!(
            result.status.success(),
            "cannot inspect installed provider version"
        );
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    });
    assert!(versions.iter().all(|v| !v.is_empty()));
    (paths, json!({"codex":versions[0],"qwen":versions[1]}))
}

fn new_file(path: &Path, content: &[u8]) {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();
    file.write_all(content).unwrap();
}

async fn checkout(f: &mut Host, remote: &Remote, c: &Client) -> (String, String, String) {
    fs::create_dir_all(&f.root).unwrap();
    fs::write(&f.config,"[[hosts]]\nhost='github.com'\nkind='github'\napi_base='https://api.github.com'\ntoken_env='GH_TOKEN'\n").unwrap();
    f.start().await;
    let project=ok(c.post(format!("{}/projects/clone",f.base)).json(&json!({"source":format!("https://github.com/{}.git",remote.name),"directory":f.root.join("base"),"name":"Kar acceptance source"}))).await;
    let source = format!("{}/projects/{}", f.base, project["id"].as_str().unwrap());
    let query = format!("{source}/forge/query");
    let identity = ok(c.post(&query).json(&json!({"op":"identity"}))).await;
    assert!(
        identity["repository"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case(&remote.name)
    );
    let base = identity["default_branch"].as_str().unwrap().to_owned();
    let pinned = ok(c
        .post(&query)
        .json(&json!({"op":"pull","number":remote.source})))
    .await;
    let head = pinned["head_sha"].as_str().unwrap().to_owned();
    let url = format!("{source}/forge/pulls/{}/worktree", remote.source);
    let req = json!({"id":f.worktree,"head":head,"directory":f.workspace});
    let tree = ok(c.post(&url).json(&req)).await;
    assert_eq!(tree["start_commit"], head);
    assert_eq!(tree, ok(c.post(&url).json(&req)).await);
    let branch = git(&f.workspace, &["branch", "--show-current"]);
    assert_eq!(
        branch,
        format!("kar/review-{}-{}", remote.source, f.worktree)
    );
    assert_eq!(git(&f.workspace, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&f.workspace, &["status", "--porcelain"]), "");
    for file in ["notes.md", ".kar-preview-port", "webhook-count"]
        .into_iter()
        .chain(office::DOCUMENTS)
    {
        assert!(
            fs::symlink_metadata(f.workspace.join(file))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "source already contains reserved acceptance path {file}"
        );
    }
    let project = ok(c
        .post(format!("{}/projects", f.base))
        .json(&json!({"root":f.workspace,"name":"Kar hosted acceptance"})))
    .await;
    f.project = project["id"].as_str().unwrap().into();
    (base, branch, head)
}

fn answer(turn: &Value) -> String {
    turn["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["role"] == "assistant")
        .filter_map(|i| i["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn observe_begin(feed: &mut HttpChatFeed, replica: &mut ChatReplica, turn: Uuid) {
    loop {
        replica
            .apply(feed.recv(replica.cursor()).await.unwrap())
            .unwrap();
        if replica
            .turns()
            .iter()
            .any(|t| t.id == turn && t.status == kar::chat::TurnStatus::Running)
        {
            return;
        }
    }
}

pub(super) async fn run() {
    let mut remote = Remote::selected();
    let (paths, versions) = programs();
    let mut host = Host::isolated(Some(paths));
    let trial = AssertUnwindSafe(tokio::time::timeout(
        Duration::from_secs(1500),
        chain(&mut host, &mut remote, &versions),
    ))
    .catch_unwind()
    .await;
    // Covers assertion failures, missing HTTP create receipts, and the scenario deadline.
    let closed = remote.close_own_pull().await;
    if host.server.is_some() {
        host.done().await;
    }
    let proof = match trial {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => panic!("hosted combined acceptance timed out"),
        Err(e) => std::panic::resume_unwind(e),
    };
    let closed = closed.expect("no matching Kar-created PR to close");
    assert_eq!(proof["pull"], closed["number"]);
    println!(
        "{}",
        json!({"proof":proof,"closed_pr":closed["html_url"],"closed_state":closed["state"]})
    );
}

fn authenticated(token: &str) -> Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    Client::builder().default_headers(headers).build().unwrap()
}

async fn chain(f: &mut Host, remote: &mut Remote, versions: &Value) -> Value {
    let owner = authenticated(&f.owner_token);
    let public = Client::new();
    let (base, branch, source_head) = checkout(f, remote, &owner).await;
    let grant = ok(owner.post(format!("{}/access/pairings", f.base))).await;
    let device = ok(public
        .post(format!("{}/access/pair", f.base))
        .json(&json!({"credential":grant["credential"],"label":"Kar hosted second client"})))
    .await;
    let second = authenticated(device["token"].as_str().unwrap());
    let scope = f.scope();
    let id = Uuid::new_v4();
    let opened = ok(owner
        .post(format!("{scope}/chats"))
        .json(&json!({"id":id,"runtime":"codex"})))
    .await;
    assert_eq!(opened["info"]["id"], id.to_string());
    assert_eq!(opened["info"]["worktree_id"], f.worktree.to_string());
    let codex = opened["info"]["native_id"].as_str().unwrap();
    let link = Uuid::new_v4();
    ok(owner
        .post(format!("{scope}/forge/links"))
        .json(&json!({"id":link,"chat":id,"kind":"pull","number":remote.source})))
    .await;
    let chat = format!("{scope}/chats/{id}");
    let mut feed_a = HttpChatFeed::with_client(&chat, owner.clone()).unwrap();
    let mut feed_b = HttpChatFeed::with_client(&chat, second.clone()).unwrap();
    let mut left = ChatReplica::from_snapshot(feed_a.snapshot().await.unwrap());
    let mut right = ChatReplica::from_snapshot(feed_b.snapshot().await.unwrap());
    let fact = format!("amber-{}", Uuid::new_v4());
    let first = Uuid::new_v4();
    let prompt = format!("Remember this marker exactly: {fact}. Reply OK without tools.");
    let (_, _, first_ack) = tokio::join!(
        observe_begin(&mut feed_a, &mut left, first),
        observe_begin(&mut feed_b, &mut right, first),
        turn(&owner, &chat, first, &prompt)
    );
    assert_eq!(first_ack["id"], first.to_string());
    let secret = Uuid::new_v4().to_string();
    let proof_name = format!("kar-proof-{}.txt", Uuid::new_v4());
    new_file(&f.workspace.join(&proof_name), secret.as_bytes());
    let tool_id = Uuid::new_v4();
    let tool=turn(&owner,&chat,tool_id,&format!("Use a read-only tool to read {proof_name} from the current workspace. Reply with its exact contents.")).await;
    assert!(answer(&tool).contains(&secret));
    assert!(
        tool["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "tool" && i.to_string().contains(&secret))
    );
    tokio::join!(
        catchup(&chat, &owner, &mut left),
        catchup(&chat, &second, &mut right)
    );
    assert_eq!(left.turns(), right.turns());
    let before_disconnect = right.cursor();
    drop(feed_b);
    let handoff = json!({"id":Uuid::new_v4(),"runtime":"qwen"});
    let switched = ok(owner.post(format!("{chat}/handoffs")).json(&handoff)).await;
    assert_eq!(switched["status"], "ready", "{}", switched["error"]);
    let qwen = switched["target"]["native_id"].as_str().unwrap();
    let target_id = Uuid::new_v4();
    let qwen_fact = format!("silver-{}", Uuid::new_v4());
    let prompt = format!(
        "Without tools, repeat the amber-prefixed marker and file value you received from Codex. Remember {qwen_fact}."
    );
    let qwen_answer = turn(&second, &chat, target_id, &prompt).await;
    assert!(
        qwen_answer["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] != "tool")
    );
    assert!(answer(&qwen_answer).contains(&fact) && answer(&qwen_answer).contains(&secret));
    assert_eq!(qwen_answer, turn(&owner, &chat, target_id, &prompt).await);
    let messages = wire(&f.native);
    let sent = messages
        .iter()
        .find(|v| v["uuid"] == target_id.to_string())
        .expect("Qwen wire input UUID");
    assert_eq!(sent["session"], qwen);
    assert!(
        sent["text"].as_str().unwrap().contains(&secret)
            && sent["text"].as_str().unwrap().contains(&fact)
    );

    new_file(&f.workspace.join("notes.md"), BASE_TEXT.as_bytes());
    let (pty, preview) = preview::open(&owner, &scope, &f.workspace).await;
    let md = coedit(f, &owner, &second).await;
    let fresh = tempfile::tempdir().unwrap();
    office::seed(fresh.path());
    for file in office::DOCUMENTS {
        new_file(
            &f.workspace.join(file),
            &fs::read(fresh.path().join(file)).unwrap(),
        );
    }
    let documents = office::submit(&owner, &second, &scope, &f.workspace).await;
    let mut files = office::DOCUMENTS.to_vec();
    files.push("notes.md");
    files.sort_unstable();
    ok(owner
        .post(format!("{scope}/git"))
        .json(&json!({"op":"stage","paths":files})))
    .await;
    assert_eq!(
        git(&f.workspace, &["diff", "--cached", "--name-only"])
            .lines()
            .collect::<Vec<_>>(),
        files
    );
    ok(owner.post(format!("{scope}/git")).json(&json!({"op":"commit","message":format!("Verify Kar document collaboration {}",f.worktree)}))).await;
    let head = git(&f.workspace, &["rev-parse", "HEAD"]);
    assert_eq!(git(&f.workspace, &["rev-parse", "HEAD^"]), source_head);
    assert_eq!(git(&f.workspace, &["branch", "--show-current"]), branch);
    let operation = Uuid::new_v4();
    eprintln!(
        "Kar hosted acceptance {} branch {branch} operation {operation}",
        remote.name
    );
    let expected_origin = format!("https://github.com/{}.git", remote.name);
    assert!(
        git(
            &f.workspace,
            &["remote", "get-url", "--push", "--all", "origin"]
        )
        .eq_ignore_ascii_case(&expected_origin),
        "push destination differs from the selected repository"
    );
    let reference = remote
        .api
        .get(remote.url(&format!("git/ref/heads/{branch}")))
        .bearer_auth(&remote.token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        reference.status(),
        reqwest::StatusCode::NOT_FOUND,
        "new Kar branch already exists or could not be checked"
    );
    ok(owner
        .post(format!("{scope}/git"))
        .json(&json!({"op":"push"})))
    .await;
    let remote_ref = remote.get(&format!("git/ref/heads/{branch}")).await;
    assert_eq!(remote_ref["object"]["sha"], head);
    let blobs = remote.remote_files(&f.workspace, &head).await;
    let create = json!({"id":operation,"op":"create_pull","chat":id,"head":branch,"base":base,"title":format!("Kar macOS acceptance {}",f.worktree),"body":format!("Disposable acceptance run starting at PR #{}.",remote.source)});
    remote.intent = Some(Intent {
        operation,
        branch: branch.clone(),
        base: base.clone(),
        head: head.clone(),
        number: None,
    });
    let pr = ok(owner
        .post(format!("{scope}/forge/operations"))
        .json(&create))
    .await;
    let n = pr["number"].as_u64().unwrap();
    assert_ne!(n, remote.source);
    remote.intent.as_mut().unwrap().number = Some(n);
    assert_eq!(pr["head_sha"], head);
    let pull = remote.get(&format!("pulls/{n}")).await;
    assert_eq!(pull["head"]["ref"], branch);
    assert_eq!(pull["head"]["sha"], head);
    assert!(
        pull["body"]
            .as_str()
            .unwrap()
            .contains(&format!("<!-- kar-operation:{operation} -->"))
    );
    let review = format!("{scope}/forge/pulls/{n}");
    ok(owner
        .put(format!("{review}/reviewed"))
        .json(&json!({"head":head,"path":"notes.md","reviewed":true})))
    .await;
    let patch = ok(second.get(format!("{review}/diff?path=notes.md"))).await;
    for needle in ["+date=human", "+author=assistant"] {
        assert!(patch["patch"].as_str().unwrap().contains(needle));
    }
    let w = Uuid::new_v4();
    let event = serde_json::to_vec(
        &json!({"id":format!("kar-webhook-{id}"),"input":{"chat":id,"head":head,"pull":n}}),
    )
    .unwrap();
    let run = workflow(f, &owner, w, &event).await;

    let share = ok(owner
        .post(format!("{chat}/shares"))
        .json(&json!({"ttl_seconds":3600})))
    .await;
    let sid = share["id"].as_str().unwrap();
    let token = ok(public
        .post(format!("{}/guest/{sid}/redeem", f.base))
        .json(&json!({"credential":share["credential"],"name":"hosted reviewer"})))
    .await;
    let guest = authenticated(token["token"].as_str().unwrap());
    let text = format!(
        "Kar guest acceptance {}. Reply OK without tools.",
        Uuid::new_v4()
    );
    let draft = json!({"request_id":Uuid::new_v4(),"text":text});
    let propose = |base: &str| {
        guest
            .post(format!("{base}/guest/{sid}/proposals"))
            .json(&draft)
    };
    let submitted = ok(propose(&f.base)).await;
    assert_eq!(submitted, ok(propose(&f.base)).await);
    let guest_id = submitted["id"].as_str().unwrap();
    let accept = |base: &str| {
        owner
            .put(format!("{base}/access/shares/{sid}/proposals/{guest_id}"))
            .json(&json!({"accept":true}))
    };
    let mut receipt = ok(accept(&f.base)).await;
    while receipt["status"] == "sending" {
        assert_ne!(
            receipt["delivery"], "uncertain",
            "guest input was not confirmed"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        receipt = ok(accept(&f.base)).await;
    }
    assert_eq!(receipt["status"], "accepted");
    // Wait for the actual target model to finish before restarting this successful-turn case.
    loop {
        let snap = feed_a.snapshot().await.unwrap();
        if let Some(t) =
            snap.history.turns.iter().find(|t| {
                t.id.to_string() == guest_id && t.status != kar::chat::TurnStatus::Running
            })
        {
            assert_eq!(t.status, kar::chat::TurnStatus::Completed);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    catchup(&chat, &owner, &mut left).await;
    assert_eq!(right.cursor(), before_disconnect);
    f.restart().await;
    let scope = f.scope();
    let chat = format!("{scope}/chats/{id}");
    tokio::join!(
        catchup(&chat, &owner, &mut left),
        catchup(&chat, &second, &mut right)
    );
    assert_eq!(left.turns(), right.turns());
    assert_eq!(
        left.turns()
            .iter()
            .map(|t| t.id.to_string())
            .collect::<Vec<_>>(),
        [
            first.to_string(),
            tool_id.to_string(),
            target_id.to_string(),
            guest_id.to_owned()
        ]
    );
    assert_eq!(left.chat().native_id, qwen);
    assert_eq!(left.chat().workspace, f.workspace);
    assert_eq!(left.chat().worktree_id, Some(f.worktree));
    assert_eq!(left.handoffs().len(), 1);
    assert_eq!(left.handoffs()[0].input, Some(target_id));
    assert_eq!(ok(accept(&f.base)).await, receipt);
    assert_eq!(ok(propose(&f.base)).await, receipt);
    let view = ok(guest.get(format!("{}/guest/{sid}/snapshot", f.base))).await;
    let guest_turns = view["turns"].as_array().unwrap();
    assert_eq!(
        guest_turns
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        left.turns()
            .iter()
            .map(|t| t.id.to_string())
            .collect::<Vec<_>>()
    );
    let seen = guest_turns.iter().find(|t| t["id"] == guest_id).unwrap();
    assert!(
        seen["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "user" && m["text"] == text)
    );
    assert_eq!(
        guest
            .get(format!("{scope}/file?path=notes.md"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(hook(f, &public, w, &event).await, run);
    let runs = ok(second.get(format!("{scope}/workflows/{w}/runs"))).await;
    assert_eq!(runs.as_array().unwrap().len(), 1);
    assert_eq!(runs[0]["id"], run["run_id"]);
    assert_eq!(fs::read(f.workspace.join("webhook-count")).unwrap(), b"H");
    assert_eq!(
        ok(owner
            .post(format!("{scope}/forge/operations"))
            .json(&create))
        .await,
        pr
    );
    let pulls = remote.run_pulls(remote.intent.as_ref().unwrap()).await;
    assert_eq!(pulls.as_array().unwrap().len(), 1);
    assert_eq!(pulls[0]["number"], n);
    let reviewed = ok(second.get(format!("{scope}/forge/pulls/{n}/diff"))).await;
    assert_eq!(reviewed["reviewed"], json!(["notes.md"]));
    assert_eq!(reviewed["pull"]["head_sha"], head);
    let links = ok(second.get(format!("{scope}/forge/links"))).await;
    for number in [remote.source, n] {
        assert!(
            links
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["binding"]["kind"] == "pull"
                    && v["binding"]["number"] == number
                    && v["binding"]["chat"] == id.to_string())
        );
    }
    let route = format!("{scope}/documents/operations?path=notes.md");
    let conflict = second
        .post(&route)
        .json(&md["request"])
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
    assert_eq!(conflict.json::<Value>().await.unwrap(), md["receipt"]);
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
    let resolve=ok(owner.post(&route).json(&json!({"author":"owner","operation_id":"md-resolve","base_revision":2,"action":{"type":"resolve","conflict":md["receipt"]["conflict_id"],"decision":{"type":"keep_current"}}}))).await;
    assert_eq!(resolve["resolves"], md["receipt"]["conflict_id"]);
    let office = office::finish(&owner, &scope, &f.workspace, &documents).await;
    assert_eq!(remote.remote_files(&f.workspace, &head).await, blobs);
    let current = remote.get(&format!("pulls/{}", remote.source)).await;
    assert_eq!(
        current["head"]["sha"], source_head,
        "the read-only source PR changed during the run"
    );
    let terminal = ok(second.get(format!("{scope}/terminals/{pty}"))).await;
    assert_eq!(terminal["status"]["state"], "interrupted");
    let previews = ok(second.get(format!("{scope}/previews"))).await;
    assert!(
        previews
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != preview["id"])
    );
    ok(owner.delete(format!("{scope}/terminals/{pty}"))).await;
    let continued = Uuid::new_v4();
    let reply=turn(&second,&chat,continued,"Without tools, repeat the silver-prefixed marker you were asked to remember before the service restart.").await;
    assert!(answer(&reply).contains(&qwen_fact));
    tokio::join!(
        catchup(&chat, &owner, &mut left),
        catchup(&chat, &second, &mut right)
    );
    assert_eq!(left.turns(), right.turns());
    assert_eq!(left.turns().len(), 5);
    assert_eq!(left.turns().last().unwrap().id, continued);
    let calls = wire(&f.native);
    let native_codex: Vec<_> = calls
        .iter()
        .filter(|v| v["runtime"] == "codex" && v["cwd"] == f.workspace.to_str().unwrap())
        .collect();
    assert_eq!(native_codex.len(), 2);
    assert!(native_codex.iter().all(|v| v["session"] == codex));
    let native_docs: Vec<_> = calls
        .iter()
        .filter(|v| {
            v["runtime"] == "codex"
                && v["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("kar-acceptance-edit-notes"))
        })
        .collect();
    assert_eq!(native_docs.len(), 1);
    assert_ne!(native_docs[0]["cwd"], f.workspace.to_str().unwrap());
    let native_qwen: Vec<_> = calls.iter().filter(|v| v["runtime"] == "qwen").collect();
    assert_eq!(native_qwen.len(), 3);
    for (call, input) in native_qwen.iter().zip([
        target_id.to_string(),
        guest_id.to_owned(),
        continued.to_string(),
    ]) {
        assert_eq!(call["uuid"], input);
        assert_eq!(call["session"], qwen);
        assert_eq!(call["cwd"], f.workspace.to_str().unwrap());
    }
    json!({
        "mode": "installed Codex and Qwen; GitHub; loopback workbench and webhook",
        "versions":versions, "repository":remote.name, "source_pr":remote.source, "source_head":source_head,
        "project":f.project, "worktree":f.worktree, "branch":branch, "head":head, "blobs":blobs,
        "pull":n, "pr_url":pr["url"], "operation":operation, "chat":id, "paired_device":device["id"],
        "codex":codex, "qwen":qwen, "handoff":handoff["id"], "turns":[first,tool_id,target_id,guest_id,continued],
        "created_remote_pulls":pulls.as_array().unwrap().len(),
        "native_chat_inputs":{"codex":native_codex.len(),"qwen":native_qwen.len()}, "native_document_inputs":native_docs.len(),
        "cursor_at_disconnect":before_disconnect, "cursor":right.cursor(), "pty":pty, "preview":preview["id"],
        "markdown_revision":md["native"]["revision"], "markdown_conflict":md["receipt"]["conflict_id"],
        "markdown_resolution":resolve["resolves"], "office":office, "reviewed":reviewed["reviewed"],
        "workflow":w, "run":run["run_id"], "webhook_runs":runs.as_array().unwrap().len(),
        "script_executions":fs::read(f.workspace.join("webhook-count")).unwrap().len(),
        "guest_share":sid, "guest_proposal":guest_id
    })
}
