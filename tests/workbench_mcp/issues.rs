//! Real workbench bearer, persisted forge receipts, loopback GitHub API.
use super::*;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

type Calls = Arc<Mutex<Vec<Value>>>;

pub(super) fn git(root: &Path, args: &[&str]) {
    let s = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(s.status.success(), "{}", String::from_utf8_lossy(&s.stderr));
}

const REMOTE: &str = "https://models.example/kar/demo.git";

pub(super) async fn fixture(root: &Path, config: &Path) -> Calls {
    git(root, &["init", "-b", "main"]);
    git(
        root,
        &[
            "-c",
            "user.name=Kar",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    git(root, &["remote", "add", "origin", REMOTE]);
    let calls = Calls::default();
    let api = Router::new()
        .route(
            "/repos/kar/demo/issues",
            post(
                |State(c): State<Calls>, Json(input): Json<Value>| async move {
                    c.lock().unwrap().push(input);
                    Json(json!({"number":7,"html_url":"https://models.example/kar/demo/issues/7"}))
                },
            ),
        )
        .route(
            "/repos/kar/demo/issues/7",
            get(|State(c): State<Calls>| async move {
                let state = c.lock().unwrap().iter().rev().find_map(|v|v["state"].as_str().map(str::to_owned)).unwrap_or_else(||"open".into());
                Json(json!({"number":7,"state":state}))
            }).patch(|State(c): State<Calls>, Json(body): Json<Value>| async move {
                c.lock().unwrap().push(body.clone()); Json(json!({"number":7,"state":body["state"]}))
            }),
        )
        .route(
            "/repos/kar/demo/issues/7/comments",
            post(
                |State(c): State<Calls>, Json(input): Json<Value>| async move {
                    c.lock().unwrap().push(input);
                    Json(json!({"id":31}))
                },
            ),
        )
        .route("/graphql", post(|Json(input):Json<Value>| async move {
            let mut node=json!({"id":"issue-7","number":7,"title":"From the model","repository":{"nameWithOwner":"kar/demo"},
                "url":"https://models.example/kar/demo/issues/7","state":"OPEN","body":"Initial text","createdAt":"2026-01-01","updatedAt":"2026-01-01"});
            for key in ["labels","assignees","closedByPullRequestsReferences","comments","timelineItems"] {
                node[key]=json!({"nodes":[],"pageInfo":{"hasNextPage":false,"hasPreviousPage":false}});
            }
            if input["query"].as_str().unwrap().contains("KarIssueSearch") {
                assert!(input["variables"]["q"].as_str().unwrap().contains("repo:kar/demo"));
                Json(json!({"data":{"search":{"nodes":[node],"pageInfo":{"hasNextPage":false}}}}))
            } else {
                Json(json!({"data":{"repository":{"issue":node},"viewer":{"login":"kar-test"}}}))
            }
        }))
        .with_state(calls.clone());
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(socket, api).await.unwrap() });
    fs::write(config,format!("[[hosts]]\nhost='models.example'\nkind='github'\napi_base='http://{addr}'\ntoken_env='KAR_MCP_TEST_FORGE'\n")).unwrap();
    calls
}

