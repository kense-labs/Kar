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
