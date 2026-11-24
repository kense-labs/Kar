use super::super::super::linear::gql;
use super::super::super::reactions::Content;
use super::super::git_hosts::after;
use super::super::standalone::{linear_actor, linear_row, linear_state};
use super::*;

const USER: &str = "id displayName name email avatarUrl";
const BOT: &str = "name userDisplayName avatarUrl";
fn react_fields() -> String {
    format!("reactions{{emoji user{{{USER}}}}}")
}
fn note_fields() -> String {
    format!(
        "id body createdAt url user{{{USER}}} botActor{{{BOT}}} {}",
        react_fields()
    )
}
fn reactions(raw: &Value, viewer: Option<&str>) -> Vec<Value> {
    let mut groups = Vec::<(Content, usize, Vec<&str>, bool)>::new();
    for r in raw.as_array().into_iter().flatten() {
        let Some(content) = r["emoji"].as_str().and_then(Content::from_linear) else {
            continue;
        };
        let index = groups
            .iter()
            .position(|g| g.0 == content)
            .unwrap_or_else(|| {
                groups.push((content, 0, Vec::new(), false));
                groups.len() - 1
            });
        let group = &mut groups[index];
        group.1 += 1;
        if viewer.is_some() && r["user"]["id"].as_str() == viewer {
            group.3 = true
        } else if let Some(name) = login(&r["user"]) {
            group.2.push(name);
        }
    }
    groups.into_iter().map(|(content,count,actors,mine)|json!({"content":content,"count":count,"actors":actors,"viewerHasReacted":mine})).collect()
}
fn login(v: &Value) -> Option<&str> {
    ["displayName", "name", "email", "id"]
        .into_iter()
        .find_map(|key| v[key].as_str().filter(|s| !s.trim().is_empty()))
}
fn comments_page(
    raw: &Value,
    viewer: Option<&str>,
    cursor: Option<&str>,
) -> Result<Value, ForgeError> {
    let rows=raw["nodes"].as_array().ok_or_else(||invalid("Linear issue comments"))?.iter().filter_map(|r|Some(json!({"id":r["id"].as_str()?,"body":r["body"],"createdAt":r["createdAt"],"url":r["url"],
        "author":linear_actor(&r["user"],&r["botActor"]),"reactions":reactions(&r["reactions"],viewer)}))).collect::<Vec<_>>();
    Ok(json!({"comments":rows,"nextCursor":after(&raw["pageInfo"],cursor)?}))
}

