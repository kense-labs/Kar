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
    for state in ["Open", "Not planned"] {
        let op = json!({"id":Uuid::new_v4(),"chat":chat,"op":"set_issue_state","number":7,"state":state});
        let value = ok(client.post(&operations).json(&op)).await;
        if state == "Not planned" {
            assert_eq!(value["state_reason"], "not_planned");
        }
    }
    let refused=client.post(&operations).json(&json!({"id":Uuid::new_v4(),"chat":chat,"op":"set_issue_state","number":7,"state":"Completed"})).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let edit = json!({"id":Uuid::new_v4(),"chat":chat,"op":"update_issue","number":7,"body":"","labels":[],"assignees":["fixture-bot"]});
    for _ in 0..2 {
        assert_eq!(ok(client.post(&operations).json(&edit)).await["number"], 7);
    }
    let mut denied_edit = edit.clone();
    denied_edit["id"] = json!(Uuid::new_v4());
    denied_edit["labels"] = json!(["triage-required"]);
    let refused = client
        .post(&operations)
        .json(&denied_edit)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let reconcile = client
        .post(format!(
            "{operations}/{}/reconcile",
            denied_edit["id"].as_str().unwrap()
        ))
        .json(&json!({"number":7}))
        .send()
        .await
        .unwrap();
    assert_eq!(reconcile.status(), StatusCode::CONFLICT);
    let reaction = json!({"id":Uuid::new_v4(),"chat":chat,"op":"set_issue_reaction","number":7,"subject_id":"comment-node","content":"heart","reacted":true});
    let mut foreign = reaction.clone();
    foreign["subject_id"] = json!("other-comment");
    foreign["id"] = json!(Uuid::new_v4());
    assert_eq!(
        client
            .post(&operations)
            .json(&foreign)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    for desired in [true, true, false] {
        let mut op = reaction.clone();
        op["id"] = json!(Uuid::new_v4());
        op["reacted"] = json!(desired);
        for _ in 0..2 {
            let res = ok(client.post(&operations).json(&op)).await;
            assert_eq!(
                (res["number"].as_u64(), res["reacted"].as_bool()),
                (Some(7), Some(desired))
            );
        }
    }
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
    if let Some(data) = super::viewed::github(&mut db, q, v) {
        return Json(json!({"data":data}));
    }
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
    } else if q.starts_with("query KarFullIssue") {
        let pull = json!({"__typename":"PullRequest","repository":{"nameWithOwner":"team/repo"},"number":11,"title":"Fix","state":"OPEN","url":"https://review.example/team/repo/pull/11","isDraft":false});
        json!({"viewer":{"login":"kar-test"},"repository":{"viewerPermission":"TRIAGE","issue":{"id":"issue-7","number":7,"title":"Detail","url":"https://review.example/team/repo/issues/7","state":"OPEN","body":"Body",
            "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-02-01T00:00:00Z","viewerCanUpdate":false,"viewerCanClose":true,"viewerCanReopen":false,
            "labels":{"nodes":[{"name":"one"}],"pageInfo":{"hasNextPage":true,"endCursor":"labels-next"}},"assignees":{"nodes":[]},"comments":{"nodes":[],"totalCount":21,"pageInfo":{"hasPreviousPage":true,"startCursor":"older"}},
            "closedByPullRequestsReferences":{"nodes":[pull]},"timelineItems":{"nodes":[{"id":"x","createdAt":"2026-02-01T00:00:00Z","__typename":"CrossReferencedEvent","source":pull}]}}}})
    } else if q.starts_with("query KarIssueCollection") {
        assert_eq!(v["after"], "labels-next");
        json!({"repository":{"issue":{"id":"issue-7","labels":{"nodes":[{"name":"two"}],"pageInfo":{"hasNextPage":false}}}}})
    } else if q.starts_with("query KarIssueCommentPage") {
        assert_eq!(v["before"], "older");
        json!({"viewer":{"login":"kar-test"},"repository":{"issue":{"number":7,"comments":{"pageInfo":{"hasPreviousPage":false},"nodes":[{"id":"first","body":"Earlier","createdAt":"2026-01-01T00:00:00Z","author":{"login":"kar-test"},"reactionGroups":[{"content":"HEART","viewerHasReacted":true,"reactors":{"totalCount":4,"nodes":[{"login":"kar-test"},{"login":"second"}]}}]}]}}}})
    } else if q.starts_with("query KarIssueSearch") {
        let search = v["q"].as_str().unwrap();
        assert!(search.contains("is:closed"));
        assert!(search.contains("sort:created-desc"));
        assert!(search.contains("label:\"release bug\""));
        assert!(search.contains("mentions:\"kar-test\""));
        assert!(search.contains("\"repo:foreign/repo\""));
        if v["cursor"] == "lost-page" {
            return Json(json!({"errors":[{"type":"RATE_LIMITED","message":"limited"}]}));
        }
        let next = v["cursor"].is_null();
        let n = if next { 7 } else { 3 };
        json!({"search":{"nodes":[{"number":n,"repository":{"nameWithOwner":"team/repo"},"title":"Closed item","state":"CLOSED","stateReason":"NOT_PLANNED",
            "url":format!("https://review.example/team/repo/issues/{n}"),"createdAt":"2026-03-01T01:00:00Z","updatedAt":"2026-04-01T01:00:00Z","labels":{"nodes":[{"name":"release bug"}]},"assignees":{"nodes":[]},"comments":{"totalCount":2}},
            {"number":1,"repository":{"nameWithOwner":"other/repo"}}],"pageInfo":{"hasNextPage":next,"endCursor":if next{Some("page2")}else{None}}}})
    } else if q.starts_with("query KarIssueReaction") {
        assert_eq!(v["n"], 7);
        let parent = if v["node"] == "comment-node" {
            "issue-7"
        } else {
            "issue-9"
        };
        json!({"repository":{"issue":{"id":"issue-7"}},"node":{"__typename":"IssueComment","id":v["node"],"issue":{"id":parent},"reactionGroups":[{"content":"HEART","viewerHasReacted":db.issue_reacted}]}})
    } else if q.starts_with("mutation KarSetIssueReaction") {
        assert_eq!(v["node"], "comment-node");
        assert_eq!(v["content"], "HEART");
        let add = q.contains("addReaction");
        assert_ne!(add, db.issue_reacted);
        db.issue_reacted = add;
        json!({(if add {"addReaction"}else{"removeReaction"}):{"subject":{"id":"comment-node"}}})
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

pub(super) async fn link_second_project(
    c: &reqwest::Client,
    base: &str,
    root: &Path,
) -> (Uuid, Uuid) {
    let dir = root.join("second-forge-project");
    fs::create_dir(&dir).unwrap();
    git(&dir, &["init", "-b", "main"]);
    git(
        &dir,
        &[
            "remote",
            "add",
            "origin",
            "https://review.example/team/repo.git",
        ],
    );
    let project = ok(c
        .post(format!("{base}/projects"))
        .json(&json!({"name":"second","root":dir})))
    .await;
    let p: Uuid = serde_json::from_value(project["id"].clone()).unwrap();
    let chat = Uuid::new_v4();
    let url = format!("{base}/projects/{p}");
    ok(c.post(format!("{url}/chats"))
        .json(&json!({"runtime":"codex","id":chat})))
    .await;
    for _ in 0..2 {
        ok(c.post(format!("{url}/forge/links"))
            .json(&json!({"id":Uuid::new_v4(),"chat":chat,"kind":"issue","number":7})))
        .await;
    }
    assert_eq!(
        ok(c.get(format!("{url}/forge/links")))
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let meta = format!("{url}/chats/{chat}/metadata");
    ok(c.post(&meta)
        .json(&json!({"command_id":Uuid::new_v4(),"action":"rename","title":"Second copy"})))
    .await;
    ok(c.post(&meta)
        .json(&json!({"command_id":Uuid::new_v4(),"action":"settle"})))
    .await;
    ok(c.put(format!("{url}/chats/{chat}/archive"))
        .json(&json!({"archived":true})))
    .await;
    (p, chat)
}

pub(super) async fn linked(
    c: &reqwest::Client,
    base: &str,
    second: (Uuid, Uuid),
    primary: &str,
) -> Value {
    let endpoint = format!("{base}/forge/linked-chats");
    let query = |kind, repository| {
        c.get(&endpoint)
            .query(&[("kind", kind), ("repository", repository), ("number", "7")])
    };
    let rows = ok(query("issue", "github://Review.Example/TEAM/repo")).await;
    let chats = rows["threads"].as_array().unwrap();
    assert!(chats.iter().any(|r| r["projectId"] == primary));
    assert_eq!(
        chats
            .iter()
            .filter(|r| r["id"] == second.1.to_string())
            .count(),
        1
    );
    let newest = &chats[0];
    assert_eq!(newest["projectId"], second.0.to_string());
    assert_eq!(newest["title"], "Second copy");
    assert_eq!(newest["settled"], true);
    assert_eq!(newest["archived"], true);
    assert!(
        newest["archivedAt"]
            .as_str()
            .unwrap()
            .parse::<jiff::Timestamp>()
            .is_ok()
    );
    for (kind, repository) in [
        ("pull", "github://review.example/team/repo"),
        ("issue", "github://other.example/team/repo"),
    ] {
        assert_eq!(ok(query(kind, repository)).await["threads"], json!([]));
    }
    rows
}

pub(super) async fn issue_pages(c: &reqwest::Client, base: &str, projects: &[&str]) {
    let detail_url = format!("{base}/projects/{}/forge/issues/7/detail", projects[0]);
    let detail = ok(c.get(&detail_url)).await;
    assert_eq!(
        detail["viewerPermissions"],
        json!({"comment":true,"setState":true,"edit":false,"labels":true,"assignees":true})
    );
    assert_eq!(detail["labels"].as_array().unwrap().len(), 2);
    assert_eq!(detail["linkedPullRequests"].as_array().unwrap().len(), 1);
    assert_eq!(detail["linkedPullRequests"][0]["closesIssue"], true);
    assert_eq!(detail["events"][0]["reference"]["kind"], "pull-request");
    let foreign = ok(c.get(&detail_url).query(&[("scope", "foreign/repo")])).await;
    assert_eq!(foreign["writable"], false);
    let older = ok(c
        .get(format!(
            "{base}/projects/{}/forge/issues/7/comments",
            projects[0]
        ))
        .query(&[("cursor", detail["commentsCursor"].as_str().unwrap())]))
    .await;
    assert_eq!(older["comments"][0]["author"]["login"], "kar-test");
    assert_eq!(
        older["comments"][0]["reactions"][0],
        json!({"count":4,"content":"heart","viewerHasReacted":true,"actors":["second"]})
    );
    let endpoint = format!("{base}/forge/issues/list");
    let mut input = json!({"state":"closed","involvement":"mentioned","sort":"created","limit":1,"labels":["release bug"],"query":"repo:foreign/repo"});
    let first = ok(c.post(&endpoint).json(&input)).await;
    let entries = first["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["state"]["closeReason"], "not-planned");
    assert_eq!(entries[0]["key"], "7");
    assert_eq!(first["viewers"]["review.example"], "kar-test");
    assert_eq!(first["trackers"][0]["projectCount"], projects.len());
    assert_eq!(first["scopes"].as_array().unwrap().len(), 1);
    let scope = "review.example team/repo";
    assert_eq!(first["nextCursors"][scope], "page2");
    input["cursors"] = first["nextCursors"].clone();
    let second = ok(c.post(&endpoint).json(&input)).await;
    assert_eq!(second["entries"][0]["key"], "3");
    assert_eq!(second["nextCursors"], json!({}));
    input["cursors"] = json!({(scope):"lost-page"});
    let failed = ok(c.post(&endpoint).json(&input)).await;
    assert_eq!(failed["errors"].as_array().unwrap().len(), 1);
    assert_eq!(failed["nextCursors"], input["cursors"]);
    input["cursors"] = json!({"elsewhere team/repo":"unused"});
    let skipped = ok(c.post(&endpoint).json(&input)).await;
    assert_eq!(skipped["entries"], json!([]));
    assert_eq!(skipped["viewers"], json!({}));
    input["excludeProjectIds"] = json!(projects);
    let excluded = ok(c.post(&endpoint).json(&input)).await;
    assert_eq!(excluded["scopes"], json!([]));
}
