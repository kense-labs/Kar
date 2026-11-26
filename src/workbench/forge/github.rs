use super::*;
use reqwest::Method;

pub(super) fn pull(v: &Value) -> Result<Value, ForgeError> {
    let number = checked_number(v)?;
    let state = if v["merged_at"].is_string() || v["merged"] == true {
        "merged"
    } else {
        v["state"]
            .as_str()
            .ok_or_else(|| ForgeError::bad("GitHub PR has no state"))?
    };
    Ok(
        json!({"number":number,"title":v["title"],"body":v["body"],"state":state,"url":v["html_url"],"base_sha":v.pointer("/base/sha"),"head_sha":v.pointer("/head/sha"),
        "mergeable":v["mergeable"],"mergeable_state":v["mergeable_state"],"draft":v["draft"],"author":v.pointer("/user/login"),"auto_merge_enabled":v.get("auto_merge").map(|a|!a.is_null()),"head_repository":v.pointer("/head/repo/full_name"),"head_ref":v.pointer("/head/ref"),"base_ref":v.pointer("/base/ref"),"updated_at":v["updated_at"]}),
    )
}
fn issue(v: &Value) -> Result<Value, ForgeError> {
    if v.get("pull_request").is_some() {
        return Err(ForgeError::remote(404, "number denotes a pull request"));
    }
    Ok(
        json!({"number":checked_number(v)?,"title":v["title"],"body":v["body"],"state":v["state"],"state_reason":v["state_reason"],"url":v["html_url"],"updated_at":v["updated_at"]}),
    )
}
pub(super) fn state_target(name: &str) -> Result<(&'static str, Option<&'static str>), ForgeError> {
    match name {
        "open" | "Open" => Ok(("open", None)),
        "closed" | "Closed" => Ok(("closed", None)),
        "Completed" => Ok(("closed", Some("completed"))),
        "Not planned" => Ok(("closed", Some("not_planned"))),
        "Duplicate" => Ok(("closed", Some("duplicate"))),
        _ => Err(ForgeError::bad("unknown GitHub issue state")),
    }
}
pub(super) fn state_matches(row: &Value, name: &str) -> Result<bool, ForgeError> {
    let (state, reason) = state_target(name)?;
    Ok(row["state"] == state && reason.is_none_or(|reason| row["state_reason"] == reason))
}
pub(super) async fn request(
    c: &Context,
    method: Method,
    parts: &[&str],
    query: &[(&str, &str)],
    body: Option<Value>,
) -> Result<Value, ForgeError> {
    let mut url = c.api_base.clone();
    url.path_segments_mut()
        .map_err(|_| ForgeError::bad("invalid GitHub API URL"))?
        .pop_if_empty()
        .extend(parts);
    let mutate = method != Method::GET;
    let mut req = c
        .client
        .request(method, url)
        .query(query)
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = &c.token {
        req = req.bearer_auth(t)
    }
    if let Some(body) = body {
        req = req.json(&body)
    }
    let response = req.send().await.map_err(|e| {
        if mutate {
            ForgeError::uncertain("GitHub write acknowledgement unavailable")
        } else {
            ForgeError::remote(502, e.without_url().to_string())
        }
    })?;
    let status = response.status();
    if status.as_u16() == 401 {
        super::signin::reject(c);
    }
    if !status.is_success() {
        if status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN
                && (response.headers().contains_key("retry-after")
                    || response
                        .headers()
                        .get("x-ratelimit-remaining")
                        .is_some_and(|v| v == "0")))
        {
            return Err(ForgeError::remote(429, "GitHub rate limit reached"));
        }
        if mutate && status.is_server_error() {
            return Err(ForgeError::uncertain(format!(
                "GitHub returned HTTP {status} after the write request"
            )));
        }
        return Err(ForgeError::remote(
            status.as_u16(),
            format!("GitHub returned HTTP {status}"),
        ));
    }
    response.json().await.map_err(|_| {
        if mutate {
            ForgeError::uncertain("GitHub accepted the request but the response was unreadable")
        } else {
            ForgeError::remote(502, "invalid GitHub JSON response")
        }
    })
}
pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let path: Vec<_> = c.repository.split('/').collect();
    let [owner, repo] = path.as_slice() else {
        return Err(ForgeError::bad("GitHub identity requires owner/repository"));
    };
    let mut parts = vec!["repos", *owner, *repo];
    match op {
        Operation::PullCommand { number, command } => {
            crate::workbench::forge::providers::pull_tools::execute(c, *number, command).await
        }
        op @ (Operation::SubmitReview { .. }
        | Operation::ReplyToThread { .. }
        | Operation::SetThreadResolution { .. }
        | Operation::ReviewThreads { .. }
        | Operation::ThreadComments { .. }) => providers::reviews::execute(c, op).await,
        Operation::SetIssueReaction { .. } => providers::reactions::execute(c, op, true).await,
        op @ (Operation::UpdateIssue { .. }
        | Operation::CreateIssue { .. }
        | Operation::CommentIssue { .. }
        | Operation::IssueComments { .. }) => providers::issue::execute(c, op).await,
        Operation::Identity => {
            let v = request(c, Method::GET, &parts, &[], None).await?;
            let full = v["full_name"]
                .as_str()
                .ok_or_else(|| ForgeError::remote(502, "GitHub repository identity missing"))?;
            if !full.eq_ignore_ascii_case(&c.repository) {
                return Err(ForgeError::bad("GitHub returned a different repository"));
            }
            let account = if c.token.is_some() {
                request(c, Method::GET, &["user"], &[], None).await?["login"].clone()
            } else {
                Value::Null
            };
            Ok(
                json!({"host":c.host,"repository":full,"account":account,"permissions":v["permissions"],"default_branch":v["default_branch"]}),
            )
        }
        Operation::Pull { number } | Operation::Issue { number } => {
            let is_pull = matches!(op, Operation::Pull { .. });
            let n = number.to_string();
            parts.extend([if is_pull { "pulls" } else { "issues" }, &n]);
            let v = request(c, Method::GET, &parts, &[], None).await?;
            if checked_number(&v)? != *number {
                return Err(ForgeError::remote(
                    502,
                    "GitHub returned a different object",
                ));
            }
            if is_pull { pull(&v) } else { issue(&v) }
        }
        Operation::Pulls | Operation::Issues => {
            let is_pull = matches!(op, Operation::Pulls);
            parts.push(if is_pull { "pulls" } else { "issues" });
            let mut result = Vec::new();
            for page in 1.. {
                let v = request(
                    c,
                    Method::GET,
                    &parts,
                    &[
                        ("per_page", "100"),
                        ("page", &page.to_string()),
                        ("state", "open"),
                    ],
                    None,
                )
                .await?;
                let rows = v
                    .as_array()
                    .ok_or_else(|| ForgeError::remote(502, "GitHub list expected"))?;
                for row in rows {
                    if is_pull {
                        result.push(pull(row)?)
                    } else if row.get("pull_request").is_none() {
                        result.push(issue(row)?)
                    }
                }
                if rows.len() < 100 {
                    break;
                }
            }
            Ok(json!(result))
        }
        Operation::CreatePull {
            head,
            head_repository,
            base,
            title,
            body,
            draft,
        } => {
            parts.push("pulls");
            let mut payload = json!({"head":head,"base":base,"title":title,"body":body});
            if *draft {
                payload["draft"] = json!(true);
            }
            if let Some(source) = head_repository
                .as_ref()
                .filter(|s| !s.eq_ignore_ascii_case(&c.repository))
            {
                let (owner, repo) = source
                    .split_once('/')
                    .ok_or_else(|| ForgeError::bad("expected owner/repository"))?;
                payload["head"] = json!(format!("{owner}:{head}"));
                payload["head_repo"] = json!(repo);
            }
            pull(&request(c, Method::POST, &parts, &[], Some(payload)).await?).map_err(|_| {
                ForgeError::uncertain("GitHub did not identify the created pull request")
            })
        }
        Operation::SetIssueState { number, state } => {
            let (category, reason) = state_target(state)?;
            let n = number.to_string();
            parts.extend(["issues", &n]);
            let old = issue(&request(c, Method::GET, &parts, &[], None).await?)?;
            if old["number"] != *number {
                return Err(ForgeError::remote(
                    404,
                    "GitHub issue number does not match",
                ));
            }
            if state_matches(&old, state)? {
                return Ok(old);
            }
            if category == "closed" && old["state"] == "closed" && reason.is_some() {
                return Err(ForgeError::bad(
                    "GitHub requires reopening an issue before changing its close reason",
                ));
            }
            let mut body = json!({"state":category});
            if let Some(reason) = reason {
                body["state_reason"] = json!(reason);
            }
            let result = issue(&request(c, Method::PATCH, &parts, &[], Some(body)).await?)
                .map_err(|_| {
                    ForgeError::uncertain("GitHub returned an invalid issue-state receipt")
                })?;
            if result["number"] != *number || !state_matches(&result, state)? {
                return Err(ForgeError::uncertain(
                    "GitHub did not confirm the requested issue state and reason",
                ));
            }
            Ok(result)
        }
    }
}
