//! Real local git repositories and a deterministic GitHub REST peer on loopback.
#[path = "support/codex_chat_peer.rs"]
mod codex_chat_peer;
#[path = "workbench_forge/diff_contents.rs"]
mod diff_contents;
#[path = "support/workbench_server.rs"]
mod server;
#[path = "workbench_forge/signals.rs"]
mod signals;
#[path = "workbench_forge/viewed.rs"]
mod viewed;
use axum::{
    Json, Router,
    extract::{Path as Param, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use server::json as ok;
use std::{
    ffi::OsStr,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

struct Forge {
    pulls: Vec<Value>,
    issue: Value,
    head: String,
    base: String,
    mutations: usize,
    publications: usize,
    comments: Vec<Value>,
    review_comments: Vec<Value>,
    resolved: bool,
    issue_reacted: bool,
    viewed: bool,
    starred: bool,
    created_issue: Option<Value>,
}
type Mock = Arc<Mutex<Forge>>;
async fn identity() -> Json<Value> {
    Json(
        json!({"full_name":"team/repo","html_url":"https://review.example/team/repo","ssh_url":"git@review.example:team/repo.git","permissions":{"push":true},"default_branch":"main"}),
    )
}
async fn user() -> Json<Value> {
    Json(json!({"login":"kar-test","id":5432}))
}
async fn stack_list(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let pr = q.get("pull_request").expect("number");
    if pr == "404" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if pr == "403" {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(json!([{"number":8,"node_id":"summary","base":"main","url":"https://example/8","pull_requests":[{"number":1,"head":{"ref":"topic"},"state":"open"}]}])).into_response()
}
async fn stack_full() -> Json<Value> {
    Json(
        json!({"number":8,"id":18,"base":{"ref":"main"},"url":"https://api.example/8","html_url":"https://example/8","pull_requests":[{"number":1,"head":{"ref":"topic","sha":"abc"},"draft":false,"title":"First","state":"closed","merged_at":"2026-01-01"}]}),
    )
}
async fn pulls(State(mock): State<Mock>) -> Json<Value> {
    Json(json!(mock.lock().unwrap().pulls))
}
async fn get_pull(State(mock): State<Mock>, Param(n): Param<usize>) -> Response {
    mock.lock()
        .unwrap()
        .pulls
        .get(n.saturating_sub(1))
        .map(|v| Json(v.clone()).into_response())
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}
async fn create_pull(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    if headers
        .get("authorization")
        .is_none_or(|v| v != "Bearer local-token")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut db = mock.lock().unwrap();
    db.mutations += 1;
    let n = db.pulls.len() + 1;
    let raw_head = input["head"].as_str().unwrap();
    let (owner, branch) = raw_head.split_once(':').unwrap_or(("team", raw_head));
    let repository = format!("{owner}/{}", input["head_repo"].as_str().unwrap_or("repo"));
    let pr = json!({"number":n,"title":input["title"],"body":input["body"],"head":{"sha":db.head,"ref":branch,"repo":{"full_name":repository}},"base":{"sha":db.base,"ref":input["base"]},
        "html_url":format!("https://review.example/team/repo/pull/{n}"),"state":"open","updated_at":"2026-11-01T10:00:00Z"});
    db.pulls.push(pr.clone());
    if input["title"] == "lost response" {
        StatusCode::BAD_GATEWAY.into_response()
    } else {
        (StatusCode::CREATED, Json(pr)).into_response()
    }
}
async fn issues(State(mock): State<Mock>) -> Json<Value> {
    Json(json!([mock.lock().unwrap().issue,{"number":5,"pull_request":{}}]))
}
async fn get_issue(State(mock): State<Mock>, Param(n): Param<u64>) -> Response {
    match n {
        7 => Json(mock.lock().unwrap().issue.clone()).into_response(),
        8 => mock
            .lock()
            .unwrap()
            .created_issue
            .clone()
            .map(|v| Json(v).into_response())
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response()),
        9 => (
            StatusCode::FORBIDDEN,
            [("x-ratelimit-remaining", "0")],
            Json(json!({"message":"limit"})),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn patch_issue(State(mock): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    let mut db = mock.lock().unwrap();
    if let Some(value) = body.get("state") {
        db.issue["state"] = value.clone();
        db.issue["state_reason"] = body["state_reason"].clone();
    }
    for field in ["title", "body", "labels", "assignees"] {
        if let Some(value) = body.get(field)
            && !(field == "labels" && value[0] == "triage-required")
        {
            db.issue[field] = value.clone();
        }
    }
    Json(db.issue.clone())
}
fn git(cwd: &Path, args: &[&str]) -> String {
    let v = Command::new("git")
        .args([
            "-c",
            "user.name=Kar",
            "-c",
            "user.email=kar@example.invalid",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(v.status.success(), "{}", String::from_utf8_lossy(&v.stderr));
    String::from_utf8(v.stdout).unwrap().trim().into()
}
async fn scenario() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let repo = root.join("repo");
    let bare = root.join("origin.git");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--bare"]);
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "Kar"]);
    git(&repo, &["config", "user.email", "kar@example.invalid"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    fs::write(repo.join("notes.txt"), "first\n").unwrap();
    git(&repo, &["add", "notes.txt"]);
    git(&repo, &["commit", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["switch", "-c", "feature"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://review.example/team/repo.git",
        ],
    );
    git(
        &repo,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            bare.to_str().unwrap(),
        ],
    );
    let mock = Arc::new(Mutex::new(Forge {
        base: base.clone(),
        head: String::new(),
        pulls: vec![],
        mutations: 0,
        publications: 0,
        comments: vec![],
        review_comments: vec![],
        resolved: false,
        issue_reacted: false,
        viewed: false,
        starred: false,
        created_issue: None,
        issue: json!({"number":7,"title":"Fix notes","state":"open","html_url":"https://review.example/team/repo/issues/7"}),
    }));
    let api = diff_contents::github(Router::new())
        .route("/user", get(user))
        .route(
            "/user/starred/team/repo",
            get(actions::starred).put(actions::star),
        )
        .route(
            "/orgs/{org}/repos",
            axum::routing::post(publication::create),
        )
        .route("/repos/team/repo/stacks", get(stack_list))
        .route("/repos/team/repo/stacks/8", get(stack_full))
        .route("/repos/team/repo", get(identity))
        .route("/repos/team/repo/pulls", get(pulls).post(create_pull))
        .route("/repos/team/repo/pulls/{n}", get(get_pull))
        .route(
            "/repos/team/repo/issues",
            get(issues).post(actions::create_issue),
        )
        .route(
            "/repos/team/repo/issues/{n}",
            get(get_issue).patch(patch_issue),
        )
        .route(
            "/repos/team/repo/issues/8/comments",
            get(actions::comments).post(actions::post_comment),
        )
        .route(
            "/repos/team/repo/pulls/1/reviews",
            axum::routing::post(actions::review_submission),
        )
        .route("/graphql", axum::routing::post(actions::review_control))
        .with_state(mock.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let peer = tokio::spawn(async move { axum::serve(l, api).await.unwrap() });
    let config = temp.path().join("hosts.toml");
    fs::write(&config,format!("[[hosts]]\nhost='review.example'\nkind='github'\napi_base='http://{addr}'\ntoken_env='KAR_TEST_FORGE'\n")).unwrap();
    let env = [
        ("KAR_FORGE_CONFIG", config.as_os_str()),
        ("KAR_TEST_FORGE", OsStr::new("local-token")),
    ];
    let native = temp.path().join("native");
    let state = temp.path().join("state");
    fs::create_dir(&native).unwrap();
    let (mut process, url) = server::serve_with_env(&state, &root, &native, &env).await;
    let a = server::client();
    let project = ok(a
        .post(format!("{url}/projects"))
        .json(&json!({"name":"repo","root":repo})))
    .await;
    let p = project["id"].as_str().unwrap();
    let project_url = format!("{url}/projects/{p}");
    // A registered Git checkout may also own CLI-created worktrees outside the host's roots.
    let outside_id = Uuid::new_v4();
    let outside = temp.path().join("outside");
    kar::workbench_git::Repository::open(&repo)
        .unwrap()
        .create_worktree(outside_id, &outside, "outside", "HEAD")
        .unwrap();
    let rejected = a
        .post(format!("{project_url}/git"))
        .json(&json!({"op":"worktree_remove","id":outside_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert!(outside.join("notes.txt").is_file());
    let forge = format!("{project_url}/forge");
    let drivers = ok(a.get(format!("{url}/forge/drivers"))).await;
    assert_eq!(drivers.as_array().unwrap().len(), 9);
    let id = Uuid::new_v4();
    ok(a.post(format!("{project_url}/chats"))
        .json(&json!({"runtime":"codex","id":id})))
    .await;
    let identity = ok(a
        .post(format!("{forge}/query"))
        .json(&json!({"op":"identity"})))
    .await;
    assert_eq!(identity["account"], "kar-test");
    assert_eq!(identity["repository"], "team/repo");
    ok(a.put(format!("{project_url}/file?path=notes.txt"))
        .json(&json!({"text":"second\n"})))
    .await;
    for op in [
        json!({"op":"stage","paths":["notes.txt"]}),
        json!({"op":"commit","message":"second"}),
        json!({"op":"push"}),
    ] {
        ok(a.post(format!("{project_url}/git")).json(&op)).await;
    }
    let head = git(&bare, &["rev-parse", "refs/heads/feature"]);
    mock.lock().unwrap().head = head.clone();
    assert_eq!(git(&bare, &["show", "feature:notes.txt"]), "second");
    let input = json!({"id":Uuid::new_v4(),"chat":id,"op":"create_pull","head":"feature","base":"main","title":"notes","body":"changes"});
    let pr = ok(a.post(format!("{forge}/operations")).json(&input)).await;
    assert_eq!(pr["number"], 1);
    let links_cursor = signals::before(&a, &url, p).await;
    let stack = ok(a.get(format!("{forge}/pulls/1/stack"))).await;
    assert_eq!(stack["id"], "18");
    assert_eq!(stack["layers"][0]["state"], "merged");
    assert_eq!(stack["layers"][0]["headSha"], "abc");
    let listed = ok(a.get(format!("{forge}/pulls/1/stack?include_details=false"))).await;
    assert_eq!(listed["id"], "summary");
    assert!(
        ok(a.get(format!("{forge}/pulls/404/stack")))
            .await
            .is_null()
    );
    assert_eq!(
        a.get(format!("{forge}/pulls/403/stack"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let stats = ok(a.get(format!("{forge}/pulls/1/change-breakdown"))).await;
    assert_eq!(
        stats,
        json!({"code":{"additions":10,"deletions":4},"tests":{"additions":2,"deletions":1},"mocks":{"additions":3,"deletions":2},"generated":{"additions":10,"deletions":0},"docs":{"additions":1,"deletions":3}})
    );
    assert!(
        ok(a.get(format!("{forge}/pulls/9/change-breakdown")))
            .await
            .is_null()
    );
    diff_contents::verify(&a, &forge, "github").await;
    viewed::host(&a, &forge).await;
    let diff = ok(a.get(format!("{forge}/pulls/1/diff"))).await;
    assert_eq!(diff["files"], json!(["notes.txt"]));
    let patch = ok(a.get(format!("{forge}/pulls/1/diff?path=notes.txt"))).await;
    assert!(patch["patch"].as_str().unwrap().contains("+second"));
    let mark = json!({"head":head,"path":"notes.txt","reviewed":true});
    ok(a.put(format!("{forge}/pulls/1/reviewed")).json(&mark)).await;
    let tree = root.join("review");
    ok(a.post(format!("{forge}/pulls/1/worktree"))
        .json(&json!({"id":Uuid::new_v4(),"head":head,"directory":tree})))
    .await;
    assert_eq!(git(&tree, &["rev-parse", "HEAD"]), head);
    let issue_chat = json!({"id":Uuid::new_v4(),"runtime":"codex"});
    let opened = ok(a.post(format!("{forge}/issues/7/chat")).json(&issue_chat)).await;
    assert_eq!(opened["chat"]["info"]["id"], issue_chat["id"]);
    assert_eq!(
        ok(a.post(format!("{forge}/issues/7/chat")).json(&issue_chat)).await,
        opened
    );
    let list = ok(a
        .post(format!("{forge}/query"))
        .json(&json!({"op":"issues"})))
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let closed = ok(a.post(format!("{forge}/operations")).json(
        &json!({"id":Uuid::new_v4(),"chat":id,"op":"set_issue_state","number":7,"state":"closed"}),
    ))
    .await;
    assert_eq!(closed["state"], "closed");
    let limited = a
        .post(format!("{forge}/query"))
        .json(&json!({"op":"issue","number":9}))
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let mut lost = input.clone();
    let lost_id = Uuid::new_v4();
    lost["id"] = json!(lost_id);
    lost["title"] = json!("lost response");
    let result = a
        .post(format!("{forge}/operations"))
        .json(&lost)
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::CONFLICT);
    let second = actions::link_second_project(&a, &url, &root).await;
    let linked = actions::linked(&a, &url, second, p).await;
    actions::issue_pages(&a, &url, [p, &second.0.to_string()]).await;
    let later = publication::first(&a, &url, &root, &mock).await;
    process.kill().await.unwrap();
    let (mut process, url) = server::serve_with_env(&state, &root, &native, &env).await;
    signals::after(&a, &url, p, links_cursor).await;
    publication::after_restart(&a, &url, &mock, later).await;
    assert_eq!(actions::linked(&a, &url, second, p).await, linked);
    let forge = format!("{url}/projects/{p}/forge");
    assert_eq!(
        ok(a.post(format!("{forge}/operations")).json(&input)).await,
        pr
    );
    assert_eq!(mock.lock().unwrap().mutations, 2);
    let verified = ok(a
        .post(format!("{forge}/operations/{lost_id}/reconcile"))
        .json(&json!({"number":2})))
    .await;
    assert_eq!(verified["head_sha"], head);
    assert_eq!(
        ok(a.post(format!("{forge}/operations")).json(&lost)).await,
        verified
    );
    assert_eq!(
        ok(a.get(format!("{forge}/pulls/1/diff"))).await["reviewed"],
        json!(["notes.txt"])
    );
    fs::write(repo.join("more.txt"), "new").unwrap();
    git(&repo, &["add", "more.txt"]);
    git(&repo, &["commit", "-m", "advance"]);
    let advanced = git(&repo, &["rev-parse", "HEAD"]);
    let previous = {
        let mut db = mock.lock().unwrap();
        let old = db.pulls[0].clone();
        db.pulls[0]["head"]["sha"] = json!(advanced);
        db.pulls[0]["updated_at"] = json!("2026-11-01T11:00:00Z");
        db.pulls[0]["state"] = json!("closed");
        old
    };
    let changed = ok(a.get(format!("{forge}/pulls/1/diff"))).await;
    assert_eq!(changed["reviewed"], json!([]));
    mock.lock().unwrap().pulls[0] = previous;
    let stale = ok(a
        .post(format!("{forge}/query"))
        .json(&json!({"op":"pull","number":1})))
    .await;
    assert_eq!(stale["head_sha"], advanced);
    assert_eq!(stale["state"], "closed");
    let links = ok(a.get(format!("{forge}/links"))).await;
    assert_eq!(links.as_array().unwrap().len(), 4);
    actions::repository(&a, &url, &bare, &root).await;
    actions::routing(&a, &forge, &url, id).await;
    let catalog = ok(a.get(format!("{forge}/issue-tracker/candidates"))).await;
    assert_eq!(catalog["labels"][0]["name"], "bug");
    assert_eq!(catalog["assignees"][0]["id"], "kar-test");
    assert_eq!(catalog["truncated"], true);
    let templates = ok(a.get(format!("{forge}/issue-tracker/templates"))).await;
    assert_eq!(templates["blankIssuesEnabled"], false);
    assert_eq!(templates["templates"][0]["labels"], json!(["bug", "alpha"]));
    assert_eq!(
        templates["templates"][0]["body"],
        "### Reproduce\n\nRequired."
    );
    actions::issue(&a, &forge, id).await;
    actions::review(&a, &forge, id).await;
    stacked::run(&a, &format!("{url}/projects/{p}"), &repo, &bare, id, &mock).await;
    assert!(
        !fs::read_to_string(state.join("workbench/projects.json"))
            .unwrap()
            .contains("local-token")
    );
    process.kill().await.unwrap();
    peer.abort();
}

#[path = "workbench_forge/actions.rs"]
mod actions;
#[path = "workbench_forge/actions_live.rs"]
mod actions_live;
#[path = "workbench_forge/clone_jobs.rs"]
mod clone_jobs;
#[path = "workbench_forge/hosted.rs"]
mod hosted;
#[path = "workbench_forge/providers.rs"]
mod providers;
#[path = "workbench_forge/stacked.rs"]
mod stacked;
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(mode @ ("hosted-issue-actions" | "hosted-review-actions")) => {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(actions_live::run(mode == "hosted-issue-actions"))
        }
        Some("hosted-github") => tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(hosted::run()),
        Some("clone-filter") => clone_jobs::filter(),
        Some("clone-ssh-refused") => {
            eprintln!("Permission denied (publickey).");
            std::process::exit(1);
        }
        Some("app-server") => codex_chat_peer::agent(),
        Some("--version") => println!("codex 0.169.0"),
        _ => tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(120), async {
                scenario().await;
                providers::run().await;
                signin::run().await;
                clone_jobs::run().await;
            })
            .await
            .unwrap()
        }),
    }
}

#[path = "workbench_forge/publication.rs"]
mod publication;

#[path = "workbench_forge/signin.rs"]
mod signin;
