//! Opt-in. Deploys an isolated Kar and makes writes only after confirming the remote scope.
use super::*;
use reqwest::Client;
use std::collections::HashSet;

struct Target {
    client: Client,
    forge: String,
    chat: Uuid,
}
impl Target {
    async fn read(&self, op: Value) -> Value {
        ok(self.client.post(format!("{}/query", self.forge)).json(&op)).await
    }
    fn write(&self, mut op: Value) -> (Value, reqwest::RequestBuilder) {
        op["id"] = json!(Uuid::new_v4());
        op["chat"] = json!(self.chat);
        let req = self
            .client
            .post(format!("{}/operations", self.forge))
            .json(&op);
        (op, req)
    }
    async fn confirmed(&self, op: Value) -> Value {
        let (op, req) = self.write(op);
        let receipt = ok(req).await;
        assert_eq!(
            ok(self
                .client
                .post(format!("{}/operations", self.forge))
                .json(&op))
            .await,
            receipt
        );
        receipt
    }
    async fn refused(&self, op: Value, reason: &str) {
        let (_, req) = self.write(op);
        let reply = req.send().await.unwrap();
        assert!(!reply.status().is_success());
        assert_eq!(reply.json::<Value>().await.unwrap()["kind"], reason);
    }
    async fn threads(&self, n: u64) -> Vec<Value> {
        let mut threads = vec![];
        let mut cursor = None::<String>;
        let mut seen = HashSet::new();
        loop {
            let page = self
                .read(json!({"op":"review_threads","number":n,"cursor":cursor}))
                .await;
            for row in page["threads"].as_array().unwrap() {
                let mut row = row.clone();
                let mut more = row["nextCursor"].as_str().map(str::to_owned);
                let mut cursors = HashSet::new();
                while let Some(after) = more.take() {
                    assert!(cursors.insert(after.clone()) && cursors.len() <= 100);
                    let extra=self.read(json!({"op":"thread_comments","number":n,"thread_id":row["id"],"cursor":after})).await;
                    row["comments"]
                        .as_array_mut()
                        .unwrap()
                        .extend(extra["comments"].as_array().unwrap().iter().cloned());
                    more = extra["nextCursor"].as_str().map(str::to_owned);
                }
                threads.push(row);
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            let Some(after) = &cursor else { break };
            assert!(seen.insert(after.clone()) && seen.len() <= 100);
        }
        threads
    }
}
fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("set {key} explicitly"))
}

