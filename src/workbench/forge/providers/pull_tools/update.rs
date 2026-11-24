use super::*;

async fn node_mutation(
    c: &Context,
    raw: &Value,
    field: &str,
    kind: &str,
    mut input: Value,
) -> Result<Value, ForgeError> {
    let id = raw["node_id"]
        .as_str()
        .ok_or_else(|| ForgeError::bad("GitHub pull node is missing"))?;
    input["pullRequestId"] = id.into();
    let query =
        format!("mutation KarPull($input:{kind}!){{{field}(input:$input){{pullRequest{{id}}}}}}");
    let mut url = c.api_base.clone();
    if url.path().trim_end_matches('/').ends_with("/api/v3") {
        url.set_path("/api/graphql");
    } else {
        url = issue::child(url, &["graphql"]);
    }
    let response = send(
        c,
        Method::POST,
        url,
        Some(json!({"query":query,"variables":{"input":input}})),
        true,
    )
    .await?;
    if response["errors"].as_array().is_some_and(|v| !v.is_empty())
        || response["data"][field]["pullRequest"]["id"] != id
    {
        return Err(ForgeError::bad("GitHub rejected the pull transition"));
    }
    Ok(response)
}
pub(super) async fn apply(
    c: &Context,
    repo: Url,
    pull: Url,
    raw: &Value,
    action: &Request,
) -> Result<Value, ForgeError> {
    let n = checked_number(raw)?.to_string();
    match action {
        Request::Merge {
            expected_head_sha,
            method,
            auto,
        } => {
            if raw.pointer("/head/sha").and_then(Value::as_str) != Some(expected_head_sha) {
                return Err(ForgeError::bad("pull head changed"));
            }
            let method = method.as_deref().unwrap_or("merge");
            if !matches!(method, "merge" | "rebase" | "squash") {
                return Err(ForgeError::bad("unknown merge method"));
            }
            if *auto {
                return node_mutation(c,raw,"enablePullRequestAutoMerge","EnablePullRequestAutoMergeInput",json!({"expectedHeadOid":expected_head_sha,"mergeMethod":method.to_uppercase()})).await;
            }
            let merged = send(
                c,
                Method::PUT,
                issue::child(pull, &["merge"]),
                Some(json!({"sha":expected_head_sha,"merge_method":method})),
                true,
            )
            .await?;
            if merged["merged"] != true {
                return Err(ForgeError::bad("GitHub did not merge the pull"));
            }
            Ok(merged)
        }
        Request::Update {
            title,
            body,
            state,
            ready,
            add_labels,
            remove_labels,
            request_reviewers,
        } => {
            if title.as_ref().is_some_and(|s| s.trim().is_empty())
                || state
                    .as_deref()
                    .is_some_and(|v| !matches!(v, "open" | "closed"))
            {
                return Err(ForgeError::bad("invalid title or pull state"));
            }
            for names in [add_labels, remove_labels, request_reviewers]
                .into_iter()
                .flatten()
            {
                if names.is_empty()
                    || names.len() > 100
                    || names.iter().any(|s| s.trim().is_empty() || s.len() > 256)
                {
                    return Err(ForgeError::bad("invalid labels or reviewers"));
                }
            }
            let owner = c.repository.split('/').next().unwrap_or("");
            let mut users = vec![];
            let mut teams = vec![];
            for login in request_reviewers.iter().flatten() {
                if let Some((org, team)) = login.split_once('/') {
                    if !org.eq_ignore_ascii_case(owner) || team.is_empty() || team.contains('/') {
                        return Err(ForgeError::bad(
                            "team reviewer must belong to the repository owner",
                        ));
                    }
                    teams.push(team);
                } else {
                    users.push(login);
                }
            }
            let mut edit = json!({});
            if let Some(v) = title {
                edit["title"] = v.clone().into();
            }
            if let Some(v) = body {
                edit["body"] = v.clone().into();
            }
            if let Some(v) = state {
                edit["state"] = v.clone().into();
            }
            let labels = issue::child(repo, &["issues", &n, "labels"]);
            let mut changes = Vec::new();
            if edit.as_object().is_some_and(|o| !o.is_empty()) {
                changes.push((Method::PATCH, pull.clone(), Some(edit)));
            }
            if let Some(v) = add_labels {
                changes.push((Method::POST, labels.clone(), Some(json!({"labels":v}))));
            }
            for v in remove_labels.iter().flatten() {
                changes.push((Method::DELETE, issue::child(labels.clone(), &[v]), None));
            }
            if request_reviewers.is_some() {
                changes.push((
                    Method::POST,
                    issue::child(pull.clone(), &["requested_reviewers"]),
                    Some(json!({"reviewers":users,"team_reviewers":teams})),
                ));
            }
            if changes.is_empty() && ready.is_none() {
                return Err(ForgeError::bad("no pull update requested"));
            }
            let mut applied = 0;
            let failure = |count: usize, error: ForgeError| {
                if count == 0 {
                    error
                } else {
                    ForgeError::uncertain(format!(
                        "{count} update operations completed before {}: {}",
                        error.kind, error.detail
                    ))
                }
            };
            for (m, u, b) in changes {
                send(c, m, u, b, true)
                    .await
                    .map_err(|e| failure(applied, e))?;
                applied += 1;
            }
            if let Some(ready) = ready {
                let (field, kind) = if *ready {
                    (
                        "markPullRequestReadyForReview",
                        "MarkPullRequestReadyForReviewInput",
                    )
                } else {
                    (
                        "convertPullRequestToDraft",
                        "ConvertPullRequestToDraftInput",
                    )
                };
                node_mutation(c, raw, field, kind, json!({}))
                    .await
                    .map_err(|e| failure(applied, e))?;
            }
            Ok(json!({"number":raw["number"],"url":raw["html_url"],"updated":true}))
        }
        _ => Err(ForgeError::bad("unsupported update")),
    }
}
