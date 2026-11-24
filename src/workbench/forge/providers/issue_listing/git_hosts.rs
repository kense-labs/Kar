use super::super::issue::child;
use super::super::reviews::github::gql;
use super::*;

pub(super) fn actor(v: &Value, login: &str) -> Option<Value> {
    let name = v.get(login)?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    Some(
        json!({"login":name,"name":v.get("name").or(v.get("full_name")),"avatarUrl":v.get("avatar_url").or(v.get("avatarUrl"))}),
    )
}
pub(super) fn labels(v: &Value) -> Vec<Value> {
    v.as_array().into_iter().flatten().filter_map(|l|Some(json!({"name":l.as_str().or_else(||l["name"].as_str())?,"color":l["color"],"description":l["description"]}))).collect()
}
pub(super) fn summary(r: &Value, kind: &str) -> Option<Value> {
    let github = kind == "github";
    let gl = kind == "gitlab";
    let n = r[if gl { "iid" } else { "number" }].as_u64()?.to_string();
    let login = if gl { "username" } else { "login" };
    let closed = r["state"].as_str()?.eq_ignore_ascii_case("closed");
    let mut state =
        json!({"name":if closed{"Closed"}else{"Open"},"category":if closed{"closed"}else{"open"}});
    if closed
        && github
        && let Some(reason) = r["stateReason"].as_str()
    {
        let reason = reason.to_lowercase().replace('_', "-");
        if let Some(name) = match reason.as_str() {
            "not-planned" => Some("Not planned"),
            "completed" => Some("Completed"),
            "duplicate" => Some("Duplicate"),
            _ => None,
        } {
            state["name"] = json!(name);
            state["closeReason"] = json!(reason);
        }
    }
    let assignees = if github {
        &r["assignees"]["nodes"]
    } else {
        &r["assignees"]
    };
    Some(
        json!({"key":n,"displayKey":format!("#{n}"),"title":r["title"].as_str()?,"url":r[if github{"url"}else if gl{"web_url"}else{"html_url"}].as_str()?,
        "author":actor(&r[if kind=="forgejo"{"user"}else{"author"}],login),"state":state,
        "createdAt":r[if github{"createdAt"}else{"created_at"}].as_str()?,"updatedAt":r[if github{"updatedAt"}else{"updated_at"}].as_str()?,
        "closedAt":r[if github{"closedAt"}else{"closed_at"}],"labels":labels(if github{&r["labels"]["nodes"]}else{&r["labels"]}),
        "assignees":assignees.as_array().into_iter().flatten().filter_map(|u|actor(u,login)).collect::<Vec<_>>(),
        "commentCount":(if github{r["comments"]["totalCount"].as_u64()}else{r[if gl{"user_notes_count"}else{"comments"}].as_u64()}).unwrap_or(0)}),
    )
}
const SEARCH: &str = "query KarIssueSearch($q:String!,$rows:Int!,$cursor:String){search(query:$q,type:ISSUE,first:$rows,after:$cursor){pageInfo{hasNextPage endCursor} nodes{... on Issue{repository{nameWithOwner} number title url state stateReason createdAt updatedAt closedAt author{login avatarUrl ... on User{name}} assignees(first:20){nodes{login name avatarUrl}} labels(first:20){nodes{name color}} comments{totalCount}}}}}";
pub(super) async fn github(
    c: &Context,
    r: &Request,
    cursor: Option<&str>,
    viewer: Option<&str>,
) -> Result<Page, ForgeError> {
    if c.repository
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || "/_-.".contains(ch)))
    {
        return Err(ForgeError::bad("invalid GitHub repository"));
    }
    let quoted = |s: &str| serde_json::to_string(s).expect("JSON string");
    let mut terms = vec!["is:issue".into(), format!("repo:{}", c.repository)];
    if r.state != "all" {
        terms.push(format!("is:{}", r.state))
    }
    if let Some(v) = viewer {
        let qualifier = match r.involvement {
            Involvement::Assigned => "assignee",
            Involvement::Authored => "author",
            Involvement::Mentioned => "mentions",
            _ => unreachable!(),
        };
        terms.push(format!("{qualifier}:{}", quoted(v)))
    }
    terms.extend(r.labels.iter().map(|l| format!("label:{}", quoted(l))));
    if let Some(a) = &r.assignee {
        terms.push(format!("assignee:{}", quoted(a)))
    }
    terms.push(format!(
        "sort:{}-desc",
        if r.sort == Sort::Created {
            "created"
        } else {
            "updated"
        }
    ));
    if let Some(q) = &r.query {
        terms.extend(q.split_whitespace().map(quoted))
    }
    let data = gql(
        c,
        SEARCH,
        json!({"q":terms.join(" "),"rows":r.limit().min(100),"cursor":cursor}),
        false,
    )
    .await?;
    let page = &data["search"];
    let items = page["nodes"]
        .as_array()
        .ok_or_else(|| invalid("GitHub issue search"))?
        .iter()
        .filter(|i| {
            i["repository"]["nameWithOwner"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(&c.repository))
        })
        .filter_map(|i| summary(i, "github"))
        .collect();
    Ok(Page {
        items,
        next: after(&page["pageInfo"], cursor)?,
    })
}
pub(super) fn after(info: &Value, current: Option<&str>) -> Result<Option<String>, ForgeError> {
    if info["hasNextPage"]
        .as_bool()
        .ok_or_else(|| invalid("page info"))?
    {
        let next = info["endCursor"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096 && Some(*s) != current)
            .ok_or_else(|| invalid("native continuation cursor"))?;
        Ok(Some(next.into()))
    } else {
        Ok(None)
    }
}
pub(super) async fn rest(
    c: &Context,
    r: &Request,
    cursor: Option<&str>,
    viewer: Option<&str>,
) -> Result<Page, ForgeError> {
    let gl = c.kind == "gitlab";
    let page = cursor
        .unwrap_or("1")
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0 && *n < u32::MAX)
        .ok_or_else(|| ForgeError::bad("invalid issue page"))?;
    let size = if gl { r.limit().min(100) } else { 50 };
    let mut url = child(super::super::rest::project(c)?, &["issues"]);
    {
        let mut params = url.query_pairs_mut();
        params
            .append_pair("page", &page.to_string())
            .append_pair(if gl { "per_page" } else { "limit" }, &size.to_string())
            .append_pair(
                "state",
                if gl && r.state == "open" {
                    "opened"
                } else {
                    &r.state
                },
            );
        if gl {
            params
                .append_pair(
                    "order_by",
                    if r.sort == Sort::Created {
                        "created_at"
                    } else {
                        "updated_at"
                    },
                )
                .append_pair("sort", "desc")
                .append_pair("with_labels_details", "true");
        } else {
            params.append_pair("type", "issues").append_pair(
                "sort",
                if r.sort == Sort::Created {
                    "latest"
                } else {
                    "recentupdate"
                },
            );
        }
        if let Some(text) = &r.query {
            params.append_pair(if gl { "search" } else { "q" }, text);
        }
        if !r.labels.is_empty() {
            params.append_pair("labels", &r.labels.join(","));
        }
        if let Some(v) = viewer {
            let key = match r.involvement {
                Involvement::Authored => Some(if gl { "author_username" } else { "created_by" }),
                Involvement::Assigned if r.assignee.is_none() => Some(if gl {
                    "assignee_username"
                } else {
                    "assigned_by"
                }),
                Involvement::Mentioned if !gl => Some("mentioned_by"),
                _ => None,
            };
            if let Some(k) = key {
                params.append_pair(k, v);
            }
        }
        if let Some(a) = &r.assignee {
            params.append_pair(
                if gl {
                    "assignee_username"
                } else {
                    "assigned_by"
                },
                a,
            );
        }
    }
    let (data, headers) = send_with_headers(c, Method::GET, url, None, false).await?;
    let rows = data
        .as_array()
        .ok_or_else(|| invalid("tracker issue page"))?;
    let items = rows
        .iter()
        .filter(|row| row["pull_request"].is_null())
        .filter_map(|v| summary(v, &c.kind))
        .filter(|item| {
            (gl || r.labels.iter().all(|wanted| {
                item["labels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|l| l["name"] == *wanted)
            })) && (r.assignee.is_none()
                || r.involvement != Involvement::Assigned
                || item["assignees"].as_array().unwrap().iter().any(|u| {
                    u["login"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(viewer.unwrap_or("")))
                }))
        })
        .collect();
    let more = if gl {
        rows.len() >= size
    } else {
        !rows.is_empty()
            && headers.get("link").is_none_or(|h| {
                h.to_str()
                    .is_ok_and(|v| v.contains("rel=\"next\"") || v.contains("rel=next"))
            })
    };
    Ok(Page {
        items,
        next: more.then(|| (page + 1).to_string()),
    })
}
