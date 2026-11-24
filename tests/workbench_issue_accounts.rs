//! Normal Kar HTTP and restart against an isolated Linear GraphQL peer.
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod server;
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use server::json as ok;
use std::{fs, path::Path};
async fn peer(headers: HeaderMap, Json(input): Json<Value>) -> Result<Json<Value>, StatusCode> {
    let token = headers.get("authorization").and_then(|v| v.to_str().ok());
    let name = match token {
        Some("sample-default") => "first",
        Some("sample-work") => "work",
        _ => return Err(StatusCode::UNAUTHORIZED),
    };
    let query = input["query"].as_str().unwrap();
    let data = if query.contains("KarTrackerAccount") {
        let second = input["variables"]["after"] == "next";
        json!({"viewer":{"id":name,"name":name},"organization":{"urlKey":"org"},"teams":{
            "nodes":[{"key":if second{"OPS"}else{"ENG"},"name":if second{"Operations"}else{"Engineering"}}],
            "pageInfo":{"hasNextPage":!second,"endCursor":"next"}}})
    } else if query.contains("KarLinearFullIssue") || query.contains("KarLinearIssueComments") {
        let full = query.contains("KarLinearFullIssue");
        let after = &input["variables"]["after"];
        let comment = json!({"id":if full{"new"}else{"old"},"body":"message","createdAt":"2026-10-03T00:00:00Z","url":"https://linear.app/message","user":{"id":"author","displayName":"leo"},"reactions":[{"emoji":"heart","user":{"id":"viewer","displayName":name}},{"emoji":"❤️","user":{"id":"author","displayName":"leo"}}]});
        if !full {
            assert_eq!(after, "older")
        }
        json!({"organization":{"urlKey":"org"},"issue":{"number":7,"identifier":"ENG-7","team":{"key":"ENG","states":{"nodes":[{"name":"Clone","type":"duplicate","position":1},{"name":"New","type":"unstarted","position":3}]}},
            "title":"Detail","url":"https://linear.app/org/issue/ENG-7","description":"Body","state":{"name":"Clone","type":"duplicate"},"createdAt":"2026-10-01T00:00:00Z","updatedAt":"2026-10-02T00:00:00Z",
            "comments":{"nodes":[comment],"pageInfo":{"hasNextPage":full,"endCursor":"older"}},"attachments":{"nodes":[{"url":"https://gitlab.com/group/sub/repo/-/merge_requests/8","title":"Fix","metadata":{"status":"merged"}}]},
            "history":{"nodes":[{"id":"h","createdAt":"2026-10-02T00:00:00Z","fromState":{"id":"new","type":"unstarted"},"toState":{"id":"done","type":"duplicate","name":"Clone"},"addedLabels":[],"removedLabels":[]}]}}})
    } else if query.contains("KarIssueDetailsViewer") {
        json!({"viewer":{"id":"viewer","displayName":name}})
    } else if query.contains("KarIssueListViewer") {
        json!({"viewer":{"displayName":name}})
    } else if query.contains("KarIssueList(") {
        let v = &input["variables"];
        let team = v["filter"]["team"]["key"]["eq"].as_str().unwrap();
        assert_eq!(v["filter"]["creator"]["displayName"]["eq"], name);
        assert_eq!(v["filter"]["and"][0]["labels"]["some"]["name"]["eq"], "bug");
        let id = if v["after"].is_null() { 8 } else { 4 };
        if !v["after"].is_null() {
            assert_eq!(team, "OPS");
            assert_eq!(v["after"], "OPS-after");
        }
        json!({"organization":{"urlKey":"org"},"issues":{"nodes":[{"number":id,"identifier":format!("{team}-{id}"),"team":{"key":team},"title":name,"url":format!("https://linear.app/org/issue/{team}-{id}"),
          "state":{"name":"New","type":"unstarted"},"createdAt":"2026-10-01T00:00:00Z","updatedAt":"2026-10-02T00:00:00Z","labels":{"nodes":[{"name":"bug"}]}}],
          "pageInfo":{"hasNextPage":id==8,"endCursor":format!("{team}-after")}}})
    } else if query.contains("KarScope") {
        let key = &input["variables"]["team"];
        json!({"organization":{"urlKey":"org"},"viewer":{"email":name},"teams":{"nodes":[{"key":key}]}})
    } else if query.contains("KarIssue") {
        let id = input["variables"]["id"].as_str().unwrap();
        let (team, n) = id.split_once('-').unwrap();
        json!({"issue":{"id":id,"identifier":id,"number":n.parse::<u64>().unwrap(),"title":name,"team":{"key":team},"state":{"name":"Todo"}}})
    } else {
        panic!("unexpected Linear query")
    };
    Ok(Json(json!({"data":data})))
}
#[tokio::test]
async fn accounts_and_team_bindings_survive_restart_without_cross_account_fallback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/graphql", listener.local_addr().unwrap());
    let host = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/graphql", post(peer)))
            .await
            .unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (state, allowed, native) = (
        root.join("state"),
        root.join("projects"),
        root.join("native"),
    );
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir(&native).unwrap();
    let vars = [("KAR_LINEAR_API_BASE", Path::new(&url).as_os_str())];
    let (mut service, base) = server::serve_with_env(&state, &allowed, &native, &vars).await;
    let http = server::client();
    let p = ok(http
        .post(format!("{base}/projects"))
        .json(&json!({"root":allowed,"name":"repo"})))
    .await;
    let p = p["id"].as_str().unwrap();
    let connect = format!("{base}/forge/issue-trackers/connect");
    let binding = format!("{base}/projects/{p}/forge/issue-tracker/binding");
    let before = ok(http.get(format!("{base}/forge/issue-trackers"))).await;
    assert_eq!(before["trackers"][0]["connected"], false);
    let failed = http
        .post(&connect)
        .json(&json!({"tracker":"linear","token":"unaccepted"}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status().as_u16(), 401);
    let default = ok(http
        .post(&connect)
        .json(&json!({"tracker":"linear","token":"sample-default"})))
    .await;
    assert_eq!(default["scopes"].as_array().unwrap().len(), 2);
    let work = ok(http
        .post(&connect)
        .json(&json!({"tracker":"linear","token":"sample-work","add_account":true})))
    .await;
    let account = &work["account_id"];
    let duplicate = http
        .post(&connect)
        .json(&json!({"tracker":"linear","token":"sample-work","add_account":true}))
        .send()
        .await
        .unwrap();
    assert!(!duplicate.status().is_success());
    ok(http.put(&binding).json(&json!({"binding":{"tracker":"linear","account_id":account,"scope":"ENG","additional_scopes":["OPS"]}}))).await;
    ok(http.put(&binding).json(&json!({"binding":null}))).await;
    service.kill().await.unwrap();
    let (mut service, base) = server::serve_with_env(&state, &allowed, &native, &vars).await;
    let query = format!("{base}/projects/{p}/forge/query");
    let existing = ok(http.get(format!("{base}/forge/issue-trackers"))).await;
    assert_eq!(existing["account_bindings"][p]["account_id"], *account);
    assert!(existing["bindings"].get(p).is_none());
    assert!(!existing.to_string().contains("sample-work"));
    let rich = format!("{base}/projects/{p}/forge/issues/7");
    let detail = ok(http
        .get(format!("{rich}/detail"))
        .query(&[("tracker", "linear"), ("scope", "ENG")]))
    .await;
    assert_eq!(detail["viewer"], "work");
    assert_eq!(detail["state"]["closeReason"], "duplicate");
    assert_eq!(detail["states"][0]["name"], "New");
    assert_eq!(detail["events"][0]["kind"], "closed");
    assert_eq!(
        detail["linkedPullRequests"][0]["repository"],
        "group/sub/repo"
    );
    let older = ok(http.get(format!("{rich}/comments")).query(&[
        ("scope", "ENG"),
        ("cursor", detail["commentsCursor"].as_str().unwrap()),
    ]))
    .await;
    assert_eq!(older["comments"][0]["id"], "old");
    assert_eq!(
        older["comments"][0]["reactions"][0],
        json!({"count":2,"actors":["leo"],"viewerHasReacted":true,"content":"heart"})
    );
    let endpoint = format!("{base}/forge/issues/list");
    let mut filters =
        json!({"state":"open","involvement":"authored","labels":["bug"],"projectId":p,"limit":1});
    let page = ok(http.post(&endpoint).json(&filters)).await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 2);
    assert_eq!(page["viewers"].as_object().unwrap().len(), 1);
    let cursors = page["nextCursors"].as_object().unwrap();
    let ops = cursors.keys().find(|k| k.ends_with("/ops")).unwrap();
    assert!(ops.contains(account.as_str().unwrap()));
    filters["cursors"] = json!({(ops):"OPS-after"});
    let page = ok(http.post(&endpoint).json(&filters)).await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["key"], "OPS-4");
    assert_eq!(page["nextCursors"], json!({}));
    for key in ["ENG", "OPS"] {
        let issue = ok(http
            .post(&query)
            .json(&json!({"op":"issue","number":7,"issue_scope":key})))
        .await;
        assert_eq!(issue["title"], "work");
        assert_eq!(issue["identifier"], format!("{key}-7"));
    }
    let bad = http
        .post(&query)
        .json(&json!({"op":"issue","number":7,"issue_scope":"OTHER"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status().as_u16(), 403);
    ok(http
        .post(format!("{base}/forge/issue-trackers/connect"))
        .json(&json!({"tracker":"linear","token":null,"account_id":account})))
    .await;
    let after = ok(http.get(format!("{base}/forge/issue-trackers"))).await;
    assert_eq!(after["accounts"].as_array().unwrap().len(), 0);
    assert!(after["account_bindings"].get(p).is_none());
    assert_eq!(after["trackers"][0]["account"], "first");
    service.kill().await.unwrap();
    host.abort();
}