async fn issue(target: &Target, id: Uuid, expect: &str) {
    let body = format!("Kar acceptance issue {id}");
    let title = format!("Kar issue {id}");
    let create = json!({"op":"create_issue","body":body,"title":title});
    if expect != "success" {
        return target.refused(create, expect).await;
    }
    let state = env("KAR_FORGE_ACTION_CLOSE_STATE");
    let created = target.confirmed(create).await;
    let n = created["number"].as_u64().unwrap();
    let detail = target.read(json!({"op":"issue","number":n})).await;
    assert_eq!(detail["title"], title);
    assert!(detail["body"].as_str().unwrap().contains(&body));
    let note = format!("Kar comment {id}");
    let posted = target
        .confirmed(json!({"op":"comment_issue","number":n,"body":note}))
        .await;
    let mut matches = vec![];
    let mut cursor = None::<String>;
    let mut seen = HashSet::new();
    loop {
        let page = target
            .read(json!({"op":"issue_comments","number":n,"cursor":cursor}))
            .await;
        matches.extend(
            page["comments"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["body"] == note)
                .cloned(),
        );
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        let Some(after) = &cursor else { break };
        assert!(seen.insert(after.clone()) && seen.len() <= 100);
    }
    assert_eq!(matches.len(), 1);
    let comment = &matches[0];
    assert!(comment["author"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(comment["id"].as_str().is_some_and(|s| !s.is_empty()));
    if !posted["comment_id"].is_null() {
        let id = posted["comment_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| posted["comment_id"].to_string());
        assert_eq!(comment["id"], id);
    }
    assert_eq!(
        target.read(json!({"op":"issue","number":n})).await["title"],
        title
    );
    let result = target
        .confirmed(json!({"op":"set_issue_state","number":n,"state":state}))
        .await;
    println!(
        "{}",
        json!({"scope":env("KAR_FORGE_ACTION_SCOPE"),"issue":created,"comment":comment,"state":result["state"]})
    );
}

async fn review(target: &Target, id: Uuid, expect: &str, driver: &str) {
    let n = env("KAR_FORGE_ACTION_PR").parse::<u64>().unwrap();
    assert!(n > 0);
    let marker = format!("Kar inline review {id}");
    let mut submit = json!({"op":"submit_review","number":n,"verdict":"comment","body":format!("Kar review {id}")});
    if expect != "success" {
        return target.refused(submit, expect).await;
    }
    let supplied = if driver == "gitcafe" {
        Some(env("KAR_FORGE_ACTION_THREAD"))
    } else {
        None
    };
    if supplied.is_none() {
        let line = env("KAR_FORGE_ACTION_LINE").parse::<u32>().unwrap();
        assert!(line > 0);
        submit["comments"] = json!([{"path":env("KAR_FORGE_ACTION_PATH"),"position":{"kind":"added","newLine":line},"body":marker}]);
    }
    let receipt = target.confirmed(submit).await;
    let remote = target.threads(n).await;
    let thread = if let Some(key) = supplied {
        remote
            .iter()
            .find(|r| r["id"] == key)
            .expect("preselected dedicated GitCafe thread")
    } else {
        let matches: Vec<_> = remote
            .iter()
            .filter(|r| {
                r["comments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x["body"] == marker && x["author"].is_string())
            })
            .collect();
        assert_eq!(matches.len(), 1);
        matches[0]
    };
    assert_eq!(
        thread["resolved"], false,
        "select an initially open, dedicated acceptance thread"
    );
    let tid = &thread["id"];
    let body = format!("Kar thread reply {id}");
    let reply = json!({"op":"reply_to_thread","number":n,"thread_id":tid,"body":body});
    if driver == "forgejo" {
        target.refused(reply, "unsupported").await;
        target
            .refused(
                json!({"op":"set_thread_resolution","number":n,"thread_id":tid,"resolved":true}),
                "unsupported",
            )
            .await;
        println!(
            "{}",
            json!({"scope":env("KAR_FORGE_ACTION_SCOPE"),"pull":n,"review":receipt,"thread":tid,"reply":"unsupported","resolve":"unsupported"})
        );
        return;
    }
    let response = target.confirmed(reply).await;
    for resolved in [true, false] {
        target.confirmed(json!({"op":"set_thread_resolution","number":n,"thread_id":tid,"resolved":resolved})).await;
        let threads = target.threads(n).await;
        let t = threads.iter().find(|t| t["id"] == *tid).unwrap();
        assert_eq!(t["resolved"], resolved);
        let comments: Vec<_> = t["comments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["body"] == body && r["author"].is_string() && r["id"].is_string())
            .collect();
        assert_eq!(comments.len(), 1);
        println!(
            "{}",
            json!({"scope":env("KAR_FORGE_ACTION_SCOPE"),"pull":n,"review":receipt,"thread":tid,"reply":response,"confirmed":comments[0],"resolved":resolved})
        );
    }
}

pub(super) async fn run(issues: bool) {
    let driver = env("KAR_FORGE_ACTION_DRIVER");
    assert!(
        [
            "github",
            "gitlab",
            "forgejo",
            "azure-devops",
            "cursor-origin",
            "gitcafe",
            "tanstack-source",
            "bitbucket",
            "linear"
        ]
        .contains(&driver.as_str())
    );
    let expected = env("KAR_FORGE_ACTION_EXPECT");
    assert!(["success", "permission", "unsupported"].contains(&expected.as_str()));
    let scope = env("KAR_FORGE_ACTION_SCOPE");
    let tmp = tempfile::tempdir().unwrap();
    let roots = tmp.path().join("projects");
    let repo = roots.join("target");
    let db = tmp.path().join("native");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir(&db).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(
        &repo,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "acceptance repository scope",
        ],
    );
    git(
        &repo,
        &["remote", "add", "origin", &env("KAR_FORGE_ACTION_REMOTE")],
    );
    let config = tmp.path().join("forge.toml");
    let template = std::env::var_os("KAR_FORGE_ACTION_CONFIG")
        .map(|p| fs::read_to_string(p).unwrap())
        .unwrap_or_default();
    fs::write(&config, "").unwrap();
    let (mut proc, addr) = server::serve_with_env(
        &tmp.path().join("state"),
        &roots,
        &db,
        &[("KAR_FORGE_CONFIG", config.as_os_str())],
    )
    .await;
    let client = server::client();
    let project = ok(client
        .post(format!("{addr}/projects"))
        .json(&json!({"root":repo,"name":"dedicated hosted acceptance"})))
    .await;
    let p = project["id"].as_str().unwrap();
    fs::write(config, template.replace("PROJECT_UUID", p)).unwrap();
    let url = format!("{addr}/projects/{p}");
    let target = Target {
        client,
        forge: format!("{url}/forge"),
        chat: Uuid::new_v4(),
    };
    let identity = if issues {
        ok(target.client.get(format!("{}/issue-tracker", target.forge))).await
    } else {
        target.read(json!({"op":"identity"})).await
    };
    let host = identity["host"].as_str().unwrap();
    let name = identity["repository"].as_str().unwrap();
    assert_eq!(
        format!("{host}/{name}"),
        scope,
        "queried host and scope must match the selected disposable target"
    );
    assert_eq!(
        identity["kind"], driver,
        "queried driver must match the selected driver"
    );
    ok(target
        .client
        .post(format!("{url}/chats"))
        .json(&json!({"id":target.chat,"runtime":"codex"})))
    .await;
    let id = Uuid::new_v4();
    if issues {
        issue(&target, id, &expected).await
    } else {
        review(&target, id, &expected, &driver).await
    }
    proc.kill().await.unwrap();
}
