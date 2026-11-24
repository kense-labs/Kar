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
    let data = if q.starts_with("query KarFileStats") {
        let page = v["after"]
            .as_str()
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(0);
        let nodes = if page == 0 {
            json!([{"path":"Cargo.lock","additions":10,"deletions":0},{"path":"tests/__mocks__/a.txt","additions":3,"deletions":2},{"path":"Latest.java","additions":4,"deletions":0},{"path":"BillTest.java","additions":2,"deletions":1}])
        } else {
            json!([{"path":"CHANGELOG.md","additions":1,"deletions":3},{"path":"requirements-dev.txt","additions":6,"deletions":4}])
        };
        json!({"repository":{"pullRequest":{"files":{"nodes":nodes,"pageInfo":{"hasNextPage":v["number"]==9||page==0,"endCursor":(page+1).to_string()}}}}})
    } else if q.starts_with("query KarIssueDirectory") {
        if q.contains("issueTemplates") {
            json!({"repository":{"issueTemplates":[],"templateDirectory":{"__typename":"Tree","entries":[{"name":"config.yml","object":{"__typename":"Blob","isTruncated":false,"text":"blank_issues_enabled: false"}},{"name":"bug.yml","object":{"__typename":"Blob","isTruncated":false,"text":"name: Bug\ndescription: Report\nlabels: bug, alpha\nbody:\n - type: input\n   attributes:\n     label: Reproduce\n   validations:\n     required: true"}}]}}})
        } else {
            json!({"repository":{"labels":{"nodes":[{"name":"bug","color":"ccaa00"}],"pageInfo":{"hasNextPage":true}},"assignableUsers":{"nodes":[{"login":"kar-test","avatarUrl":"https://test.invalid/avatar"}],"pageInfo":{"hasNextPage":false}}}})
        }
    } else if q.starts_with("query KarReviewThreads") {
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

pub(super) async fn starred(State(mock): State<Mock>) -> StatusCode {
    if mock.lock().unwrap().starred {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
pub(super) async fn star(State(mock): State<Mock>) -> StatusCode {
    mock.lock().unwrap().starred = true;
    StatusCode::NO_CONTENT
}
pub(super) async fn routing(c: &reqwest::Client, forge: &str, base: &str, chat: Uuid) {
    let identity = ok(c
        .get(format!("{base}/forge/routing-identity"))
        .query(&[("host", "REVIEW.EXAMPLE")]))
    .await;
    assert_eq!(
        identity,
        json!({"host":"review.example","provider":"github","accountId":"5432","viewer":"kar-test"})
    );
    let project = ok(c.get(format!("{forge}/routing"))).await;
    assert_eq!(project["accountId"], identity["accountId"]);
    assert_eq!(project["projectTitle"], "repo");
    let req =
        json!({"op":"pull","number":1,"routing":{"host":"review.example","account_id":"5432"}});
    let read = ok(c.post(format!("{forge}/query")).json(&req)).await;
    assert_eq!(read["number"], 1);
    let denied = json!({"id":Uuid::new_v4(),"chat":chat,"op":"create_issue","title":"wrong account","routing":{"host":"review.example","account_id":"999"}});
    assert_eq!(
        c.post(format!("{forge}/operations"))
            .json(&denied)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        c.get(format!("{forge}/pulls/1/diff"))
            .header("x-kar-forge-host", "wrong.example")
            .header("x-kar-forge-account-id", "5432")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let star = format!("{forge}/starred");
    assert_eq!(ok(c.get(&star)).await, false);
    for _ in 0..2 {
        assert_eq!(ok(c.put(&star)).await, true);
    }
    assert_eq!(ok(c.get(&star)).await, true);
}

pub(super) async fn repository(c: &reqwest::Client, base: &str, bare: &Path, root: &Path) {
    let lookup = format!("{base}/forge/repositories/lookup");
    let clone = format!("{base}/forge/repositories/clone");
    let row = ok(c
        .post(&lookup)
        .json(&json!({"provider":"github","host":"review.example","repository":"team/repo"})))
    .await;
    assert_eq!(
        row,
        json!({"provider":"github","nameWithOwner":"team/repo","url":"https://review.example/team/repo","sshUrl":"git@review.example:team/repo.git"})
    );
    assert_eq!(
        c.post(&lookup)
            .json(&json!({"provider":"gitlab","host":"review.example","repository":"team/repo"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let dest = root.join("nested/deeper/clone");
    let input = json!({"source":bare,"directory":dest});
    let data = ok(c.post(&clone).json(&input)).await;
    assert_eq!(data["cwd"], json!(dest));
    assert!(data["repository"].is_null());
    assert_eq!(
        git(&dest, &["rev-parse", "refs/remotes/origin/feature"]),
        git(bare, &["rev-parse", "feature"])
    );
    assert_eq!(
        c.post(&clone).json(&input).send().await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}
