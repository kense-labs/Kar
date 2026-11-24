use super::super::super::{issue::child, reactions::Content, rest::project};
use super::super::git_hosts::{actor, summary};
use super::*;
use futures_util::TryStreamExt;

fn array(v: Value) -> Result<Vec<Value>, ForgeError> {
    match v {
        Value::Array(rows) => Ok(rows),
        Value::Null => Ok(Vec::new()),
        _ => Err(invalid("Forgejo list")),
    }
}

pub(super) fn awards(rows: &Value, viewer: &str, gitlab: bool) -> Result<Vec<Value>, ForgeError> {
    let mut grouped = BTreeMap::<String, (usize, Vec<String>, bool)>::new();
    let rows = if rows.is_null() {
        &[][..]
    } else {
        rows.as_array()
            .ok_or_else(|| invalid("issue reactions"))?
            .as_slice()
    };
    for r in rows {
        let name = r[if gitlab { "name" } else { "content" }]
            .as_str()
            .unwrap_or("");
        let Some(content) = Content::from_host(&name.trim().to_ascii_lowercase(), gitlab) else {
            continue;
        };
        let key = serde_json::to_value(content)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        let who = r["user"][if gitlab { "username" } else { "login" }]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if who.is_none() && gitlab {
            continue;
        }
        let g = grouped.entry(key).or_default();
        g.0 += 1;
        if let Some(who) = who {
            if viewer.eq_ignore_ascii_case(who) {
                g.2 = true
            } else {
                g.1.push(who.to_string())
            }
        }
    }
    Ok(grouped.into_iter().map(|(content,(count,actors,mine))|json!({"content":content,"count":count,"actors":actors,"viewerHasReacted":mine})).collect())
}
fn parse_page(cursor: Option<&str>) -> Result<usize, ForgeError> {
    cursor
        .unwrap_or("1")
        .parse::<usize>()
        .ok()
        .filter(|p| *p > 0 && *p < 10_000_000)
        .ok_or_else(|| ForgeError::bad("invalid Forgejo comments cursor"))
}
async fn notes(
    c: &Context,
    repo: &Url,
    target: &Url,
    viewer: &str,
    page: usize,
) -> Result<Value, ForgeError> {
    let raw = array(get(c, child(target.clone(), &["comments"])).await?)?;
    let rows = raw;
    let start = (page - 1) * 50;
    let fetch = |row: &Value| {
        let row = row.clone();
        async move {
            let id = number(&row["id"])?.to_string();
            let url = child(repo.clone(), &["issues", "comments", &id, "reactions"]);
            let reactions = awards(&get(c, url).await?, viewer, false)?;
            Ok::<_, ForgeError>(json!({
                "id":id,"body":row["body"],"author":actor(&row["user"],"login"),
                "createdAt":row["created_at"],"url":row["html_url"],"reactions":reactions
            }))
        }
    };
    let items = stream::iter(rows.iter().skip(start).take(50).map(fetch))
        .buffered(4)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(json!({"comments":items,"nextCursor":(rows.len()>start+50).then(||(page+1).to_string())}))
}
async fn timeline(c: &Context, target: &Url) -> Result<Vec<Value>, ForgeError> {
    let mut out = Vec::new();
    for page in 1..=10 {
        let mut u = child(target.clone(), &["timeline"]);
        u.query_pairs_mut()
            .append_pair("limit", "50")
            .append_pair("page", &page.to_string());
        let (raw, headers) = send_with_headers(c, Method::GET, u, None, false).await?;
        let rows = array(raw)?;
        let empty = rows.is_empty();
        out.extend(rows);
        if empty
            || headers.get("link").is_some_and(|v| {
                v.to_str()
                    .is_ok_and(|v| !v.contains("rel=\"next\"") && !v.contains("rel=next"))
            })
        {
            break;
        }
    }
    Ok(out)
}
fn event(v: &Value) -> Option<Value> {
    let (kind, detail) = match v["type"].as_str()? {
        "close" => ("closed", Value::Null),
        "reopen" => ("reopened", Value::Null),
        "assignees" => (
            if v["removed_assignee"] == true {
                "unassigned"
            } else {
                "assigned"
            },
            v["assignee"]["login"].clone(),
        ),
        "change_title" => ("renamed", v["new_title"].clone()),
        "label" => (
            if v["body"] == "1" {
                "labeled"
            } else {
                "unlabeled"
            },
            v["label"]["name"].clone(),
        ),
        "issue_ref" | "commit_ref" | "comment_ref" | "pull_ref" => (
            "referenced",
            json!(v["ref_issue"]["number"].as_u64().map(|n| format!("#{n}"))),
        ),
        _ => return None,
    };
    Some(
        json!({"id":number(&v["id"]).ok()?.to_string(),"createdAt":v["created_at"],"actor":actor(&v["user"],"login"),"kind":kind,"detail":detail}),
    )
}
async fn pulls(c: &Context, events: &[Value]) -> Result<Vec<Value>, ForgeError> {
    let mut refs = BTreeSet::new();
    for v in events
        .iter()
        .filter(|v| v["type"] == "pull_ref" && !v["ref_issue"]["pull_request"].is_null())
    {
        let Some(n) = v["ref_issue"]["number"].as_u64() else {
            continue;
        };
        let named = v["ref_issue"]["repository"]["full_name"].as_str();
        let repo = named
            .filter(|r| !c.repository.ends_with(&format!("/{r}")))
            .unwrap_or(&c.repository);
        refs.insert((repo.to_owned(), n));
    }
    let fetch = |(repo, n): (String, u64)| async move {
        let other = Context {
            repository: repo.clone(),
            ..c.clone()
        };
        let url = child(project(&other)?, &["pulls", &n.to_string()]);
        let pull = match get(c, url).await {
            Ok(p) => p,
            Err(e) if e.status == 404 => return Ok(None),
            Err(e) => return Err(e),
        };
        if pull["number"] != n {
            return Err(invalid("Forgejo linked pull identity"));
        }
        let state = if pull["merged"] == true {
            "merged"
        } else if pull["state"] == "closed" {
            "closed"
        } else {
            "open"
        };
        Ok(Some(
            json!({"repository":repo,"number":n,"title":pull["title"],"url":pull["html_url"],
            "state":state,"isDraft":pull["draft"]==true,"closesIssue":false}),
        ))
    };
    let rows = stream::iter(refs.into_iter().map(fetch))
        .buffered(2)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(rows.into_iter().flatten().collect())
}
pub(super) async fn read(
    c: &Context,
    n: u64,
    cursor: Option<&str>,
    comments_only: bool,
) -> Result<Value, ForgeError> {
    let p = parse_page(cursor)?;
    let repo = project(c)?;
    let target = child(repo.clone(), &["issues", &n.to_string()]);
    let (issue, user) = tokio::try_join!(get(c, target.clone()), get(c, path(c, &["user"])?))?;
    super::super::super::issue::verify_issue(c, &issue, n)?;
    let viewer = user["login"]
        .as_str()
        .ok_or_else(|| invalid("Forgejo viewer"))?;
    let page = notes(c, &repo, &target, viewer, p).await?;
    if comments_only {
        return Ok(page);
    }
    let (repo, history, raw) = tokio::try_join!(
        get(c, repo),
        timeline(c, &target),
        get(c, child(target.clone(), &["reactions"]))
    )?;
    let active = repo["archived"] != true;
    let push = repo["permissions"]["push"] == true;
    let edit = active && (push || issue["user"]["login"] == viewer);
    let mut row = summary(&issue, "forgejo").ok_or_else(|| invalid("Forgejo issue summary"))?;
    row["viewerPermissions"] = json!({"setState":edit,"edit":edit,"comment":active&&(issue["is_locked"]!=true||push),"labels":active&&push,"assignees":active&&push});
    row["viewer"] = json!(viewer);
    row["states"] =
        json!([{"category":"open","name":"Open"},{"category":"closed","name":"Closed"}]);
    row["body"] = json!(issue["body"].as_str().unwrap_or(""));
    row["comments"] = page["comments"].clone();
    row["commentsCursor"] = page["nextCursor"].clone();
    row["events"] = json!(history.iter().filter_map(event).collect::<Vec<_>>());
    row["linkedPullRequests"] = json!(pulls(c, &history).await?);
    row["reactions"] = json!(awards(&raw, viewer, false)?);
    Ok(row)
}
