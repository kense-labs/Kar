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
fn publication_peer(kind: &str, path: &str, method: &Method, b: &Value) -> Option<Value> {
    let name = "new-repo";
    let owner = "team";
    let url = format!("https://{kind}.example/{owner}/{name}.git");
    let ssh = format!("git@{kind}.example:{owner}/{name}.git");
    Some(match (kind, path, method.as_str()) {
        ("gitlab", "namespaces/team", "GET") => json!({"id":22}),
        ("gitlab", "projects", "POST") if b["name"] == name => {
            assert_eq!(b["namespace_id"], 22);
            assert_eq!(b["visibility"], "private");
            json!({"path_with_namespace":"team/new-repo","web_url":url,"ssh_url_to_repo":ssh})
        }
        ("forgejo", "orgs/team/repos", "POST") if b["name"] == name => {
            assert_eq!(b["private"], true);
            assert_eq!(b["auto_init"], false);
            json!({"full_name":"team/new-repo","clone_url":url,"ssh_url":ssh})
        }
        ("bitbucket", "repositories/team/new-repo", "POST") => {
            assert_eq!(b["is_private"], true);
            json!({"full_name":"team/new-repo","links":{"clone":[{"name":"https","href":url},{"name":"ssh","href":ssh}]}})
        }
        ("gitcafe", "orgs/team/repos", "POST") if b["name"] == name => {
            assert_eq!(b["visibility"], "private");
            assert!(b["requestKey"].is_string());
            json!({"repoId":"A","state":"pending","owner":owner,"name":name})
        }
        ("gitcafe", "orgs/team/admissions/A", "GET") => {
            json!({"repoId":"A","state":"complete","owner":owner,"name":name})
        }
        ("cursor-origin", "repos/team", "POST") => json!({"name":name,"fullName":"team/new-repo"}),
        ("cursor-origin", "repos/team/new-repo", "PATCH") => {
            assert_eq!(b["visibility"], "private");
            json!({"name":name,"fullName":"team/new-repo","cloneUrl":url})
        }
        ("tanstack-source", "repos", "POST") if b["name"] == name => {
            assert_eq!(b["visibility"], "private");
            assert_eq!(b["owner"], owner);
            json!({"name":name,"owner":owner,"cloneUrl":url})
        }
        ("azure-devops", "org/_apis/projects/proj", "GET") => {
            json!({"id":"project-id","visibility":"private"})
        }
        ("azure-devops", "org/proj/_apis/git/repositories", "POST") => {
            assert_eq!(b["project"]["id"], "project-id");
            assert_eq!(b["name"], name);
            let clone = "https://azure-devops.example/org/proj/_git/new-repo";
            json!({"name":name,"project":{"name":"proj"},"remoteUrl":clone,"sshUrl":clone})
        }
        _ => return None,
    })
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
    if let Some(result) = issue_search_peer(kind, path, &uri, &b) {
        return result;
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
    if kind == "gitlab"
        && path == "graphql"
        && b["query"]
            .as_str()
            .is_some_and(|q| q.starts_with("query KarFileStats"))
    {
        let iid = b["variables"]["iid"].as_str().unwrap();
        return Json(json!({"data":{"project":{"mergeRequest":{"diffStats":[{"path":"docs/readme.md","additions":7,"deletions":3}],"diffStatsSummary":{"fileCount":if iid=="2"{1}else{2}}}}}})).into_response();
    }
    if kind == "tanstack-source" && path.ends_with("/files") {
        return Json(json!({"files":[{"path":"docs/readme.md","additions":7,"deletions":3}],"truncated":path.contains("/9/")})).into_response();
    }
    if let Some(r) = publication_peer(kind, path, &method, &b) {
        return Json(r).into_response();
    }
    if kind == "gitcafe" && path.ends_with("/stack") {
        return Json(json!({"stack":{"id":"ST","number":8,"revision":2,"landingBase":"release","members":[
            {"position":2,"pullRequestNumber":2,"draft":true,"sourceBranch":"next","title":"Top","state":"open","headOid":"old"},
            {"position":1,"pullRequestNumber":1,"draft":false,"sourceBranch":"base-feature","title":"Bottom","state":"merged","headOid":"original"}]}})).into_response();
    }
    if kind == "tanstack-source" && path.ends_with("/stack") {
        return Json(json!({"members":[{"number":1,"title":"Bottom","headRef":"refs/heads/one","baseRef":"release","draft":false},
        {"number":2,"title":"Top","headRef":"fork:two","baseRef":"one","draft":true}]})).into_response();
    }
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
        if matches!(
            kind,
            "gitlab" | "forgejo" | "tanstack-source" | "azure-devops"
        ) {
            verify_search(&client, &base, p, kind).await;
        }
        let chat = Uuid::new_v4();
        ok(client
            .post(format!("{base}/projects/{p}/chats"))
            .json(&json!({"id":chat,"runtime":"codex"})))
        .await;
        if kind != "linear" {
            let repo_name = if kind == "azure-devops" {
                "org/proj/_git/new-repo"
            } else {
                "team/new-repo"
            };
            let endpoint = format!("{base}/projects/{p}/forge/publish");
            let publish = json!({"id":Uuid::new_v4(),"visibility":"private","protocol":"https","host":format!("{kind}.example"),"provider":kind,"repository":repo_name});
            let result = ok(client.post(&endpoint).json(&publish)).await;
            assert_eq!(result["status"], "remote_added", "{kind}");
            assert_eq!(result["remoteName"], "origin-2");
            if kind == "cursor-origin" || kind == "azure-devops" {
                let mut denied = publish.clone();
                denied["visibility"] = json!("public");
                denied["id"] = json!(Uuid::new_v4());
                let response = client.post(&endpoint).json(&denied).send().await.unwrap();
                assert!(!response.status().is_success(), "{kind}");
            }

            let stack = ok(client.get(format!("{url}/pulls/2/stack"))).await;
            match kind {
                "gitcafe" => {
                    assert_eq!(stack["layers"][0]["number"], 1);
                    assert_eq!(stack["layers"][0]["headSha"], "original");
                    assert_eq!(stack["layers"][1]["headSha"], HEAD);
                }
                "tanstack-source" => {
                    assert_eq!(stack["number"], 1);
                    assert_eq!(stack["base"], "release");
                    assert_eq!(stack["layers"][1]["headBranch"], "two");
                }
                _ => assert!(stack.is_null(), "{kind}"),
            }
            let stats = ok(client.get(format!("{url}/pulls/2/change-breakdown"))).await;
            if matches!(kind, "gitlab" | "tanstack-source") {
                assert_eq!(stats["docs"], json!({"additions":7,"deletions":3}));
                assert!(
                    ok(client.get(format!("{url}/pulls/9/change-breakdown")))
                        .await
                        .is_null()
                );
            } else {
                assert!(stats.is_null(), "{kind}");
            }
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

fn issue_search_peer(kind: &str, path: &str, uri: &Uri, b: &Value) -> Option<Response> {
    let params = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    let date = "2026-11-01T00:00:00Z";
    if kind == "gitlab" {
        let issue_url = "https://gitlab.example/root/team/repo/-/issues/72";
        let note = |id: u64, body: &str, system: bool| json!({"id":id,"body":body,"system":system,"created_at":date,"author":{"username":"viewer"}});
        let native = match path {
            "projects/team%2Frepo/issues" if params.get("iids[]").is_some_and(|n| n == "72") => {
                assert_eq!(params["with_labels_details"], "true");
                assert_eq!(params["per_page"], "1");
                Some(
                    json!([{"iid":72,"title":"read","description":"body","state":"opened","web_url":issue_url,"created_at":date,"updated_at":date,"labels":[{"name":"red","color":"#ff0000"}]}]),
                )
            }
            "projects/team%2Frepo/issues/72/notes" => {
                assert_eq!(params["sort"], "asc");
                assert_eq!(params["order_by"], "created_at");
                let activity = params.contains_key("activity_filter");
                let first = params["page"] == "1";
                let rows = match (activity, first) {
                    (true, true) => json!([note(30, "assigned to @viewer", true)]),
                    (true, false) => {
                        json!([note(31, "mentioned in merge request sibling!4", true)])
                    }
                    (false, true) => json!([
                        note(7, "first", false),
                        note(30, "assigned to @viewer", true)
                    ]),
                    (false, false) => json!([note(8, "second", false)]),
                };
                let mut res = Json(rows).into_response();
                res.headers_mut()
                    .insert("x-next-page", if first { "2" } else { "" }.parse().unwrap());
                return Some(res);
            }
            "projects/team%2Frepo/issues/72/resource_label_events" => {
                Some(json!([{"id":30,"action":"add","label":{"name":"red"},"created_at":date}]))
            }
            "projects/team%2Frepo/issues/72/closed_by"
            | "projects/team%2Frepo/issues/72/related_merge_requests" => Some(
                json!([{"iid":4,"references":{"full":"team/sibling!4"},"title":"fix","web_url":"https://sample/fix","state":"merged"}]),
            ),
            "projects/team%2Frepo/issues/72/award_emoji" => {
                Some(json!([{"id":88,"name":"thumbsup","user":{"username":"other"}}]))
            }
            "projects/team%2Frepo/issues/74/notes" | "projects/team%2Frepo/issues/75/notes" => {
                Some(json!([note(7, "works with old GitLab", false)]))
            }
            "graphql"
                if b["query"]
                    .as_str()
                    .is_some_and(|q| q.starts_with("query KarIssueAwards")) =>
            {
                assert_eq!(b["variables"]["fullPath"], "team/repo");
                let page = &b["variables"]["cursor"];
                if b["variables"]["iid"] == "74" {
                    Some(
                        json!({"errors":[{"message":"Field 'awardEmoji' doesn't exist on type 'Note'"}]}),
                    )
                } else if b["variables"]["iid"] == "75" {
                    return Some(StatusCode::FORBIDDEN.into_response());
                } else {
                    assert_eq!(b["variables"]["iid"], "72");
                    let id = if page.is_null() {
                        7
                    } else {
                        assert_eq!(page, "notes:+/");
                        8
                    };
                    Some(
                        json!({"data":{"currentUser":{"username":"viewer"},"project":{"issue":{"notes":{"pageInfo":{"hasNextPage":page.is_null(),"endCursor":"notes:+/"},
                        "nodes":[{"id":format!("gid://gitlab/Note/{id}"),"awardEmoji":{"nodes":[{"name":"heart","user":{"username":"viewer"}}]}}]}}}}}),
                    )
                }
            }
            _ => None,
        };
        if let Some(v) = native {
            return Some(Json(v).into_response());
        }
    }
    if kind == "forgejo" {
        let reactions = json!([{"content":"+1","user":{"login":"viewer"}},{"content":"+1","user":{"login":"reader"}}]);
        let native = match path {
            "repos/team/repo/issues/72" => Some(json!({"number":72,"state":"open","title":"locked","body":"story","is_locked":true,
                "user":{"login":"viewer"},"created_at":date,"updated_at":date,"html_url":"https://forgejo.example/team/repo/issues/72"})),
            "repos/team/repo/issues/72/comments" => Some(json!((1..=51).map(|id|json!({"id":id,"body":format!("note {id}"),"created_at":date,"user":{"login":"reader"}})).collect::<Vec<_>>())),
            "repos/team/repo/issues/72/timeline" if params["page"]=="1" => {
                let reference=json!({"type":"pull_ref","id":2,"created_at":date,"ref_issue":{"number":4,"pull_request":{},"repository":{"full_name":"third/repo"}}});
                Some(json!([reference,reference]))
            }
            "repos/team/repo/issues/72/timeline" => Some(Value::Null),
            "repos/team/repo/issues/72/reactions" => Some(reactions),
            path if path.starts_with("repos/team/repo/issues/comments/") && path.ends_with("/reactions") => Some(reactions),
            "repos/third/repo/pulls/4" => Some(json!({"number":4,"title":"fix","merged":true,"state":"closed","html_url":"https://forgejo.example/third/repo/pulls/4"})),
            _ => None,
        };
        if let Some(v) = native {
            return Some(Json(v).into_response());
        }
    }
    if kind == "tanstack-source" && path.ends_with("/issues/72") {
        let notes = if params.contains_key("cursor") {
            json!([{"id":1,"author":"viewer","comment":"old","at":1000}])
        } else {
            json!([
            {"id":2,"author":"viewer","comment":"new","at":2000},{"id":3,"comment":"hidden","moderation":"hidden","at":3000},{"event":"renamed","actor":"bot","at":3100}])
        };
        return Some(Json(json!({"number":72,"title":"Detail","kind":"issue","state":"open","locked":true,"author":"viewer","createdAt":1000,"body":"Hello","timeline":notes,
            "fixedBy":[{"number":9,"title":"Fix","state":"merged"}],"next":if params.contains_key("cursor"){None}else{Some("older")}})).into_response());
    }
    if kind == "azure-devops" {
        if path.ends_with("/workitems/72") {
            assert_eq!(params["$expand"], "relations");
            let link =
                json!({"rel":"ArtifactLink","url":"vstfs:///Git/PullRequestId/proj%2Frepo%2F9"});
            return Some(Json(json!({"id":72,"fields":{"System.TeamProject":"proj","System.Title":"regression","System.WorkItemType":"Task","System.State":"ResolvedQA",
                "System.CreatedDate":date,"System.Description":" ","Microsoft.VSTS.TCM.ReproSteps":"Reproduce"},"relations":[link,link]})).into_response());
        }
        if path.ends_with("/workitems/73") {
            return Some(
                Json(json!({"id":73,"fields":{"System.TeamProject":"other"}})).into_response(),
            );
        }
        if path.ends_with("/workitems/72/comments") {
            assert_eq!(params["order"], "desc");
            assert_eq!(params["$expand"], "reactions");
            assert_eq!(params["api-version"], "7.1-preview.4");
            let page = if params.contains_key("continuationToken") {
                assert_eq!(params["continuationToken"], "older +/");
                json!({"comments":[{"id":1,"text":"old","createdDate":date}]})
            } else {
                json!({"continuationToken":"older +/","comments":[{"id":4,"text":"gone","isDeleted":true,"createdDate":date},{"id":3,"text":"newer","createdDate":date},
                    {"id":2,"text":"older","createdDate":date,"createdBy":{"displayName":"Viewer","uniqueName":"v@example"},
                    "reactions":[{"type":"smile","count":2,"isCurrentUserEngaged":true},{"type":"unknown","count":1}]}]})
            };
            return Some(Json(page).into_response());
        }
        if path.ends_with("/workitems/72/updates") {
            assert_eq!(params["$top"], "100");
            return Some(Json(json!({"value":[{"id":1},{"id":2,"revisedDate":"9999-01-01",
                "fields":{"System.ChangedDate":{"newValue":date},"System.State":{"oldValue":"Doing","newValue":"ResolvedQA"},
                "System.Tags":{"oldValue":"red; green","newValue":"green; blue"},"System.AssignedTo":{"oldValue":"old","newValue":{"uniqueName":"new"}}}}]})).into_response());
        }
        if path == "org/_apis/git/pullrequests/9" {
            return Some(Json(json!({"pullRequestId":9,"title":"Board fix","status":"completed","repository":{"name":"sibling","project":{"name":"proj"}}})).into_response());
        }
        if path.ends_with("/workitemtypes") {
            return Some(Json(json!({"value":[{"name":"Task","states":[{"name":"ResolvedQA","category":"Resolved"}]}]})).into_response());
        }
        if path.ends_with("/wiql") && params.contains_key("timePrecision") {
            let q = b["query"].as_str().unwrap();
            assert!(q.contains("CONTAINS 'Issue ''search'''"));
            assert!(q.contains("NOT IN ('resolvedqa')"));
            assert_eq!(params["$top"], "2");
            let ids = if q.contains("AND [System.Id] < 72") {
                json!([{"id":71}])
            } else {
                json!([{"id":72},{"id":71}])
            };
            return Some(Json(json!({"workItems":ids})).into_response());
        }
        if path.ends_with("/workitemsbatch") && b["ids"][0].as_u64().is_some_and(|n| n >= 71) {
            let n = &b["ids"][0];
            return Some(Json(json!({"value":[{"id":n,"fields":{"System.TeamProject":"proj","System.CreatedDate":date,"System.ChangedDate":date,"System.Title":"selected","System.State":"Active","System.Tags":"red; blue"}}]})).into_response());
        }
    }
    if !path.ends_with("/issues") || !params.values().any(|v| v == "Issue 'search'") {
        return None;
    }
    if kind == "tanstack-source" {
        assert_eq!(params["label"], "red");
        assert_eq!(params["state"], "open");
        return Some(Json(json!({"items":[{"number":72,"title":"visible","createdAt":3000,"state":"open","author":"bot","labels":["red","blue"]},
            {"number":80,"title":"hidden","createdAt":2000,"state":"open","moderation":"hidden","labels":["red","blue"]},
            {"number":71,"title":"older","createdAt":1000,"state":"open","labels":["red","blue"]}],"next":null})).into_response());
    }
    if !matches!(kind, "gitlab" | "forgejo") {
        return None;
    }
    assert_eq!(params["labels"], "red,blue");
    let n = if params["page"] == "1" { 72 } else { 71 };
    let row = json!({"iid":n,"number":n,"state":"open","title":"result","web_url":"https://sample/issue","html_url":"https://sample/issue","created_at":date,"updated_at":date,
        "assignees":[],"labels":["red","blue"]});
    let mut res = Json(json!([row])).into_response();
    if kind == "forgejo" {
        res.headers_mut().insert(
            "link",
            if n == 72 {
                "<next>; rel=next"
            } else {
                "<previous>; rel=prev"
            }
            .parse()
            .unwrap(),
        );
    }
    Some(res)
}
async fn verify_search(c: &reqwest::Client, base: &str, project: &str, kind: &str) {
    if kind == "gitlab" {
        let root = format!("{base}/projects/{project}/forge/issues");
        let url = format!("{root}/72");
        let detail = ok(c.get(format!("{url}/detail"))).await;
        assert_eq!(detail["comments"].as_array().unwrap().len(), 1);
        assert_eq!(
            detail["comments"][0]["reactions"][0]["viewerHasReacted"],
            true
        );
        assert_eq!(detail["commentsCursor"], "2:notes:+/");
        assert_eq!(detail["labels"][0]["color"], "#ff0000");
        assert_eq!(detail["reactions"][0]["actors"], json!(["other"]));
        let events = detail["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        let reference = events.iter().find(|r| r["id"] == "note-31").unwrap();
        assert_eq!(
            reference["reference"]["url"],
            "https://gitlab.example/root/team/sibling/-/merge_requests/4"
        );
        assert!(events.iter().any(|r| r["id"] == "label-30"));
        let links = detail["linkedPullRequests"].as_array().unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["closesIssue"], true);
        let older = ok(c
            .get(format!("{url}/comments"))
            .query(&[("cursor", detail["commentsCursor"].as_str().unwrap())]))
        .await;
        assert_eq!(older["comments"][0]["id"], "8");
        assert_eq!(older["comments"][0]["reactions"][0]["content"], "heart");
        assert!(older["nextCursor"].is_null());
        let unavailable = ok(c.get(format!("{root}/74/comments"))).await;
        assert_eq!(unavailable["comments"][0]["reactions"], json!([]));
        assert_eq!(
            c.get(format!("{root}/75/comments"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            403
        );
        assert_eq!(
            c.get(format!("{url}/comments"))
                .query(&[("cursor", "0:x")])
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            400
        );
    }
    if kind == "azure-devops" {
        let issue = format!("{base}/projects/{project}/forge/issues/72");
        let detail = ok(c.get(format!("{issue}/detail"))).await;
        assert_eq!(detail["body"], "Reproduce");
        assert_eq!(
            detail["state"],
            json!({"name":"ResolvedQA","category":"closed"})
        );
        assert_eq!(detail["states"], json!([detail["state"]]));
        assert!(detail["viewer"].is_null());
        assert!(detail.get("reactions").is_none());
        let events = detail["events"].as_array().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|r| r["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["closed", "unlabeled", "labeled", "unassigned", "assigned"]
        );
        assert_eq!(events[4]["id"], "update-2-4");
        assert_eq!(events[4]["createdAt"], "2026-11-01T00:00:00Z");
        let notes = detail["comments"].as_array().unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0]["id"], "2");
        assert_eq!(notes[0]["author"]["login"], "v@example");
        assert_eq!(
            notes[0]["reactions"],
            json!([{"content":"laugh","count":2,"viewerHasReacted":true,"actors":[]}])
        );
        let linked = detail["linkedPullRequests"].as_array().unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0]["repository"], "proj/sibling");
        assert_eq!(linked[0]["closesIssue"], false);
        let more = ok(c
            .get(format!("{issue}/comments"))
            .query(&[("cursor", detail["commentsCursor"].as_str().unwrap())]))
        .await;
        assert_eq!(more["comments"][0]["id"], "1");
        assert!(more["nextCursor"].is_null());
        assert_eq!(
            c.get(format!("{issue}/comments"))
                .query(&[("cursor", "@unexpected")])
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            400
        );
        let other = format!("{base}/projects/{project}/forge/issues/73/comments");
        assert_eq!(c.get(other).send().await.unwrap().status().as_u16(), 404);
    }
    if kind == "forgejo" {
        let issue = format!("{base}/projects/{project}/forge/issues/72");
        let detail = ok(c.get(format!("{issue}/detail"))).await;
        assert_eq!(
            detail["viewerPermissions"],
            json!({"setState":true,"comment":false,"edit":true,"labels":false,"assignees":false})
        );
        assert_eq!(detail["comments"].as_array().unwrap().len(), 50);
        assert_eq!(detail["commentsCursor"], "2");
        let expected =
            json!([{"content":"thumbs-up","count":2,"actors":["reader"],"viewerHasReacted":true}]);
        assert_eq!(detail["comments"][0]["reactions"], expected);
        assert_eq!(detail["reactions"], expected);
        assert_eq!(detail["events"][1]["detail"], "#4");
        let linked = detail["linkedPullRequests"].as_array().unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0]["repository"], "third/repo");
        assert_eq!(linked[0]["state"], "merged");
        let end = ok(c.get(format!("{issue}/comments")).query(&[("cursor", "2")])).await;
        assert_eq!(end["comments"].as_array().unwrap().len(), 1);
        assert_eq!(end["comments"][0]["id"], "51");
        assert!(end["nextCursor"].is_null());
    }
    if kind == "tanstack-source" {
        let issue = format!("{base}/projects/{project}/forge/issues/72");
        let detail = ok(c.get(format!("{issue}/detail"))).await;
        assert_eq!(detail["viewerPermissions"]["comment"], false);
        assert_eq!(detail["events"][0]["kind"], "renamed");
        assert_eq!(detail["linkedPullRequests"][0]["closesIssue"], true);
        assert_eq!(detail["comments"].as_array().unwrap().len(), 1);
        assert!(detail["comments"][0].get("reactions").is_none());
        let older = ok(c
            .get(format!("{issue}/comments"))
            .query(&[("cursor", detail["commentsCursor"].as_str().unwrap())]))
        .await;
        assert_eq!(older["comments"][0]["id"], "1");
    }
    let target = format!("{base}/forge/issues/list");
    let mut input = json!({"projectId":project,"state":"open","query":"Issue 'search'","labels":["red","blue"],"limit":1});
    let first = ok(c.post(&target).json(&input)).await;
    assert_eq!(first["errors"], json!([]), "{kind} {first}");
    assert_eq!(first["entries"][0]["key"], "72", "{kind}");
    let next = &first["nextCursors"];
    assert_eq!(next.as_object().unwrap().len(), 1, "{kind}");
    input["cursors"] = next.clone();
    let more = ok(c.post(&target).json(&input)).await;
    assert_eq!(more["errors"], json!([]), "{kind}");
    assert_eq!(more["entries"][0]["key"], "71", "{kind}");
}
