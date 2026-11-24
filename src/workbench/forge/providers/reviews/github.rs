use super::*;

fn endpoint(c: &Context) -> Result<Url, ForgeError> {
    let mut u = c.api_base.clone();
    if u.path().trim_end_matches('/').ends_with("/api/v3") {
        u.path_segments_mut().unwrap().pop_if_empty().pop();
    }
    Ok(child(u, &["graphql"]))
}

async fn gql(c: &Context, query: &str, variables: Value, write: bool) -> Result<Value, ForgeError> {
    let r = send(
        c,
        Method::POST,
        endpoint(c)?,
        Some(json!({"query":query,"variables":variables})),
        write,
    )
    .await?;
    if let Some(errors) = r["errors"].as_array().filter(|s| !s.is_empty()) {
        let status = if errors.iter().any(|r| r["type"] == "FORBIDDEN") {
            403
        } else if errors.iter().any(|r| r["type"] == "NOT_FOUND") {
            404
        } else if errors.iter().any(|r| r["type"] == "RATE_LIMITED") {
            429
        } else if write {
            return Err(ForgeError::uncertain(
                "GitHub did not confirm the GraphQL review mutation",
            ));
        } else {
            502
        };
        return Err(ForgeError::remote(
            status,
            "GitHub rejected the review operation",
        ));
    }
    r.get("data")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| {
            if write {
                ForgeError::uncertain("GitHub returned no mutation data")
            } else {
                invalid("GitHub GraphQL data")
            }
        })
}

const COMMENT: &str =
    "pageInfo{hasNextPage endCursor} nodes{id body createdAt author{login} url isMinimized}";
fn project_comments(c: &Value) -> Result<Value, ForgeError> {
    let nodes = c["nodes"]
        .as_array()
        .ok_or_else(|| invalid("review comments"))?;
    let mut comments = vec![];
    let mut hidden = 0;
    for row in nodes {
        if row["isMinimized"] == true {
            hidden += 1;
            continue;
        }
        comments.push(json!({"id":row["id"],"author":row["author"]["login"],"body":row["body"],"url":row["url"],"createdAt":row["createdAt"]}));
    }
    Ok(
        json!({"comments":comments,"nextCursor":(c["pageInfo"]["hasNextPage"]==true).then_some(&c["pageInfo"]["endCursor"]),"hiddenCount":hidden}),
    )
}

pub(super) async fn threads(
    c: &Context,
    n: u64,
    cursor: Option<&str>,
) -> Result<Value, ForgeError> {
    let (owner, repo) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("repository"))?;
    let query = format!(
        "query KarReviewThreads($owner:String!,$repo:String!,$number:Int!,$after:String){{repository(owner:$owner,name:$repo){{pullRequest(number:$number){{reviewThreads(first:100,after:$after){{pageInfo{{hasNextPage endCursor}} nodes{{id path isResolved comments(first:10){{{COMMENT}}}}}}}}}}}}}"
    );
    let data = gql(
        c,
        &query,
        json!({"owner":owner,"repo":repo,"number":n,"after":cursor}),
        false,
    )
    .await?;
    let p = &data["repository"]["pullRequest"]["reviewThreads"];
    let mut threads = vec![];
    for r in p["nodes"]
        .as_array()
        .ok_or_else(|| invalid("GitHub review threads"))?
    {
        let mut t = project_comments(&r["comments"])?;
        for (target, src) in [("id", "id"), ("path", "path"), ("resolved", "isResolved")] {
            t[target] = r[src].clone();
        }
        threads.push(t)
    }
    Ok(
        json!({"threads":threads,"nextCursor":(p["pageInfo"]["hasNextPage"]==true).then_some(&p["pageInfo"]["endCursor"])}),
    )
}

