use super::git_hosts::{actor, after, labels};
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};

pub(super) async fn linear(
    c: &Context,
    team: &str,
    r: &Request,
    cursor: Option<&str>,
    viewer: Option<&str>,
) -> Result<Page, ForgeError> {
    if c.token.is_none() {
        return Err(ForgeError::remote(
            401,
            "Linear credentials are unavailable",
        ));
    }
    let mut filter = json!({"team":{"key":{"eq":team}}});
    if r.state != "all" {
        filter["state"] =
            json!({"type":{(if r.state=="closed"{"in"}else{"nin"}):["completed","canceled"]}});
    }
    if let Some(v) = viewer {
        let actor = json!({"displayName":{"eq":v}});
        match r.involvement {
            Involvement::Assigned => filter["assignee"] = actor,
            Involvement::Authored => filter["creator"] = actor,
            Involvement::Mentioned => filter["subscribers"] = json!({"some":actor}),
            _ => {}
        }
    }
    let mut clauses: Vec<Value> = r
        .labels
        .iter()
        .map(|l| json!({"labels":{"some":{"name":{"eq":l}}}}))
        .collect();
    if let Some(a) = &r.assignee {
        clauses.push(json!({"assignee":{"displayName":{"eq":a}}}));
    }
    if !clauses.is_empty() {
        filter["and"] = json!(clauses)
    }
    if let Some(q) = &r.query {
        let mut terms = vec![
            json!({"title":{"containsIgnoreCase":q}}),
            json!({"description":{"containsIgnoreCase":q}}),
        ];
        let digits = if let Some((prefix, n)) = q.split_once('-') {
            prefix.eq_ignore_ascii_case(team).then_some(n)
        } else {
            Some(q.trim_start_matches('#'))
        };
        if let Some(n) = digits
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|n| *n > 0)
        {
            terms.push(json!({"number":{"eq":n}}))
        }
        filter["or"] = json!(terms);
    }
    let q = "query KarIssueList($n:Int!,$after:String,$filter:IssueFilter!,$order:PaginationOrderBy!){organization{urlKey} issues(first:$n,after:$after,filter:$filter,orderBy:$order){pageInfo{hasNextPage endCursor} nodes{number identifier team{key} title url createdAt updatedAt completedAt canceledAt creator{displayName name avatarUrl} botActor{name userDisplayName avatarUrl} assignee{displayName name avatarUrl} labels{nodes{name color}} state{name type}}}}";
    let data=super::super::linear::gql(c,q,json!({"n":r.limit(),"after":cursor,"filter":filter,"order":if r.sort==Sort::Created{"createdAt"}else{"updatedAt"}}),false).await?;
    if let Some((org, _)) = c.repository.split_once('/')
        && data["organization"]["urlKey"] != org
    {
        return Err(ForgeError::remote(
            403,
            "Linear credential belongs to another workspace",
        ));
    }
    let page = &data["issues"];
    let mut items = Vec::new();
    for row in page["nodes"]
        .as_array()
        .ok_or_else(|| invalid("Linear issue page"))?
    {
        if row["team"]["key"] != team {
            return Err(invalid("Linear team returned by issue search"));
        }
        let key = row["identifier"]
            .as_str()
            .ok_or_else(|| invalid("Linear issue identifier"))?;
        let category = if matches!(
            row["state"]["type"].as_str(),
            Some("completed" | "canceled")
        ) {
            "closed"
        } else {
            "open"
        };
        let author = actor(&row["creator"], "displayName").or_else(|| {
            actor(&row["botActor"], "name").map(|mut v| {
                v["isBot"] = json!(true);
                v
            })
        });
        items.push(json!({"key":key,"displayKey":key,"number":row["number"],"url":row["url"],"title":row["title"],"createdAt":row["createdAt"],"updatedAt":row["updatedAt"],
            "closedAt":row["completedAt"].as_str().or_else(||row["canceledAt"].as_str()),"author":author,"assignees":actor(&row["assignee"],"displayName").into_iter().collect::<Vec<_>>(),
            "state":{"category":category,"name":row["state"]["name"]},"labels":labels(&row["labels"]["nodes"]),"commentCount":0}));
    }
    Ok(Page {
        items,
        next: after(&page["pageInfo"], cursor)?,
    })
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Position {
    after: Option<(i64, u64)>,
    pages: BTreeMap<String, Option<String>>,
}
fn key(row: &Value) -> Option<(i64, u64)> {
    Some((row["createdAt"].as_i64()?, row["number"].as_u64()?))
}
fn source_actor(c: &Context, name: Option<&str>) -> Option<Value> {
    let name = name?.trim();
    if name.is_empty() {
        return None;
    }
    let mut url = Url::parse(&format!("https://{}", c.host)).ok()?;
    url.path_segments_mut().ok()?.extend(["avatars", name]);
    Some(json!({"login":name,"name":null,"avatarUrl":url.as_str(),"isBot":name.ends_with("[bot]")}))
}
fn source_row(c: &Context, row: &Value) -> Result<Value, ForgeError> {
    let (at, n) = key(row).ok_or_else(|| invalid("Source issue timestamp or number"))?;
    let created = jiff::Timestamp::from_millisecond(at)
        .map_err(|_| invalid("Source issue timestamp"))?
        .to_string();
    let state = if row["state"] == "closed" {
        json!({"category":"closed","name":"Closed"})
    } else {
        json!({"category":"open","name":"Open"})
    };
    let mut url =
        Url::parse(&format!("https://{}/", c.host)).map_err(|_| invalid("Source host"))?;
    let repo = format!("@{}", c.repository.trim_start_matches('@'));
    url.path_segments_mut()
        .unwrap()
        .extend(repo.split('/'))
        .extend(["issues", &n.to_string()]);
    let assignees = row["assignees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| source_actor(c, v.as_str().or_else(|| v["username"].as_str())))
        .collect::<Vec<_>>();
    Ok(
        json!({"key":n.to_string(),"displayKey":format!("#{n}"),"title":row["title"],"state":state,"author":source_actor(c,row["author"].as_str()),"createdAt":created,
        "updatedAt":created,"closedAt":null,"assignees":assignees,"labels":labels(&row["labels"]),"commentCount":row["comments"].as_u64().unwrap_or(0),"url":url.as_str()}),
    )
}
pub(super) async fn source(
    c: &Context,
    r: &Request,
    cursor: Option<&str>,
    viewer: Option<&str>,
) -> Result<Page, ForgeError> {
    let mut pos = if let Some(token) = cursor {
        URL_SAFE_NO_PAD
            .decode(token)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Position>(&bytes).ok())
            .ok_or_else(|| ForgeError::bad("invalid Source issue cursor"))?
    } else {
        Position::default()
    };
    let states = if r.state == "all" {
        vec!["open", "closed"]
    } else {
        vec![r.state.as_str()]
    };
    if cursor.is_some()
        && (pos.pages.len() != states.len() || states.iter().any(|s| !pos.pages.contains_key(*s)))
    {
        return Err(ForgeError::bad("Source cursor does not cover these states"));
    }
    let mut base =
        super::super::issue::child(super::super::hosted::resource(c, None, None)?, &["issues"]);
    {
        let mut p = base.query_pairs_mut();
        if let Some(q) = &r.query {
            p.append_pair("query", q);
        }
        if let Some(l) = r.labels.first() {
            p.append_pair("label", l);
        }
        if let Some(a) = &r.assignee {
            p.append_pair("assignee", a);
        } else if r.involvement == Involvement::Assigned {
            p.append_pair("assignee", "@me");
        }
        if r.involvement == Involvement::Authored {
            p.append_pair("author", "@me");
        }
        if r.involvement == Involvement::Mentioned {
            p.append_pair("mention", "@me");
        }
    }
    let mut batches = Vec::new();
    let mut cutoff = None;
    for state in states {
        let next = pos
            .pages
            .entry(state.into())
            .or_insert_with(|| Some(String::new()));
        let mut rows = Vec::new();
        let mut oldest = None;
        let mut seen = BTreeSet::new();
        for _ in 0..5 {
            let Some(token) = next.clone() else { break };
            if !seen.insert(token.clone()) {
                return Err(invalid("repeated Source issue page"));
            }
            let mut url = base.clone();
            url.query_pairs_mut().append_pair("state", state);
            if !token.is_empty() {
                url.query_pairs_mut().append_pair("cursor", &token);
            }
            let page = get(c, url).await?;
            let entries = page["items"]
                .as_array()
                .ok_or_else(|| invalid("Source issue page"))?;
            for row in entries {
                let k = key(row).ok_or_else(|| invalid("Source issue sort key"))?;
                oldest = Some(k);
                if pos.after.is_none_or(|a| k < a) {
                    rows.push((k, token.clone(), row.clone()));
                }
            }
            *next = if entries.is_empty() {
                None
            } else {
                page["next"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
            };
            if rows.len() > r.limit() || next.is_none() {
                break;
            }
        }
        if next.is_some() {
            cutoff = cutoff.max(oldest);
        }
        batches.push((state, rows));
    }
    let mut merged = batches
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .filter(|(k, _, _)| cutoff.is_none_or(|c| *k >= c))
        .collect::<Vec<_>>();
    merged.sort_unstable_by_key(|(k, _, _)| Reverse(*k));
    let mut delivered = Vec::new();
    let mut numbers = BTreeSet::new();
    for (k, _, raw) in merged {
        if !numbers.insert(k.1) {
            continue;
        }
        pos.after = Some(*k);
        if raw["moderation"].as_str().is_some_and(|v| v != "visible") {
            continue;
        }
        let row = source_row(c, raw)?;
        if !r.labels.iter().skip(1).all(|name| {
            row["labels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["name"] == *name)
        }) {
            continue;
        }
        if r.involvement == Involvement::Assigned
            && r.assignee.is_some()
            && !row["assignees"].as_array().unwrap().iter().any(|a| {
                a["login"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(viewer.unwrap_or("")))
            })
        {
            continue;
        }
        delivered.push(row);
        if delivered.len() == r.limit() {
            break;
        }
    }
    for (state, rows) in batches {
        if let Some((_, p, _)) = rows
            .iter()
            .find(|(k, _, _)| pos.after.is_none_or(|a| *k < a))
        {
            pos.pages.insert(state.into(), Some(p.clone()));
        }
    }
    let next = pos
        .pages
        .values()
        .any(Option::is_some)
        .then(|| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&pos).expect("Source cursor")));
    if next
        .as_deref()
        .is_some_and(|s| Some(s) == cursor || s.len() > 4096)
    {
        return Err(invalid("Source search continuation"));
    }
    Ok(Page {
        items: delivered,
        next,
    })
}
