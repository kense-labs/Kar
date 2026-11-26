//! Issue creation and conversations for the repository-scoped REST drivers.
use super::*;
pub(in crate::workbench::forge) mod update;

pub(in crate::workbench::forge) fn validate(op: &Operation) -> Result<(), ForgeError> {
    let valid = match op {
        Operation::CreateIssue {
            title,
            body,
            labels,
            assignees,
        } => {
            !title.trim().is_empty()
                && title.trim() == title
                && title.encode_utf16().count() <= 1024
                && body.encode_utf16().count() <= 65_536
                && labels.len() <= 50
                && assignees.len() <= 25
                && labels
                    .iter()
                    .chain(assignees)
                    .all(|s| !s.is_empty() && s.trim() == s)
        }
        Operation::SetIssueReaction {
            number, subject_id, ..
        } => {
            *number > 0
                && subject_id
                    .as_ref()
                    .is_none_or(|id| !id.is_empty() && id.trim() == id && id.len() <= 4096)
        }
        Operation::UpdateIssue { number, fields } => *number > 0 && fields.valid(),
        Operation::CommentIssue { number, body } => {
            *number > 0 && !body.trim().is_empty() && body.encode_utf16().count() <= 65_536
        }
        Operation::IssueComments { number, cursor } => {
            *number > 0
                && cursor
                    .as_deref()
                    .is_none_or(|s| !s.trim().is_empty() && s.len() <= 4096)
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(ForgeError::bad(
            "invalid issue title, comment, labels, assignees or cursor",
        ))
    }
}

pub(in crate::workbench::forge) fn child(mut url: Url, tail: &[&str]) -> Url {
    url.path_segments_mut()
        .expect("validated hierarchical API base")
        .pop_if_empty()
        .extend(tail);
    url
}

fn page_number(cursor: Option<&str>, initial: u64) -> Result<u64, ForgeError> {
    match cursor {
        None => Ok(initial.max(1)),
        Some(s) => s
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .map(u64::from)
            .ok_or_else(|| ForgeError::bad("invalid issue comment page")),
    }
}

pub(super) async fn labels(
    c: &Context,
    repo: &Url,
    names: &[String],
) -> Result<Vec<u64>, ForgeError> {
    let mut found = std::collections::HashMap::new();
    for page in 1..=100 {
        if names.iter().all(|n| found.contains_key(n)) {
            break;
        }
        let mut u = child(repo.clone(), &["labels"]);
        u.query_pairs_mut()
            .append_pair("page", &page.to_string())
            .append_pair("limit", "50");
        let data = get(c, u).await?;
        let rows = data
            .as_array()
            .ok_or_else(|| invalid("Forgejo label list"))?;
        for row in rows {
            if let Some(s) = row["name"].as_str() {
                found.insert(s.to_owned(), number(&row["id"])?);
            }
        }
        if rows.len() < 50 {
            break;
        }
    }
    names
        .iter()
        .map(|s| {
            found
                .get(s)
                .copied()
                .ok_or_else(|| ForgeError::bad(format!("unknown Forgejo label {s}")))
        })
        .collect()
}

pub(super) async fn gitlab_users(c: &Context, names: &[String]) -> Result<Vec<u64>, ForgeError> {
    let mut ids = Vec::new();
    for name in names {
        if let Ok(id) = name.parse::<u64>()
            && id > 0
        {
            ids.push(id);
        } else {
            let mut url = path(c, &["users"])?;
            url.query_pairs_mut().append_pair("username", name);
            let data = get(c, url).await?;
            let row = data
                .as_array()
                .and_then(|rows| {
                    rows.iter().find(|u| {
                        u["username"]
                            .as_str()
                            .is_some_and(|s| s.eq_ignore_ascii_case(name))
                    })
                })
                .ok_or_else(|| ForgeError::bad("unknown GitLab assignee"))?;
            ids.push(number(&row["id"])?);
        }
    }
    Ok(ids)
}

pub(super) fn verify_issue(c: &Context, raw: &Value, n: u64) -> Result<(), ForgeError> {
    if raw.get("pull_request").is_some_and(|v| !v.is_null()) || raw["kind"] == "pull" {
        return Err(ForgeError::remote(
            404,
            "this number identifies a pull request",
        ));
    }
    if number(&raw[if c.kind == "gitlab" { "iid" } else { "number" }])? != n {
        return Err(invalid("issue number"));
    }
    Ok(())
}

async fn create(
    c: &Context,
    repo: Url,
    title: &str,
    body: &str,
    names: &[String],
    assignees: &[String],
) -> Result<Value, ForgeError> {
    let url = child(repo.clone(), &["issues"]);
    let input = match c.kind.as_str() {
        "gitlab" => {
            json!({"title":title,"description":body,"labels":names.join(","),"assignee_ids":gitlab_users(c,assignees).await?})
        }
        "forgejo" => {
            json!({"title":title,"body":body,"labels":labels(c,&repo,names).await?,"assignees":assignees})
        }
        "tanstack-source" if body.trim().is_empty() => json!({"title":title}),
        "tanstack-source" => json!({"title":title,"body":body}),
        _ => json!({"title":title,"body":body,"labels":names,"assignees":assignees}),
    };
    let row = send(c, Method::POST, url.clone(), Some(input), true).await?;
    let n = number(&row[if c.kind == "gitlab" { "iid" } else { "number" }])
        .map_err(|_| ForgeError::uncertain("created issue has no confirmed number"))?;
    let issue_url = if c.kind == "tanstack-source" {
        for (field, values) in [("labels", names), ("assignees", assignees)] {
            if !values.is_empty() {
                send(
                    c,
                    Method::PUT,
                    child(url.clone(), &[&n.to_string(), field]),
                    Some(json!({(field):values})),
                    true,
                )
                .await
                .map_err(|err| {
                    ForgeError::uncertain(format!(
                        "created issue #{n}; {field} update needs inspection ({})",
                        err.kind
                    ))
                })?;
            }
        }
        format!(
            "https://{}/@{}/issues/{n}",
            c.host,
            c.repository.trim_start_matches('@')
        )
    } else {
        row[if c.kind == "gitlab" {
            "web_url"
        } else {
            "html_url"
        }]
        .as_str()
        .ok_or_else(|| ForgeError::uncertain("created issue has no URL"))?
        .into()
    };
    Ok(json!({"number":n,"key":n.to_string(),"displayKey":format!("#{n}"),"url":issue_url}))
}

async fn comments(
    c: &Context,
    target: Url,
    issue: &Value,
    cursor: Option<&str>,
) -> Result<Value, ForgeError> {
    if c.kind == "tanstack-source" {
        let detail = if let Some(s) = cursor {
            let mut u = target;
            u.query_pairs_mut().append_pair("cursor", s);
            let data = get(c, u).await?;
            verify_issue(c, &data, number(&issue["number"])?)?;
            data
        } else {
            issue.clone()
        };
        let mut rows = Vec::new();
        for r in detail["timeline"]
            .as_array()
            .ok_or_else(|| invalid("issue timeline"))?
        {
            if r["event"].is_string()
                || r["moderation"].as_str().is_some_and(|s| s != "visible")
                || r["id"].is_null()
            {
                continue;
            }
            rows.push(json!({"id":number(&r["id"])?.to_string(),"author":r["author"],"body":r["comment"].as_str().unwrap_or(""),"createdAt":r["at"]}));
        }
        return Ok(json!({"comments":rows,"nextCursor":detail["next"]}));
    }
    const COUNT: u64 = 50;
    let gl = c.kind == "gitlab";
    let all = c.kind == "forgejo";
    let initial = if c.kind == "github" {
        issue["comments"]
            .as_u64()
            .ok_or_else(|| invalid("GitHub issue comment count"))?
            .div_ceil(COUNT)
    } else {
        1
    };
    let page = page_number(cursor, initial)?;
    let mut u = child(target, &[if gl { "notes" } else { "comments" }]);
    if !all {
        u.query_pairs_mut()
            .append_pair("per_page", &COUNT.to_string())
            .append_pair("page", &page.to_string());
        if gl {
            u.query_pairs_mut()
                .append_pair("sort", "desc")
                .append_pair("order_by", "created_at");
        }
    }
    let data = get(c, u).await?;
    let all_rows = data.as_array().ok_or_else(|| invalid("issue comments"))?;
    let (slice, next) = if all {
        let end = all_rows.len().saturating_sub(((page - 1) * COUNT) as usize);
        let start = end.saturating_sub(COUNT as usize);
        (
            &all_rows[start..end],
            (start > 0).then(|| (page + 1).to_string()),
        )
    } else if gl {
        (
            all_rows.as_slice(),
            (all_rows.len() as u64 == COUNT).then(|| (page + 1).to_string()),
        )
    } else {
        (
            all_rows.as_slice(),
            (page > 1).then(|| (page - 1).to_string()),
        )
    };
    let mut rows = Vec::new();
    for row in slice {
        if gl && row["system"] == true {
            continue;
        }
        let author = if gl {
            &row["author"]["username"]
        } else {
            &row["user"]["login"]
        };
        rows.push(json!({"id":number(&row["id"])?.to_string(),"author":author,"body":row["body"],"createdAt":row["created_at"],"url":row["html_url"]}));
    }
    if gl {
        rows.reverse();
    }
    Ok(json!({"comments":rows,"nextCursor":next}))
}

pub(in crate::workbench::forge) async fn execute(
    c: &Context,
    op: &Operation,
) -> Result<Value, ForgeError> {
    let repo = match c.kind.as_str() {
        "github" | "gitlab" | "forgejo" => super::rest::project(c)?,
        "tanstack-source" => super::hosted::resource(c, None, None)?,
        _ => {
            return Err(ForgeError::unsupported(format!(
                "{} does not register an issue tracker",
                c.kind
            )));
        }
    };
    match op {
        Operation::CreateIssue {
            title,
            body,
            labels,
            assignees,
        } => create(c, repo, title, body, labels, assignees).await,
        Operation::UpdateIssue { number, fields } => update::apply(c, repo, *number, fields).await,
        Operation::IssueComments { number: n, .. } | Operation::CommentIssue { number: n, .. } => {
            let target = child(repo, &["issues", &n.to_string()]);
            let issue = get(c, target.clone()).await?;
            verify_issue(c, &issue, *n)?;
            if let Operation::IssueComments { cursor, .. } = op {
                return comments(c, target, &issue, cursor.as_deref()).await;
            }
            let Operation::CommentIssue { body, .. } = op else {
                unreachable!()
            };
            let row = send(
                c,
                Method::POST,
                child(
                    target,
                    &[if c.kind == "gitlab" {
                        "notes"
                    } else {
                        "comments"
                    }],
                ),
                Some(json!({"body":body})),
                true,
            )
            .await?;
            Ok(json!({"number":n,"comment_id":row["id"]}))
        }
        _ => unreachable!(),
    }
}
