use super::*;

fn line_fields(pos: Position) -> Value {
    match pos {
        Position::Added { new_line } => json!({"new_line":new_line}),
        Position::Deleted { old_line } => json!({"old_line":old_line}),
        Position::Context {
            old_line, new_line, ..
        } => json!({"old_line":old_line,"new_line":new_line}),
    }
}

fn counters(patch: &str, pos: Position) -> Option<(u64, u64)> {
    let mut at = None;
    for text in patch.lines() {
        if let Some(hunk) = text.strip_prefix("@@ -") {
            let (a, b) = hunk.split_once(" +")?;
            let start = |s: &str| s.split([',', ' ']).next()?.parse::<u64>().ok();
            at = Some((start(a)?, start(b)?));
            continue;
        }
        let Some((old, new)) = at.as_mut() else {
            continue;
        };
        match text.as_bytes().first().copied() {
            Some(b'+') => {
                if matches!(pos,Position::Added{new_line} if u64::from(new_line.get())==*new) {
                    return Some((*old, *new));
                }
                *new += 1;
            }
            Some(b'-') => {
                if matches!(pos,Position::Deleted{old_line} if u64::from(old_line.get())==*old) {
                    return Some((*old, *new));
                }
                *old += 1;
            }
            Some(b' ') => {
                *old += 1;
                *new += 1;
            }
            _ => {}
        }
    }
    None
}

fn range_end(hash: &str, pos: Position, patch: &str) -> Value {
    let mut v = line_fields(pos);
    let (old, new) = counters(patch, pos).unwrap_or_else(|| {
        (
            v["old_line"].as_u64().unwrap_or(0),
            v["new_line"].as_u64().unwrap_or(0),
        )
    });
    v["line_code"] = format!("{hash}_{old}_{new}").into();
    v["type"] = match pos {
        Position::Added { .. } => "new".into(),
        Position::Deleted { .. } => "old".into(),
        Position::Context { .. } => Value::Null,
    };
    v
}

async fn discussion(
    c: &Context,
    root: &Url,
    refs: &Value,
    comment: &Comment,
) -> Result<Value, ForgeError> {
    let old = comment.old_path.as_deref().unwrap_or(&comment.path);
    let mut p = line_fields(comment.position);
    for key in ["base_sha", "head_sha", "start_sha"] {
        p[key] = refs[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("GitLab diff revisions"))?
            .into();
    }
    p["position_type"] = "text".into();
    p["old_path"] = old.into();
    p["new_path"] = comment.path.clone().into();
    if let Some(start) = comment.start_position {
        let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
        hasher.update(comment.path.as_bytes());
        let hash = hasher
            .try_finalize()
            .map_err(|_| ForgeError::bad("file path digest collision"))?
            .to_string();
        let mut patch = String::new();
        if !matches!(
            (start, comment.position),
            (Position::Context { .. }, Position::Context { .. })
        ) {
            for page in 1..=50 {
                let mut url = child(root.clone(), &["diffs"]);
                url.query_pairs_mut()
                    .append_pair("per_page", "100")
                    .append_pair("page", &page.to_string());
                let Ok(data) = get(c, url).await else { break };
                let Some(rows) = data.as_array() else { break };
                if let Some(found) = rows
                    .iter()
                    .find(|r| r["old_path"] == old && r["new_path"] == comment.path)
                {
                    patch = found["diff"].as_str().unwrap_or("").into();
                    break;
                }
                if rows.len() < 100 {
                    break;
                }
            }
        }
        let a = range_end(&hash, start, &patch);
        let b = range_end(&hash, comment.position, &patch);
        if a["line_code"] != b["line_code"] {
            p["line_range"] = json!({"start":a,"end":b});
        }
    }
    Ok(json!({"body":comment.body,"position":p}))
}

async fn review_mutation(
    c: &Context,
    query: &str,
    vars: Value,
    key: &str,
) -> Result<(), ForgeError> {
    let mut u = c.api_base.clone();
    if u.path().trim_end_matches('/').ends_with("/v4") {
        u.path_segments_mut().unwrap().pop_if_empty().pop();
    }
    let v = send(
        c,
        Method::POST,
        child(u, &["graphql"]),
        Some(json!({"query":query,"variables":vars})),
        true,
    )
    .await?;
    if v["errors"].as_array().is_some_and(|xs| !xs.is_empty())
        || !v["data"][key]["errors"]
            .as_array()
            .is_some_and(Vec::is_empty)
    {
        return Err(ForgeError::uncertain(
            "GitLab did not confirm the review mutation",
        ));
    }
    Ok(())
}

pub(super) async fn submit(
    c: &Context,
    root: Url,
    n: u64,
    verdict: Verdict,
    body: &str,
    notes: &[Comment],
) -> Result<Value, ForgeError> {
    let mut writes = 0;
    let mut prepared = vec![];
    if !notes.is_empty() {
        let detail = get(c, root.clone()).await?;
        if number(&detail["iid"])? != n {
            return Err(invalid("GitLab MR number"));
        }
        for note in notes {
            prepared.push(discussion(c, &root, &detail["diff_refs"], note).await?);
        }
    }
    if verdict == Verdict::RequestChanges {
        let viewer = get(c, path(c, &["user"])?).await?;
        let name = viewer["username"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("GitLab viewer"))?;
        review_mutation(c,"mutation KarSelf($project:ID!,$iid:String!,$users:[String!]!){mergeRequestSetReviewers(input:{projectPath:$project,iid:$iid,reviewerUsernames:$users,operationMode:APPEND}){errors}}",json!({"project":c.repository,"iid":n.to_string(),"users":[name]}),"mergeRequestSetReviewers").await?;
        writes += 1;
        review_mutation(c,"mutation KarChanges($project:ID!,$iid:String!){mergeRequestRequestChanges(input:{projectPath:$project,iid:$iid}){errors}}",json!({"project":c.repository,"iid":n.to_string()}),"mergeRequestRequestChanges").await.map_err(|e|partial(writes,e))?;
        writes += 1;
    }
    for note in prepared {
        send(
            c,
            Method::POST,
            child(root.clone(), &["discussions"]),
            Some(note),
            true,
        )
        .await
        .map_err(|e| partial(writes, e))?;
        writes += 1;
    }
    if !body.trim().is_empty() {
        send(
            c,
            Method::POST,
            child(root.clone(), &["notes"]),
            Some(json!({"body":body})),
            true,
        )
        .await
        .map_err(|e| partial(writes, e))?;
        writes += 1;
    }
    if verdict == Verdict::Approve {
        send(c, Method::POST, child(root, &["approve"]), None, true)
            .await
            .map_err(|e| partial(writes, e))?;
        writes += 1;
    }
    Ok(json!({"number":n,"confirmed_writes":writes}))
}
