use super::super::super::{
    issue::child,
    rest::{gitlab_graphql, project},
};
use super::super::git_hosts::{actor, summary};
use super::*;

const AWARDS: &str = "query KarIssueAwards($fullPath:ID!,$iid:String!,$cursor:String){currentUser{username} project(fullPath:$fullPath){issue(iid:$iid){notes(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{id awardEmoji{nodes{name user{username}}}}}}}}";

fn page_number(s: &str) -> Option<u32> {
    let n = s.parse::<u32>().ok()?;
    (n > 0 && s.len() <= 7 && n.to_string() == s).then_some(n)
}
fn cursor(s: Option<&str>) -> Result<(u32, Option<&str>), ForgeError> {
    let (page, awards) = s
        .unwrap_or("1")
        .split_once(':')
        .map_or((s.unwrap_or("1"), None), |(p, a)| (p, Some(a)));
    if awards.is_some_and(|a| a.is_empty() || a.len() > 512 || a.chars().any(char::is_control)) {
        return Err(ForgeError::bad("invalid GitLab note awards cursor"));
    }
    Ok((
        page_number(page).ok_or_else(|| ForgeError::bad("invalid GitLab notes page"))?,
        awards,
    ))
}
fn list(v: &Value) -> Result<&[Value], ForgeError> {
    v.as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid("GitLab list"))
}
fn page_url(target: &Url, resource: &str, page: u32) -> Url {
    let mut u = child(target.clone(), &[resource]);
    u.query_pairs_mut()
        .append_pair("per_page", "100")
        .append_pair("page", &page.to_string());
    u
}
async fn notes(
    c: &Context,
    target: &Url,
    page: u32,
    only_activity: bool,
) -> Result<(Vec<Value>, Option<u32>), ForgeError> {
    let mut u = page_url(target, "notes", page);
    u.query_pairs_mut()
        .extend_pairs([("order_by", "created_at"), ("sort", "asc")]);
    if only_activity {
        u.query_pairs_mut()
            .append_pair("activity_filter", "only_activity");
    }
    let (v, headers) = send_with_headers(c, Method::GET, u, None, false).await?;
    let rows = list(&v)?.to_vec();
    let next = match headers.get("x-next-page") {
        None => (rows.len() >= 100).then_some(page + 1),
        Some(s) => {
            let s = s
                .to_str()
                .map_err(|_| invalid("GitLab pagination header"))?
                .trim();
            if s.is_empty() {
                None
            } else {
                Some(
                    page_number(s)
                        .filter(|p| *p > page)
                        .ok_or_else(|| invalid("GitLab next notes page"))?,
                )
            }
        }
    };
    Ok((rows, next))
}

