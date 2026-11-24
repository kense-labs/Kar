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