pub(super) async fn exercise(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    provider: &str,
    root: &Path,
    calls: &Calls,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) -> Uuid {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let enable = json!({"mode":"execute","issue_writes":true});
    let plan = json!({"mode":"plan","issue_writes":true});
    let create = "kar_create_issue";
    let comment = "kar_comment_on_issue";
    let make = |name: &str, args: Value| rpc("tools/call", json!({"name":name,"arguments":args}));
    let id = Uuid::new_v4();
    let args = json!({"command_id":id,"title":"From the model","body":"Initial text","labels":["integration"]});
    let denied = |v: &Value| v.get("error").is_some() || v["result"]["isError"] == true;
    let strong = "full-access/default";

    assert_eq!(server::json(host.get(&policy)).await["issue_writes"], false);
    assert!(denied(
        &server::json(make(create, args.clone()).header("kar-mode-limit", strong)).await
    ));
    let external = Client::new()
        .put(&policy)
        .header("Authorization", provider)
        .json(&enable)
        .send()
        .await
        .unwrap();
    assert_eq!(external.status(), StatusCode::UNAUTHORIZED);
    let registered = server::json(host.put(&policy).json(&enable)).await;
    assert!(
        registered["repository"]
            .as_str()
            .unwrap()
            .contains("models.example/kar/demo")
    );
    for name in [create, comment] {
        let search = server::json(make("tool_search", json!({"query":name}))).await;
        assert!(
            search["result"]["structuredContent"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == name)
        );
    }
    let listed = server::json(rpc("tools/list", json!({}))).await;
    for name in [create, comment] {
        assert!(
            listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == name)
        );
    }
    for field in ["thread_id", "project_id", "op"] {
        let mut foreign = args.clone();
        foreign[field] = json!(Uuid::new_v4());
        assert!(denied(&server::json(make(create, foreign)).await));
    }
    assert!(denied(
        &server::json(make(create, args.clone()).header("kar-mode-limit", "full-access/plan"))
            .await
    ));
    git(
        root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://models.example/kar/other.git",
        ],
    );
    let changed = server::json(make(create, args.clone())).await;
    assert!(
        changed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("authorized execution session")
    );
    git(root, &["remote", "set-url", "origin", REMOTE]);
    assert!(calls.lock().unwrap().is_empty());
    for _ in 0..2 {
        let created = server::json(make(create, args.clone())).await;
        assert_eq!(
            created["result"]["structuredContent"]["number"], 7,
            "{created}"
        );
    }
    let comment_id = Uuid::new_v4();
    let body = json!({"command_id":comment_id,"number":7,"body":"Agent comment"});
    for _ in 0..2 {
        let sent = server::json(make(comment, body.clone())).await;
        assert_eq!(
            sent["result"]["structuredContent"]["comment_id"], 31,
            "{sent}"
        );
    }
    let mut changed = args.clone();
    changed["title"] = json!("Different payload");
    assert!(denied(&server::json(make(create, changed)).await));
    for key in [id, comment_id] {
        let receipt =
            server::json(host.get(format!("{url}/projects/{project}/forge/operations/{key}")))
                .await;
        assert_eq!(receipt["chat"], chat.to_string());
        assert_eq!(receipt["result"]["Ok"]["number"], 7, "{receipt}");
    }

    for expected in ["closed", "open"] {
        let command = Uuid::new_v4();
        for _ in 0..2 {
            let state = server::json(make(
                "kar_set_issue_state",
                json!({"command_id":command,"number":7,"state":expected}),
            ))
            .await;
            assert_eq!(
                state["result"]["structuredContent"]["state"], expected,
                "{state}"
            );
        }
    }
    let read_only = json!({"mode":"plan","issue_reads":true});
    server::json(host.put(&policy).json(&read_only)).await;
    let list = server::json(make("kar_list_issues", json!({"query":"model","limit":1}))).await;
    assert_eq!(
        list["result"]["structuredContent"]["issues"][0]["key"], "7",
        "{list}"
    );
    let detail = server::json(make("kar_read_issue", json!({"number":7}))).await;
    assert_eq!(
        detail["result"]["structuredContent"]["body"], "Initial text",
        "{detail}"
    );
    assert!(denied(
        &server::json(make(
            "kar_read_issue",
            json!({"number":7,"thread_id":Uuid::new_v4()})
        ))
        .await
    ));
    for permissions in [plan, json!({"mode":"execute","issue_writes":false})] {
        server::json(host.put(&policy).json(&permissions)).await;
        let mut fresh = body.clone();
        fresh["command_id"] = json!(Uuid::new_v4());
        assert!(denied(
            &server::json(make(comment, fresh).header("kar-mode-limit", strong)).await
        ));
        let list = server::json(rpc("tools/list", json!({}))).await;
        assert!(
            !list["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == comment)
        );
    }
    let sent = calls.lock().unwrap();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[0]["title"], "From the model");
    assert_eq!(sent[0]["labels"], json!(["integration"]));
    assert_eq!(sent[1]["body"], "Agent comment");
    comment_id
}