#[derive(Default)]
struct Awards {
    notes: BTreeMap<String, Vec<Value>>,
    next: Option<String>,
}
async fn awards(c: &Context, n: u64, after: Option<&str>) -> Result<Awards, ForgeError> {
    let b = json!({"query":AWARDS,"variables":{"fullPath":c.repository,"iid":n.to_string(),"cursor":after}});
    let v = send(c, Method::POST, gitlab_graphql(c), Some(b), false).await?;
    if let Some(errors) = v["errors"].as_array().filter(|e| !e.is_empty()) {
        // Older servers reject the Note.awardEmoji field during GraphQL validation.
        if errors.iter().all(|e| {
            let msg = e["message"].as_str().unwrap_or("").to_ascii_lowercase();
            msg.contains("awardemoji")
                && (msg.contains("doesn't exist")
                    || msg.contains("does not exist")
                    || msg.contains("cannot query field")
                    || msg.contains("undefined"))
        }) {
            return Ok(Awards::default());
        }
        return Err(ForgeError::remote(
            502,
            "GitLab rejected the note awards query",
        ));
    }
    let notes = &v["data"]["project"]["issue"]["notes"];
    let viewer = v["data"]["currentUser"]["username"]
        .as_str()
        .unwrap_or_default();
    let mut result = Awards::default();
    for note in notes["nodes"].as_array().into_iter().flatten() {
        if let Some(id) = note["id"]
            .as_str()
            .and_then(|g| g.rsplit('/').next())
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            result.notes.insert(
                id.into(),
                forgejo::awards(&note["awardEmoji"]["nodes"], viewer, true)?,
            );
        }
    }
    if notes["pageInfo"]["hasNextPage"] == true {
        let end = notes["pageInfo"]["endCursor"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 512)
            .ok_or_else(|| invalid("GitLab note awards cursor"))?;
        result.next = Some(end.into());
    }
    Ok(result)
}
async fn conversation(
    c: &Context,
    target: &Url,
    n: u64,
    page: u32,
    after: Option<&str>,
) -> Result<Value, ForgeError> {
    let award_page = async {
        if page > 1 && after.is_none() {
            Ok(Awards::default())
        } else {
            awards(c, n, after).await
        }
    };
    let ((rows, next), awards) = tokio::try_join!(notes(c, target, page, false), award_page)?;
    let comments:Vec<_>=rows.iter().filter(|r|r["system"]!=true).filter_map(|r|{
        let id=r["id"].as_u64()?.to_string();
        let date=r["created_at"].as_str()?;
        Some(json!({"id":id,"author":actor(&r["author"],"username"),"body":r["body"].as_str().unwrap_or_default(),"createdAt":date,
            "url":null,"reactions":awards.notes.get(&id).map(Vec::as_slice).unwrap_or_default()}))
    }).collect();
    let next = next.map(|p| {
        awards
            .next
            .map_or_else(|| p.to_string(), |a| format!("{p}:{a}"))
    });
    Ok(json!({"comments":comments,"nextCursor":next}))
}
pub(super) async fn comments(
    c: &Context,
    n: u64,
    input: Option<&str>,
) -> Result<Value, ForgeError> {
    let (page, after) = cursor(input)?;
    let target = child(project(c)?, &["issues", &n.to_string()]);
    conversation(c, &target, n, page, after).await
}