async fn thread(c: &Context, n: u64, id: &str, cursor: Option<&str>) -> Result<Value, ForgeError> {
    let fields = if cursor.is_some() {
        format!("comments(first:100,after:$after){{{COMMENT}}}")
    } else {
        String::new()
    };
    let vars = if cursor.is_some() {
        "$id:ID!,$after:String!"
    } else {
        "$id:ID!"
    };
    let q = format!(
        "query KarThread({vars}){{node(id:$id){{ ... on PullRequestReviewThread{{id isResolved viewerCanResolve viewerCanUnresolve viewerCanReply pullRequest{{number repository{{nameWithOwner}}}} {fields}}}}}}}"
    );
    let mut input = json!({"id":id});
    if let Some(after) = cursor {
        input["after"] = after.into();
    }
    let v = gql(c, &q, input, false).await?;
    let t = &v["node"];
    if t["id"] != id
        || t["pullRequest"]["number"] != n
        || !t["pullRequest"]["repository"]["nameWithOwner"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&c.repository))
    {
        return Err(ForgeError::remote(404, "thread outside the selected PR"));
    }
    Ok(t.clone())
}

pub(super) async fn execute(
    c: &Context,
    root: Url,
    op: &Operation,
    n: u64,
) -> Result<Value, ForgeError> {
    match op {
        Operation::ReviewThreads { cursor, .. } => threads(c, n, cursor.as_deref()).await,
        Operation::ThreadComments {
            thread_id, cursor, ..
        } => project_comments(&thread(c, n, thread_id, Some(cursor)).await?["comments"]),
        Operation::SubmitReview {
            verdict,
            body,
            comments,
            ..
        } => {
            let notes:Vec<_>=comments.iter().map(|comment|{
                let (side,line)=comment.position.point();
                let mut note=json!({"path":comment.path,"body":comment.body,"line":line,"side":if side==Side::Left{"LEFT"}else{"RIGHT"}});
                if let Some(start)=comment.start_position && start.point()!=(side,line) {
                    let (s,l)=start.point();
                    note["start_line"]=l.into();note["start_side"]=if s==Side::Left {"LEFT"}else{"RIGHT"}.into();
                }
                note
            }).collect();
            let r = send(
                c,
                Method::POST,
                child(root, &["reviews"]),
                Some(json!({"event":verdict.event(),"body":body,"comments":notes})),
                true,
            )
            .await?;
            Ok(json!({"number":n,"review_id":r["id"]}))
        }
        Operation::ReplyToThread {
            thread_id, body, ..
        } => {
            let t = thread(c, n, thread_id, None).await?;
            if t["viewerCanReply"] != true {
                return Err(ForgeError::remote(
                    403,
                    "GitHub does not grant reply access to this thread",
                ));
            }
            let r=gql(c,"mutation KarReply($id:ID!,$body:String!){addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$id,body:$body}){comment{id}}}",json!({"id":thread_id,"body":body}),true).await?;
            let id = r["addPullRequestReviewThreadReply"]["comment"]["id"]
                .as_str()
                .ok_or_else(|| ForgeError::uncertain("GitHub did not identify the posted reply"))?;
            Ok(json!({"number":n,"thread_id":thread_id,"comment_id":id}))
        }
        Operation::SetThreadResolution {
            thread_id,
            resolved,
            ..
        } => {
            let t = thread(c, n, thread_id, None).await?;
            if t["isResolved"] != *resolved {
                let (permission, method) = if *resolved {
                    ("viewerCanResolve", "resolveReviewThread")
                } else {
                    ("viewerCanUnresolve", "unresolveReviewThread")
                };
                if t[permission] != true {
                    return Err(ForgeError::remote(
                        403,
                        "GitHub does not grant the selected resolution change",
                    ));
                }
                let q = format!(
                    "mutation KarResolution($id:ID!){{{method}(input:{{threadId:$id}}){{thread{{id isResolved}}}}}}"
                );
                let r = gql(c, &q, json!({"id":thread_id}), true).await?;
                if r[method]["thread"]["id"] != *thread_id
                    || r[method]["thread"]["isResolved"] != *resolved
                {
                    return Err(ForgeError::uncertain(
                        "GitHub did not confirm the requested thread state",
                    ));
                }
            }
            Ok(json!({"number":n,"thread_id":thread_id,"resolved":resolved}))
        }
        _ => unreachable!(),
    }
}