fn events(raw: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for r in raw["nodes"].as_array().into_iter().flatten() {
        let Some(id) = r["id"].as_str() else { continue };
        let actor = linear_actor(&r["actor"], &r["botActor"]);
        let mut add = |kind: &str, detail: Option<&str>, suffix: &str| {
            out.push(json!({"id":format!("{id}:{suffix}"),"kind":kind,"createdAt":r["createdAt"],"actor":actor,"detail":detail}))
        };
        let before = &r["fromState"];
        let to = &r["toState"];
        if !to.is_null() && before["id"] != to["id"] {
            let kind = match (
                linear_state(before)["category"] == "closed",
                linear_state(to)["category"] == "closed",
            ) {
                (true, false) => "reopened",
                (false, true) => "closed",
                _ => "state-changed",
            };
            add(kind, to["name"].as_str(), "state");
        }
        for (key, kind, prefix) in [
            ("addedLabels", "labeled", "label+"),
            ("removedLabels", "unlabeled", "label-"),
        ] {
            for l in r[key].as_array().into_iter().flatten() {
                if let Some(id) = l["id"].as_str() {
                    add(kind, l["name"].as_str(), &format!("{prefix}{id}"));
                }
            }
        }
        if r["fromAssignee"]["id"] != r["toAssignee"]["id"] {
            if !r["fromAssignee"].is_null() {
                add("unassigned", login(&r["fromAssignee"]), "unassigned");
            }
            if !r["toAssignee"].is_null() {
                add("assigned", login(&r["toAssignee"]), "assigned");
            }
        }
        if let Some(title) = r["toTitle"].as_str()
            && r["fromTitle"] != title
        {
            add("renamed", Some(title), "title");
        }
    }
    out
}
fn links(raw: &Value) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    raw["nodes"].as_array().into_iter().flatten().filter_map(|a|{
        let href=a["url"].as_str()?;let u=Url::parse(href).ok()?;
        if u.scheme()!="https"||!u.username().is_empty()||u.password().is_some()||u.port().is_some(){return None}
        let path=u.path().trim_matches('/');let (repo,num)=match u.host_str()?{"github.com"=>{let pair=path.rsplit_once("/pull/")?;if pair.0.split('/').count()!=2{return None}pair},"gitlab.com"=>path.rsplit_once("/-/merge_requests/")?,_=>return None};
        let n=num.parse::<u64>().ok().filter(|n|*n>0)?;if repo.is_empty()||!seen.insert(href){return None}
        let m=&a["metadata"];let state=match m["status"].as_str(){Some("merged")=>"merged",Some("closed")=>"closed",_=>"open"};
        Some(json!({"repository":repo,"number":n,"title":a["title"].as_str().map(str::trim).filter(|s|!s.is_empty()).unwrap_or(href),"url":href,"state":state,"isDraft":m["status"]=="draft"||m["draft"]==true,"closesIssue":false}))
    }).collect()
}
fn ensure(c: &Context, data: &Value, n: u64, team: &str) -> Result<(), ForgeError> {
    if let Some((org, _)) = c.repository.split_once('/')
        && data["organization"]["urlKey"] != org
    {
        return Err(ForgeError::remote(
            403,
            "Linear credential is outside the bound workspace",
        ));
    }
    let i = &data["issue"];
    if i["team"]["key"] != team || i["number"] != n {
        return Err(ForgeError::remote(404, "unknown Linear team issue"));
    }
    Ok(())
}
async fn viewer(c: &Context) -> Value {
    gql(
        c,
        "query KarIssueDetailsViewer{viewer{id displayName name avatarUrl}}",
        json!({}),
        false,
    )
    .await
    .ok()
    .and_then(|r| r.get("viewer").cloned())
    .unwrap_or_default()
}
pub(super) async fn comments(
    c: &Context,
    team: &str,
    n: u64,
    cursor: Option<&str>,
) -> Result<Value, ForgeError> {
    let q = format!(
        "query KarLinearIssueComments($id:String!,$after:String){{organization{{urlKey}} issue(id:$id){{team{{key}} number comments(first:50,orderBy:createdAt,after:$after){{nodes{{{}}} pageInfo{{hasNextPage endCursor}}}}}}}}",
        note_fields()
    );
    let data = gql(
        c,
        &q,
        json!({"id":format!("{team}-{n}"),"after":cursor}),
        false,
    )
    .await?;
    ensure(c, &data, n, team)?;
    let connection = &data["issue"]["comments"];
    let viewer = if connection["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| {
            r["reactions"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        }) {
        viewer(c).await
    } else {
        Value::Null
    };
    comments_page(connection, viewer["id"].as_str(), cursor)
}
pub(super) async fn read(c: &Context, team: &str, n: u64) -> Result<Value, ForgeError> {
    let note = note_fields();
    let reacts = react_fields();
    let q = format!(
        "query KarLinearFullIssue($id:String!){{organization{{urlKey}} issue(id:$id){{number identifier title url description createdAt updatedAt completedAt canceledAt state{{name type}} team{{key states(first:100){{nodes{{name type position}}}}}} creator{{{USER}}} botActor{{{BOT}}} assignee{{{USER}}} labels{{nodes{{name color}}}} {reacts} comments(first:50,orderBy:createdAt){{nodes{{{note}}} pageInfo{{hasNextPage endCursor}}}} attachments(first:50){{nodes{{url title metadata}}}} history(first:100,orderBy:createdAt){{nodes{{id createdAt actor{{{USER}}} botActor{{{BOT}}} fromState{{id type name}} toState{{id type name}} fromAssignee{{{USER}}} toAssignee{{{USER}}} addedLabels{{id name}} removedLabels{{id name}} fromTitle toTitle}}}}}}}}"
    );
    let (data, me) = tokio::join!(
        gql(c, &q, json!({"id":format!("{team}-{n}")}), false),
        viewer(c)
    );
    let data = data?;
    ensure(c, &data, n, team)?;
    let i = &data["issue"];
    let mut row = linear_row(i, team)?;
    let comments = comments_page(&i["comments"], me["id"].as_str(), None)?;
    let mut states = i["team"]["states"]["nodes"]
        .as_array()
        .ok_or_else(|| invalid("Linear workflow states"))?
        .clone();
    let types = [
        "triage",
        "backlog",
        "unstarted",
        "started",
        "completed",
        "canceled",
        "duplicate",
    ];
    states.sort_by(|a, b| {
        let rank = |v: &Value| {
            types
                .iter()
                .position(|t| v["type"] == *t)
                .map_or(-1, |i| i as i32)
        };
        rank(a).cmp(&rank(b)).then_with(|| {
            a["position"]
                .as_f64()
                .unwrap_or(0.)
                .total_cmp(&b["position"].as_f64().unwrap_or(0.))
        })
    });
    row["body"] = json!(i["description"].as_str().unwrap_or(""));
    row["viewer"] = json!(login(&me));
    row["states"] = json!(states.iter().map(linear_state).collect::<Vec<_>>());
    row["viewerPermissions"] =
        json!({"edit":true,"comment":true,"setState":true,"labels":true,"assignees":true});
    row["commentCount"] = json!(comments["comments"].as_array().unwrap().len());
    row["comments"] = comments["comments"].clone();
    row["commentsCursor"] = comments["nextCursor"].clone();
    row["reactions"] = json!(reactions(&i["reactions"], me["id"].as_str()));
    row["events"] = json!(events(&i["history"]));
    row["linkedPullRequests"] = json!(links(&i["attachments"]));
    Ok(row)
}