fn mention(detail: &str, issue_url: &str, scope: &str) -> Option<Value> {
    let (refname, kind, sep, resource) = if let Some(s) = detail.strip_prefix("merge request ") {
        (s, "pull-request", '!', "merge_requests")
    } else {
        (detail.strip_prefix("issue ")?, "issue", '#', "issues")
    };
    let (repo, id) = refname.split_once(sep)?;
    let n = id
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == id)?;
    if !repo
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
        || repo.split('/').any(|s| matches!(s, ".." | "."))
    {
        return None;
    }
    let project = issue_url.split_once("/-/")?.0;
    if !project
        .to_ascii_lowercase()
        .ends_with(&format!("/{}", scope.to_ascii_lowercase()))
    {
        return None;
    }
    let repo = if repo.is_empty() {
        scope.into()
    } else if repo.contains('/') {
        repo.to_string()
    } else if let Some((ns, _)) = scope.rsplit_once('/') {
        format!("{ns}/{repo}")
    } else {
        repo.to_string()
    };
    let base = project.get(..project.len() - scope.len())?;
    Some(
        json!({"kind":kind,"repository":repo,"number":n,"url":format!("{base}{repo}/-/{resource}/{id}")}),
    )
}
fn system_event(r: &Value, issue_url: &str, scope: &str) -> Option<Value> {
    if r["system"] != true {
        return None;
    }
    let body = r["body"].as_str()?.trim();
    let (kind, detail) = match body {
        "closed" => ("closed", None),
        "reopened" => ("reopened", None),
        _ if body.starts_with("assigned to ") => (
            "assigned",
            body.strip_prefix("assigned to ")
                .map(|s| s.trim_start_matches('@')),
        ),
        _ if body.starts_with("unassigned ") => (
            "unassigned",
            body.strip_prefix("unassigned ")
                .map(|s| s.trim_start_matches('@')),
        ),
        _ if body.starts_with("mentioned in ") => {
            ("referenced", body.strip_prefix("mentioned in "))
        }
        _ if body.starts_with("changed title") => (
            "renamed",
            body.split_once("{+")
                .and_then(|(_, s)| s.rsplit_once("+}").map(|(name, _)| name)),
        ),
        _ => return None,
    };
    let detail = detail.map(str::trim).filter(|s| !s.is_empty());
    let mut v = json!({"id":format!("note-{}",r["id"].as_u64()?),"kind":kind,"detail":detail,"actor":actor(&r["author"],"username"),"createdAt":r["created_at"].as_str()?});
    if kind == "referenced"
        && let Some(target) = detail.and_then(|d| mention(d, issue_url, scope))
    {
        v["reference"] = target;
    }
    Some(v)
}
async fn events(c: &Context, target: &Url, issue_url: &str) -> Result<Vec<Value>, ForgeError> {
    let mut result = Vec::new();
    let mut page = 1;
    for _ in 0..20 {
        let (rows, next) = notes(c, target, page, true).await?;
        result.extend(
            rows.iter()
                .filter_map(|r| system_event(r, issue_url, &c.repository)),
        );
        let Some(n) = next else {
            break;
        };
        page = n;
    }
    let labels = get(c, page_url(target, "resource_label_events", 1)).await?;
    result.extend(list(&labels)?.iter().filter_map(|r|{
        let kind=match r["action"].as_str()?.trim().to_ascii_lowercase().as_str(){"add"=>"labeled","remove"=>"unlabeled",_=>return None};
        Some(json!({"id":format!("label-{}",r["id"].as_u64()?),"kind":kind,"detail":r["label"]["name"],"actor":actor(&r["user"],"username"),"createdAt":r["created_at"].as_str()?}))
    }));
    result.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()));
    Ok(result)
}
async fn links(c: &Context, target: &Url) -> Result<Vec<Value>, ForgeError> {
    let (closing, related) = tokio::try_join!(
        get(c, page_url(target, "closed_by", 1)),
        get(c, page_url(target, "related_merge_requests", 1))
    )?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for (raw, closes) in [(&closing, true), (&related, false)] {
        for r in list(raw)? {
            let Some((repo, _)) = r["references"]["full"]
                .as_str()
                .and_then(|s| s.split_once('!'))
            else {
                continue;
            };
            let (Some(n), Some(title), Some(url)) = (
                r["iid"].as_u64().filter(|n| *n > 0),
                r["title"].as_str(),
                r["web_url"].as_str(),
            ) else {
                continue;
            };
            if repo.trim().is_empty() || !seen.insert((repo.to_owned(), n)) {
                continue;
            }
            let state = match r["state"].as_str() {
                Some("merged") => "merged",
                Some("closed") => "closed",
                _ => "open",
            };
            result.push(json!({"repository":repo,"number":n,"url":url,"title":title,"state":state,
                "closesIssue":closes,"isDraft":r["draft"].as_bool().or(r["work_in_progress"].as_bool()).unwrap_or_default()}));
        }
    }
    Ok(result)
}
pub(super) async fn read(c: &Context, n: u64) -> Result<Value, ForgeError> {
    let repo = project(c)?;
    let mut filter = child(repo.clone(), &["issues"]);
    filter.query_pairs_mut().extend_pairs([
        ("iids[]", &n.to_string()[..]),
        ("with_labels_details", "true"),
        ("per_page", "1"),
    ]);
    let target = child(repo, &["issues", &n.to_string()]);
    let (raw, user) = tokio::try_join!(get(c, filter), get(c, path(c, &["user"])?))?;
    let issue = list(&raw)?
        .first()
        .ok_or_else(|| ForgeError::remote(404, "unknown GitLab issue"))?;
    super::super::super::issue::verify_issue(c, issue, n)?;
    let viewer = user["username"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("GitLab viewer"))?;
    let mut detail = summary(issue, "gitlab").ok_or_else(|| invalid("GitLab issue detail"))?;
    let (notes, events, pulls, raw_awards) = tokio::try_join!(
        conversation(c, &target, n, 1, None),
        events(c, &target, issue["web_url"].as_str().unwrap()),
        links(c, &target),
        get(c, page_url(&target, "award_emoji", 1))
    )?;
    detail["body"] = json!(issue["description"].as_str().unwrap_or_default());
    detail["states"] =
        json!([{"name":"Open","category":"open"},{"name":"Closed","category":"closed"}]);
    detail["viewer"] = json!(viewer);
    detail["viewerPermissions"] =
        json!({"setState":true,"comment":true,"edit":true,"labels":true,"assignees":true});
    detail["comments"] = notes["comments"].clone();
    detail["commentsCursor"] = notes["nextCursor"].clone();
    detail["events"] = json!(events);
    detail["linkedPullRequests"] = json!(pulls);
    detail["reactions"] = json!(forgejo::awards(&raw_awards, viewer, true)?);
    Ok(detail)
}
