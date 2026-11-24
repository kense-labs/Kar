use super::*;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Query {
    state: Option<String>,
    involvement: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}
#[derive(Serialize, Deserialize, Default, PartialEq)]
struct Cursor {
    sig: String,
    bucket: usize,
    page: usize,
    token: Option<String>,
    skip: usize,
}
struct Page {
    rows: Vec<Value>,
    token: Option<String>,
    more: bool,
}
const PAGE: usize = 50;
fn states(kind: &str, state: &str) -> Vec<&'static str> {
    match (kind, state) {
        ("gitlab", "open") => vec!["opened"],
        ("azure-devops", "open") => vec!["active"],
        ("azure-devops", "closed") => vec!["abandoned"],
        ("azure-devops", "merged") => vec!["completed"],
        ("bitbucket", "open") => vec!["OPEN"],
        ("bitbucket", "closed") => vec!["DECLINED", "SUPERSEDED"],
        ("bitbucket", "merged") => vec!["MERGED"],
        ("bitbucket", "all") => vec!["OPEN", "MERGED", "DECLINED", "SUPERSEDED"],
        ("tanstack-source", "all") => vec!["open", "closed"],
        ("github" | "forgejo" | "tanstack-source", "merged") => vec!["closed"],
        (_, "merged") => vec!["merged"],
        (_, "open") => vec!["open"],
        (_, "closed") => vec!["closed"],
        _ => vec!["all"],
    }
}
async fn page(c: &Context, cur: &Cursor, state: &str) -> Result<Page, ForgeError> {
    let collection = match c.kind.as_str() {
        "gitlab" => "merge_requests",
        "bitbucket" | "azure-devops" => "pullrequests",
        _ => "pulls",
    };
    let mut url = child(repo(c)?, &[collection]);
    let page = cur
        .page
        .checked_add(1)
        .filter(|p| *p < 1_000_000)
        .ok_or_else(|| ForgeError::bad("invalid pull page"))?;
    let size = PAGE.to_string();
    let page_str = page.to_string();
    {
        let mut q = url.query_pairs_mut();
        match c.kind.as_str() {
            "gitcafe" | "tanstack-source" | "cursor-origin" => {
                if !(c.kind == "gitcafe" && state == "all") {
                    q.append_pair("state", state);
                }
                let (limit, cursor) = match c.kind.as_str() {
                    "gitcafe" => ("limit", "after"),
                    "cursor-origin" => ("pageSize", "pageToken"),
                    _ => ("limit", "cursor"),
                };
                q.append_pair(limit, &size);
                if c.kind == "cursor-origin" {
                    q.append_pair("sortBy", "updated");
                }
                if let Some(s) = &cur.token {
                    q.append_pair(cursor, s);
                }
            }
            "azure-devops" => {
                q.append_pair("searchCriteria.status", state)
                    .append_pair("$top", &size)
                    .append_pair("$skip", &(cur.page * PAGE).to_string());
            }
            _ => {
                let limit = match c.kind.as_str() {
                    "forgejo" => "limit",
                    "bitbucket" => "pagelen",
                    _ => "per_page",
                };
                q.append_pair("state", state)
                    .append_pair(limit, &size)
                    .append_pair("page", &page_str);
                if c.kind == "github" {
                    q.append_pair("sort", "updated")
                        .append_pair("direction", "desc");
                }
            }
        }
    }
    let raw = providers::get(c, url).await?;
    let entries = match c.kind.as_str() {
        "azure-devops" => &raw["value"],
        "bitbucket" => &raw["values"],
        "gitcafe" | "tanstack-source" => &raw["items"],
        "cursor-origin" => &raw["pullRequests"],
        _ => &raw,
    };
    let rows = if c.kind == "cursor-origin" && entries.is_null() {
        vec![]
    } else {
        entries
            .as_array()
            .ok_or_else(|| ForgeError::bad("invalid pull list page"))?
            .clone()
    };
    let token = raw[if c.kind == "cursor-origin" {
        "nextPageToken"
    } else {
        "next"
    }]
    .as_str()
    .filter(|s| !s.is_empty())
    .map(str::to_owned);
    let more = if matches!(
        c.kind.as_str(),
        "gitcafe" | "tanstack-source" | "cursor-origin" | "bitbucket"
    ) {
        token.is_some()
    } else {
        rows.len() == PAGE
    };
    Ok(Page {
        rows,
        more,
        token: if c.kind == "bitbucket" { None } else { token },
    })
}
fn actor(v: &Value) -> Option<&str> {
    v.as_str().or_else(|| {
        [
            "login",
            "username",
            "handle",
            "nickname",
            "slug",
            "displayName",
            "uniqueName",
        ]
        .iter()
        .find_map(|k| v[*k].as_str())
    })
}
fn matches_actor(v: &Value, viewer: &str) -> bool {
    actor(v).is_some_and(|s| s.eq_ignore_ascii_case(viewer))
        || v.as_array()
            .is_some_and(|xs| xs.iter().any(|v| matches_actor(v, viewer)))
}
fn item(c: &Context, raw: &Value) -> Result<Value, ForgeError> {
    let mut v = match c.kind.as_str() {
        "github" => github::pull(raw),
        "gitcafe" | "cursor-origin" | "tanstack-source" => providers::hosted::item(c, raw, true),
        "azure-devops" => providers::azure::listed_pull(c, raw),
        _ => providers::rest::item(c, raw, true),
    }?;
    v["author"] = json!(
        ["user", "author", "createdBy", "creator"]
            .iter()
            .find_map(|k| actor(&raw[*k]))
    );
    v["draft"] = raw
        .get("draft")
        .or(raw.get("isDraft"))
        .or(raw.get("work_in_progress"))
        .cloned()
        .unwrap_or_default();
    Ok(v)
}
pub(super) async fn read(c: &Context, q: Query) -> Result<Value, ForgeError> {
    let state = q.state.as_deref().unwrap_or("open");
    let involvement = q.involvement.as_deref().unwrap_or("all");
    let query = q.query.as_deref().unwrap_or("").trim().to_lowercase();
    let limit = q.limit.unwrap_or(30);
    if !matches!(state, "open" | "closed" | "all" | "merged")
        || !matches!(involvement, "all" | "authored" | "reviewing")
        || !(1..=100).contains(&limit)
        || query.len() > 512
    {
        return Err(ForgeError::bad("invalid pull list filters"));
    }
    let viewer = if involvement == "all" {
        String::new()
    } else {
        c.execute(&Operation::Identity).await?["account"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ForgeError::bad("involvement requires a signed-in viewer"))?
            .into()
    };
    let sig = signature(c, &json!([state, involvement, query, viewer]));
    let mut cursor = match q.cursor {
        Some(s) => decode::<Cursor>(&s)?,
        None => Cursor {
            sig: sig.clone(),
            ..Default::default()
        },
    };
    let buckets = states(&c.kind, state);
    if cursor.sig != sig || cursor.bucket >= buckets.len() || cursor.skip > 1000 {
        return Err(ForgeError::bad("pull cursor does not match these filters"));
    }
    let mut out = vec![];
    for _ in 0..100 {
        let native = page(c, &cursor, buckets[cursor.bucket]).await?;
        for (i, raw) in native.rows.iter().enumerate().skip(cursor.skip) {
            cursor.skip = i + 1;
            let v = item(c, raw)?;
            if state != "all" && v["state"] != state {
                continue;
            }
            let author = v["author"].as_str().unwrap_or("");
            let text = format!(
                "{} #{} {} {}",
                v["title"].as_str().unwrap_or(""),
                v["number"],
                author,
                v["body"].as_str().unwrap_or("")
            )
            .to_lowercase();
            if !text.contains(&query) {
                continue;
            }
            if involvement == "authored" && !author.eq_ignore_ascii_case(&viewer) {
                continue;
            }
            if involvement == "reviewing"
                && ![
                    "requested_reviewers",
                    "reviewers",
                    "reviewRequests",
                    "reviewRequested",
                    "reviewRequestLogins",
                ]
                .iter()
                .any(|k| matches_actor(&raw[*k], &viewer))
            {
                continue;
            }
            out.push(v);
            if out.len() == limit {
                break;
            }
        }
        if cursor.skip >= native.rows.len() {
            cursor.skip = 0;
            if native.more {
                if native.token.is_some() && native.token == cursor.token {
                    return Err(ForgeError::bad("repeated pull cursor"));
                }
                cursor.token = native.token;
                cursor.page += 1;
            } else {
                cursor.bucket += 1;
                cursor.page = 0;
                cursor.token = None;
            }
        }
        if cursor.bucket == buckets.len() {
            return Ok(json!({"entries":out,"truncated":false,"next_cursor":null}));
        }
        if out.len() == limit {
            break;
        }
    }
    Ok(json!({"entries":out,"truncated":true,"next_cursor":encode(&cursor)?}))
}
