//! Non-GitHub contracts over a real Kar HTTP process and local HTTP/GraphQL peers.
//! No external accounts are used by these protocol scenarios.
use super::{git, ok, server};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

const KINDS: &[&str] = &[
    "gitlab",
    "forgejo",
    "bitbucket",
    "gitcafe",
    "cursor-origin",
    "tanstack-source",
    "azure-devops",
];
const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
#[derive(Default)]
struct Data {
    writes: HashMap<String, usize>,
    markers: HashMap<String, String>,
    states: HashMap<String, String>,
}
type Shared = Arc<Mutex<Data>>;
fn native_pull(kind: &str, body: &str) -> Value {
    let repository = json!({"name":"repo","project":{"name":"proj"}});
    match kind {
        "gitlab" => {
            json!({"iid":2,"title":"queued","description":body,"state":"opened","web_url":"https://example/2","source_branch":"feature","target_branch":"main","diff_refs":{"base_sha":BASE,"head_sha":HEAD}})
        }
        "forgejo" => {
            json!({"number":2,"title":"queued","body":body,"state":"open","html_url":"https://example/2","head":{"ref":"feature","sha":HEAD},"base":{"ref":"main","sha":BASE}})
        }
        "bitbucket" => {
            json!({"id":2,"title":"queued","description":body,"state":"OPEN","links":{"html":{"href":"https://example/2"}},"source":{"branch":{"name":"feature"},"commit":{"hash":HEAD}},"destination":{"branch":{"name":"main"},"commit":{"hash":BASE}}})
        }
        "gitcafe" => {
            json!({"number":2,"title":"queued","description":body,"state":"open","sourceBranch":"feature","targetBranch":"main","headOid":HEAD,"observedBaseOid":BASE})
        }
        "cursor-origin" => {
            json!({"number":"2","title":"queued","body":body,"state":"open","webUrl":"https://example/2","head":{"ref":"feature","sha":HEAD},"base":{"ref":"main","sha":BASE}})
        }
        "tanstack-source" => {
            json!({"number":2,"title":"queued","body":body,"state":"open","head":"team:feature","base":"main","headSha":HEAD,"baseSha":BASE})
        }
        "azure-devops" => {
            json!({"pullRequestId":2,"title":"queued","description":body,"status":"active","sourceRefName":"refs/heads/feature","targetRefName":"refs/heads/main","lastMergeSourceCommit":{"commitId":HEAD},"lastMergeTargetCommit":{"commitId":BASE},"repository":repository})
        }
        _ => unreachable!(),
    }
}
fn raw_issue(kind: &str, state: &str) -> Value {
    match kind {
        "gitlab" => json!({"iid":7,"state":if state=="open"{"opened"}else{state},"title":"work"}),
        "azure-devops" => {
            json!({"id":7,"rev":5,"fields":{"System.TeamProject":"proj","System.State":state,"System.Title":"work","System.WorkItemType":"Task"}})
        }
        "linear" => {
            json!({"id":"lin-7","identifier":"ENG-7","number":7,"title":"work","team":{"id":"team-id","key":"ENG"},"state":{"id":if state=="Todo"{"todo"}else{"done"},"name":state,"type":if state=="Todo"{"unstarted"}else{"completed"}}})
        }
        _ => json!({"number":7,"title":"work","state":state}),
    }
}
fn linear(data: &mut Data, body: &Value) -> Response {
    let q = body["query"].as_str().unwrap();
    let state = data.states.entry("linear".into()).or_insert("Todo".into());
    let result = if q.starts_with("query KarScope") {
        assert_eq!(body["variables"]["team"], "ENG");
        json!({"viewer":{"email":"viewer"},"organization":{"urlKey":"sample"},"teams":{"nodes":[{"id":"team-id","key":"ENG","states":{"nodes":[{"id":"todo","name":"Todo","type":"unstarted"},{"id":"done","name":"Done","type":"completed"}]}}]}})
    } else if q.starts_with("query KarIssues") {
        json!({"issues":{"nodes":[raw_issue("linear",state)],"pageInfo":{"hasNextPage":false}}})
    } else if q.starts_with("query KarIssue") {
        if body["variables"]["id"] == "ENG-403" {
            return Json(json!({"errors":[{"extensions":{"code":"FORBIDDEN"}}]})).into_response();
        }
        assert_eq!(body["variables"]["id"], "ENG-7");
        json!({"issue":raw_issue("linear",state)})
    } else {
        assert!(q.starts_with("mutation KarUpdate"));
        assert_eq!(body["variables"]["id"], "lin-7");
        assert_eq!(body["variables"]["input"]["stateId"], "done");
        *state = "Done".into();
        *data.writes.entry("linear".into()).or_default() += 1;
        json!({"issueUpdate":{"success":true,"issue":raw_issue("linear",state)}})
    };
    Json(json!({"data":result})).into_response()
}
async fn peer(
    State(shared): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Response {
    let (kind, path) = uri
        .path()
        .trim_start_matches('/')
        .split_once('/')
        .unwrap_or((uri.path().trim_start_matches('/'), ""));
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let correct = match kind {
        "linear" => auth == "fixture-secret",
        "azure-devops" | "bitbucket" => auth.starts_with("Basic "),
        _ => auth == "Bearer fixture-secret",
    };
    if !correct {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut db = shared.lock().unwrap();
    let b: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    if kind == "linear" {
        return linear(&mut db, &b);
    }
    if path.ends_with("/403") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if path.ends_with("/429") {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let repo = match kind {
        "gitlab" => "projects/team%2Frepo",
        "bitbucket" => "repositories/team/repo",
        "azure-devops" => "org/proj/_apis/git/repositories/repo",
        _ => "repos/team/repo",
    };
    let state = db.states.get(kind).cloned().unwrap_or_else(|| {
        if kind == "azure-devops" {
            "New".into()
        } else {
            "open".into()
        }
    });
    let resource = if path == repo && method == Method::GET {
        match kind {
            "gitlab" => json!({"id":11,"path_with_namespace":"team/repo"}),
            "forgejo" | "bitbucket" => json!({"full_name":"team/repo"}),
            "azure-devops" => {
                json!({"id":"repo-id","name":"repo","project":{"id":"p","name":"proj"}})
            }
            _ => json!({"id":"repo-id","name":"repo","defaultBranch":"main"}),
        }
    } else if kind == "gitcafe" && path == "repos/fork/renamed" && method == Method::GET {
        json!({"id":"fork-id"})
    } else if method == Method::POST && b["title"] == "Fork PR" {
        match kind {
            "gitcafe" => {
                assert_eq!(path, "repos/team/repo/pulls");
                assert_eq!(b["source"], "feature");
                assert_eq!(b["repo"], "fork-id");
            }
            "forgejo" => {
                assert_eq!(path, "repos/team/repo/pulls");
                assert_eq!(b["head"], "fork:feature");
            }
            "bitbucket" => {
                assert_eq!(path, "repositories/team/repo/pullrequests");
                assert_eq!(b["source"]["repository"]["full_name"], "fork/renamed");
            }
            "gitlab" => {
                assert_eq!(path, "projects/fork%2Frenamed/merge_requests");
                assert_eq!(b["source_branch"], "feature");
                assert_eq!(b["target_project_id"], 11);
            }
            _ => panic!("driver sent an unrepresentable fork create"),
        }
        *db.writes.entry(kind.into()).or_default() += 1;
        native_pull(
            kind,
            b["body"].as_str().or(b["description"].as_str()).unwrap(),
        )
    } else if matches!(
        path,
        "user" | "auth/principal" | "namespaces" | "org/_apis/connectionData"
    ) {
        json!({"username":"viewer","login":"viewer","nickname":"viewer","handle":"viewer","namespaces":[{"namespace":{"type":"user","slug":"viewer"}}],"authenticatedUser":{"providerDisplayName":"viewer"}})
    } else if kind == "bitbucket"
        && path == format!("repositories/fork/repo/commit/{}", &HEAD[..12])
    {
        json!({"hash":HEAD})
    } else if path.ends_with("/pullrequests/2/iterations") {
        json!({"value":[{"id":2,"sourceRefCommit":{"commitId":HEAD},"commonRefCommit":{"commitId":BASE}},{"id":1,"sourceRefCommit":{"commitId":"stale"},"commonRefCommit":{"commitId":"older"}}]})
    } else if path.ends_with("workitemtypes/Task/states") {
        json!({"value":[{"name":"New","category":"Proposed"},{"name":"Done","category":"Completed"}]})
    } else if path.ends_with("/wiql") {
        let rows = if b["query"].as_str().unwrap().contains("> 0 ") {
            vec![json!({"id":7})]
        } else {
            vec![]
        };
        json!({"workItems":rows})
    } else if path.ends_with("/workitemsbatch") {
        assert_eq!(b["ids"], json!([7]));
        json!({"value":[raw_issue(kind,&state)]})
    } else if path.ends_with("/issues/7") || path.ends_with("/workitems/7") {
        if method == Method::GET {
            raw_issue(kind, &state)
        } else {
            let target = if kind == "azure-devops" {
                assert_eq!(headers["content-type"], "application/json-patch+json");
                assert_eq!(b[0]["value"], 5);
                b[1]["value"].as_str().unwrap()
            } else if kind == "gitlab" {
                assert_eq!(b["state_event"], "close");
                "closed"
            } else {
                b["state"].as_str().unwrap()
            };
            db.states.insert(kind.into(), target.into());
            raw_issue(kind, target)
        }
    } else if path.ends_with("/issues") {
        let rows = if uri.query().unwrap_or_default().contains("page=2") {
            vec![]
        } else {
            vec![raw_issue(kind, &state)]
        };
        if kind == "tanstack-source" {
            json!({"items":rows,"next":null})
        } else {
            json!(rows)
        }
    } else {
        let coll = match kind {
            "gitlab" => "merge_requests",
            "bitbucket" | "azure-devops" => "pullrequests",
            _ => "pulls",
        };
        if path == format!("{repo}/{coll}/2") && method == Method::GET {
            let mut v = native_pull(
                kind,
                db.markers
                    .get(kind)
                    .map(String::as_str)
                    .unwrap_or("original"),
            );
            if kind == "bitbucket" {
                v["source"]["commit"]["hash"] = json!(&HEAD[..12]);
                v["source"]["repository"] = json!({"full_name":"fork/repo"});
            }
            v
        } else if path == format!("{repo}/{coll}") && method == Method::POST {
            let body = b["body"].as_str().or(b["description"].as_str()).unwrap();
            assert!(body.contains("<!-- kar-operation:"));
            if kind == "gitcafe" {
                assert_eq!(b["repo"], "repo-id");
                assert_eq!(b["source"], "feature");
            }
            db.markers.insert(kind.into(), body.into());
            *db.writes.entry(kind.into()).or_default() += 1;
            native_pull(kind, body)
        } else if path == format!("{repo}/{coll}") && method == Method::GET {
            let second = uri
                .query()
                .is_some_and(|q| q.contains("page=2") || q.contains("%24skip=1"));
            let rows = if second {
                vec![]
            } else {
                vec![native_pull(kind, "original")]
            };
            match kind {
                "azure-devops" => json!({"value":rows}),
                "bitbucket" => json!({"values":rows}),
                "cursor-origin" => json!({"pullRequests":rows}),
                "gitcafe" | "tanstack-source" => json!({"items":rows}),
                _ => json!(rows),
            }
        } else {
            return (StatusCode::NOT_FOUND, format!("{method} {uri}")).into_response();
        }
    };
    Json(resource).into_response()
}
pub(super) async fn run() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let root = dir.path().join("work");
    let native = dir.path().join("native");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&native).unwrap();
    let store = Shared::default();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let router = Router::new().fallback(peer).with_state(store.clone());
    let api = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    let config = dir.path().join("forge.toml");
    let mut hosts = String::new();
    for kind in KINDS {
        hosts.push_str(&format!("[[hosts]]\nhost='{kind}.example'\nkind='{kind}'\napi_base='http://{addr}/{kind}'\ntoken_env='KAR_FIXTURE_FORGE_TOKEN'\n"));
    }
    fs::write(&config, &hosts).unwrap();
    let (mut kar, base) = server::serve_with_env(
        &state,
        &root,
        &native,
        &[
            ("KAR_FORGE_CONFIG", config.as_os_str()),
            ("KAR_FIXTURE_FORGE_TOKEN", OsStr::new("fixture-secret")),
            ("KAR_BITBUCKET_EMAIL", OsStr::new("tester@example.invalid")),
            ("KAR_TANSTACK_SOURCE", OsStr::new("1")),
        ],
    )
    .await;
    let client = server::client();
    for kind in KINDS.iter().copied().chain(["linear"]) {
        let repo = root.join(kind);
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        let git_host = if kind == "linear" { "forgejo" } else { kind };
        let relative = if kind == "azure-devops" {
            "org/proj/_git/repo"
        } else {
            "team/repo"
        };
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                &format!("https://{git_host}.example/{relative}.git"),
            ],
        );
        let project = ok(client
            .post(format!("{base}/projects"))
            .json(&json!({"root":repo,"name":kind})))
        .await;
        let p = project["id"].as_str().unwrap();
        if kind == "linear" {
            hosts.push_str(&format!("[[issue_trackers]]\nproject='{p}'\nteam='ENG'\nworkspace='sample'\napi_base='http://{addr}/linear'\ntoken_env='KAR_FIXTURE_FORGE_TOKEN'\n"));
            fs::write(&config, &hosts).unwrap();
        }
        let url = format!("{base}/projects/{p}/forge");
        let query = format!("{url}/query");
        let identity = if kind == "linear" {
            ok(client.get(format!("{url}/issue-tracker"))).await
        } else {
            ok(client.post(&query).json(&json!({"op":"identity"}))).await
        };
        assert_eq!(identity["account"], "viewer", "{kind}");
        let chat = Uuid::new_v4();
        ok(client
            .post(format!("{base}/projects/{p}/chats"))
            .json(&json!({"id":chat,"runtime":"codex"})))
        .await;
        if kind != "linear" {
            let ps = ok(client.post(&query).json(&json!({"op":"pulls"}))).await;
            assert_eq!(ps.as_array().unwrap().len(), 1, "{kind}");
            let pr = ok(client.post(&query).json(&json!({"op":"pull","number":2}))).await;
            assert_eq!(pr["head_sha"], HEAD, "{kind}");
            assert_eq!(pr["base_sha"], BASE);
            assert_eq!(pr["head_ref"], "feature");
            assert_eq!(pr["base_ref"], "main");
            let create = json!({"op":"create_pull","id":Uuid::new_v4(),"chat":chat,"head":"feature","base":"main","title":"queued","body":"note"});
            let operations = format!("{url}/operations");
            let created = ok(client.post(&operations).json(&create)).await;
            assert_eq!(created["number"], 2);
            assert_eq!(created, ok(client.post(&operations).json(&create)).await);
            assert_eq!(store.lock().unwrap().writes[kind], 1);
            let fork = json!({"op":"create_pull","id":Uuid::new_v4(),"chat":chat,"head":"feature",
                              "head_repository":"fork/renamed","base":"main","title":"Fork PR","body":"fork"});
            let reply = client.post(&operations).json(&fork).send().await.unwrap();
            if ["gitlab", "bitbucket", "gitcafe", "forgejo"].contains(&kind) {
                assert!(reply.status().is_success(), "{kind}");
                assert_eq!(store.lock().unwrap().writes[kind], 2);
            } else {
                assert_eq!(reply.status(), 422, "{kind}");
                assert_eq!(reply.json::<Value>().await.unwrap()["kind"], "unsupported");
                assert_eq!(store.lock().unwrap().writes[kind], 1);
            }
        }
        if matches!(kind, "bitbucket" | "cursor-origin" | "gitcafe") {
            let r = client
                .post(&query)
                .json(&json!({"op":"issues"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 422);
            assert_eq!(r.json::<Value>().await.unwrap()["kind"], "unsupported");
        } else {
            let rows = ok(client.post(&query).json(&json!({"op":"issues"}))).await;
            assert_eq!(rows[0]["number"], 7, "{kind}");
            let state = if matches!(kind, "azure-devops" | "linear") {
                "Done"
            } else {
                "closed"
            };
            let write = json!({"op":"set_issue_state","id":Uuid::new_v4(),"chat":chat,"number":7,"state":state});
            let changed = ok(client.post(format!("{url}/operations")).json(&write)).await;
            assert_eq!(changed["state"], state);
        }
        let op = if kind == "linear" { "issue" } else { "pull" };
        let blocked = client
            .post(&query)
            .json(&json!({"op":op,"number":403}))
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status(), 403, "{kind}");
        assert_eq!(blocked.json::<Value>().await.unwrap()["kind"], "permission");
        if kind != "linear" {
            let limited = client
                .post(&query)
                .json(&json!({"op":op,"number":429}))
                .send()
                .await
                .unwrap();
            assert_eq!(limited.status(), 429, "{kind}");
        }
    }
    kar.kill().await.unwrap();
    api.abort();
}
