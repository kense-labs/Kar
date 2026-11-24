use super::*;
pub(super) async fn create_issue(
    State(mock): State<Mock>,
    Json(input): Json<Value>,
) -> Json<Value> {
    assert_eq!(input["labels"], json!(["bug"]));
    assert_eq!(input["assignees"], json!(["kar-test"]));
    let row = json!({"number":8,"title":input["title"],"body":input["body"],"html_url":"https://review.example/team/repo/issues/8","state":"open","comments":0});
    let mut db = mock.lock().unwrap();
    assert!(db.created_issue.replace(row.clone()).is_none());
    Json(row)
}
pub(super) async fn comments(State(mock): State<Mock>) -> Json<Value> {
    Json(json!(mock.lock().unwrap().comments))
}
pub(super) async fn post_comment(State(mock): State<Mock>, Json(input): Json<Value>) -> Response {
    if input["body"] == "read-only" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut db = mock.lock().unwrap();
    let n = db.comments.len() + 1;
    let row = json!({"id":n,"body":input["body"],"user":{"login":"reviewer"},"created_at":"2026-01-01T00:00:00Z"});
    db.comments.push(row.clone());
    db.created_issue.as_mut().unwrap()["comments"] = json!(n);
    Json(row).into_response()
}
pub(super) async fn issue(client: &reqwest::Client, forge: &str, chat: Uuid) {
    let operations = format!("{forge}/operations");
    let create = json!({"id":Uuid::new_v4(),"chat":chat,"op":"create_issue","title":"New issue","body":"  original markdown","labels":["bug"],"assignees":["kar-test"]});
    let created = ok(client.post(&operations).json(&create)).await;
    assert_eq!(created["number"], 8);
    assert_eq!(ok(client.post(&operations).json(&create)).await, created);
    let text = format!("  unique comment {}", Uuid::new_v4());
    let post = json!({"id":Uuid::new_v4(),"chat":chat,"op":"comment_issue","number":8,"body":text});
    for _ in 0..2 {
        assert_eq!(
            ok(client.post(&operations).json(&post)).await["comment_id"],
            1
        );
    }
    let page = ok(client
        .post(format!("{forge}/query"))
        .json(&json!({"op":"issue_comments","number":8})))
    .await;
    let rows = page["comments"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0]["author"].as_str(),
            rows[0]["body"].as_str(),
            rows[0]["id"].as_str()
        ),
        (Some("reviewer"), Some(text.as_str()), Some("1"))
    );
    assert!(page["nextCursor"].is_null());
    let mut denied = post;
    denied["id"] = json!(Uuid::new_v4());
    denied["body"] = "read-only".into();
    let res = client.post(&operations).json(&denied).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(res.json::<Value>().await.unwrap()["kind"], "permission");
}

pub(super) async fn review_submission(
    State(mock): State<Mock>,
    Json(req): Json<Value>,
) -> Json<Value> {
    assert_eq!(req["event"], "COMMENT");
    assert_eq!(
        req["comments"][0],
        json!({"path":"notes.txt","line":4,"side":"RIGHT","start_line":2,"start_side":"RIGHT","body":"  inline text"})
    );
    let mut db = mock.lock().unwrap();
    assert!(db.review_comments.is_empty());
    db.review_comments.push(
        json!({"id":"note-1","body":req["comments"][0]["body"],"author":{"login":"kar-test"}}),
    );
    Json(json!({"id":42}))
}
pub(super) async fn review_control(
    State(mock): State<Mock>,
    Json(input): Json<Value>,
) -> Json<Value> {
    let q = input["query"].as_str().unwrap();
    let v = &input["variables"];
    let mut db = mock.lock().unwrap();
    let comments = json!({"nodes":db.review_comments,"pageInfo":{"hasNextPage":false}});
    let tid = "thread-1";
    let t = json!({"id":tid,"path":"notes.txt","isResolved":db.resolved,"viewerCanResolve":true,"viewerCanUnresolve":true,"viewerCanReply":true,"pullRequest":{"number":1,"repository":{"nameWithOwner":"team/repo"}},"comments":comments});
    let data = if q.starts_with("query KarReviewThreads") {
        json!({"repository":{"pullRequest":{"reviewThreads":{"nodes":[t],"pageInfo":{"hasNextPage":false}}}}})
    } else if q.starts_with("query KarThread") {
        json!({"node":if v["id"]==tid {t}else{Value::Null}})
    } else if q.starts_with("mutation KarReply") {
        assert_eq!(v["id"], tid);
        db.review_comments
            .push(json!({"id":"note-2","body":v["body"],"author":{"login":"reviewer"}}));
        json!({"addPullRequestReviewThreadReply":{"comment":{"id":"note-2"}}})
    } else {
        assert!(q.starts_with("mutation KarResolution"));
        assert_eq!(v["id"], tid);
        let resolve = !q.contains("unresolveReviewThread");
        db.resolved = resolve;
        json!({(if resolve {"resolveReviewThread"}else{"unresolveReviewThread"}):{"thread":{"id":tid,"isResolved":resolve}}})
    };
    Json(json!({"data":data}))
}
pub(super) async fn review(client: &reqwest::Client, forge: &str, chat: Uuid) {
    let ops = format!("{forge}/operations");
    let req = json!({"id":Uuid::new_v4(),"chat":chat,"op":"submit_review","number":1,"body":"Review","verdict":"comment","comments":[{"path":"notes.txt","position":{"kind":"added","newLine":4},"startPosition":{"kind":"added","newLine":2},"body":"  inline text"}]});
    for _ in 0..2 {
        assert_eq!(ok(client.post(&ops).json(&req)).await["review_id"], 42);
    }
    let list = json!({"op":"review_threads","number":1});
    let route = format!("{forge}/query");
    let first = ok(client.post(&route).json(&list)).await;
    let tid = &first["threads"][0]["id"];
    assert_eq!(first["threads"][0]["comments"][0]["author"], "kar-test");
    let reply = json!({"id":Uuid::new_v4(),"chat":chat,"op":"reply_to_thread","number":1,"thread_id":tid,"body":"reply"});
    for _ in 0..2 {
        assert_eq!(
            ok(client.post(&ops).json(&reply)).await["comment_id"],
            "note-2"
        );
    }
    for resolved in [true, false] {
        let input = json!({"id":Uuid::new_v4(),"chat":chat,"op":"set_thread_resolution","number":1,"thread_id":tid,"resolved":resolved});
        ok(client.post(&ops).json(&input)).await;
        let page = ok(client.post(&route).json(&list)).await;
        let t = &page["threads"][0];
        assert_eq!(t["id"], *tid);
        assert_eq!(t["resolved"], resolved);
        let rows = t["comments"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["body"], "reply");
        assert_eq!(rows[1]["author"], "reviewer");
    }
    let mut wrong = reply;
    wrong["id"] = json!(Uuid::new_v4());
    wrong["number"] = 2.into();
    assert_eq!(
        client
            .post(&ops)
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}
