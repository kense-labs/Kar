use super::super::super::azure::{issue_body_field, organization_pull, wit_endpoint};
use super::super::azure::{actor, closed_states, summary};
use super::*;
use futures_util::TryStreamExt;

fn text(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn states(types: &Value, ty: &Value, current: &Value) -> Vec<Value> {
    let current_type = text(ty).unwrap_or_default();
    let mut states: Vec<_> = types["value"]
        .as_array()
        .iter()
        .flat_map(|r| r.iter())
        .find(|r| text(&r["name"]).is_some_and(|name| name.eq_ignore_ascii_case(current_type)))
        .and_then(|r| r["states"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let name = text(&r["name"])?;
            let closed = matches!(
                text(&r["category"]),
                Some("Resolved" | "Completed" | "Removed")
            );
            Some(json!({"name":name,"category":if closed{"closed"}else{"open"}}))
        })
        .collect();
    if states.is_empty() {
        for (category, names) in [
            ("open", ["New", "To Do", "Active", "Doing"]),
            ("closed", ["Resolved", "Done", "Closed", "Removed"]),
        ] {
            states.extend(names.map(|name| json!({"name":name,"category":category})));
        }
    }
    if !states.iter().any(|r| r["name"] == current["name"]) {
        states.push(current.clone());
    }
    states
}

fn comment(v: &Value) -> Option<Value> {
    if v["isDeleted"] == true {
        return None;
    }
    let id = v["id"].as_u64()?;
    let date = text(&v["createdDate"])?;
    let reactions:Vec<_>=v["reactions"].as_array().into_iter().flatten().filter_map(|r|{
        let count=r["count"].as_u64().filter(|n|*n>0)?;
        let content=match text(&r["type"])?.to_ascii_lowercase().as_str(){
            "like"=>"thumbs-up","dislike"=>"thumbs-down","heart"=>"heart","hooray"=>"hooray","smile"=>"laugh","confused"=>"confused",_=>return None
        };
        Some(json!({"content":content,"count":count,"actors":[],"viewerHasReacted":r["isCurrentUserEngaged"]==true}))
    }).collect();
    Some(
        json!({"id":id.to_string(),"createdAt":date,"author":actor(&v["createdBy"]),
        "url":null,"body":v["text"].as_str().unwrap_or_default(),"reactions":reactions}),
    )
}
async fn page(c: &Context, n: u64, cursor: Option<&str>) -> Result<Value, ForgeError> {
    if cursor.is_some_and(|s| s.starts_with('@')) {
        return Err(ForgeError::bad("invalid Azure comments cursor"));
    }
    let mut u = wit_endpoint(c, &["workitems", &n.to_string(), "comments"])?;
    u.set_query(None);
    u.query_pairs_mut().extend_pairs([
        ("api-version", "7.1-preview.4"),
        ("$expand", "reactions"),
        ("$top", "100"),
        ("order", "desc"),
    ]);
    if let Some(s) = cursor {
        u.query_pairs_mut().append_pair("continuationToken", s);
    }
    let v = get(c, u).await?;
    let items = v["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .filter_map(comment)
        .collect::<Vec<_>>();
    Ok(json!({"comments":items,"nextCursor":text(&v["continuationToken"])}))
}

async fn work_item(c: &Context, scope: &str, n: u64) -> Result<Value, ForgeError> {
    let mut u = wit_endpoint(c, &["workitems", &n.to_string()])?;
    u.query_pairs_mut().append_pair("$expand", "relations");
    let row = get(c, u).await?;
    let project = scope.rsplit('/').next().unwrap();
    if number(&row["id"])? != n
        || !text(&row["fields"]["System.TeamProject"])
            .is_some_and(|s| s.eq_ignore_ascii_case(project))
    {
        return Err(ForgeError::remote(
            404,
            "work item is outside the selected Azure project",
        ));
    }
    Ok(row)
}
pub(super) async fn comments(
    c: &Context,
    scope: &str,
    n: u64,
    cursor: Option<&str>,
) -> Result<Value, ForgeError> {
    work_item(c, scope, n).await?;
    page(c, n, cursor).await
}

fn tags(v: &Value) -> Vec<&str> {
    let mut seen = BTreeSet::new();
    v.as_str()
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty() && seen.insert(*s))
        .collect()
}
fn update(row: &Value, closed: &BTreeSet<String>) -> Vec<Value> {
    let mut events = Vec::new();
    let Some(id) = row["id"].as_u64().filter(|n| *n > 1) else {
        return events;
    };
    let fields = &row["fields"];
    let Some(date) =
        text(&fields["System.ChangedDate"]["newValue"]).or_else(|| text(&row["revisedDate"]))
    else {
        return events;
    };
    let who = actor(&row["revisedBy"]);
    let mut add = |kind: &str, detail: Option<&str>| {
        let key = format!("update-{id}-{}", events.len());
        events.push(json!({"kind":kind,"detail":detail,"actor":who,"createdAt":date,"id":key}));
    };
    let state = &fields["System.State"];
    if let (Some(old), Some(new)) = (text(&state["oldValue"]), text(&state["newValue"])) {
        let kind = match (
            closed.contains(&old.to_lowercase()),
            closed.contains(&new.to_lowercase()),
        ) {
            (true, false) => "reopened",
            (false, true) => "closed",
            _ => "state-changed",
        };
        add(kind, Some(new));
    }
    let title = &fields["System.Title"];
    if title["oldValue"].is_string() && title["newValue"].is_string() {
        add("renamed", text(&title["newValue"]));
    }
    let labels = &fields["System.Tags"];
    let before = tags(&labels["oldValue"]);
    let after = tags(&labels["newValue"]);
    for label in &before {
        if !after.contains(label) {
            add("unlabeled", Some(label));
        }
    }
    for label in &after {
        if !before.contains(label) {
            add("labeled", Some(label));
        }
    }
    let assigned = &fields["System.AssignedTo"];
    let before = actor(&assigned["oldValue"]);
    let after = actor(&assigned["newValue"]);
    let login = |v: &Value| v["login"].as_str().map(str::to_owned);
    let before = before.as_ref().and_then(login);
    let after = after.as_ref().and_then(login);
    if before != after {
        if let Some(who) = before {
            add("unassigned", Some(&who));
        }
        if let Some(who) = after {
            add("assigned", Some(&who));
        }
    }
    events
}
async fn updates(c: &Context, n: u64, closed: &BTreeSet<String>) -> Result<Vec<Value>, ForgeError> {
    let mut events = Vec::new();
    for skip in (0..500).step_by(100) {
        let mut u = wit_endpoint(c, &["workitems", &n.to_string(), "updates"])?;
        u.query_pairs_mut().append_pair("$top", "100");
        if skip > 0 {
            u.query_pairs_mut().append_pair("$skip", &skip.to_string());
        }
        let v = get(c, u).await?;
        let rows = v["value"]
            .as_array()
            .ok_or_else(|| invalid("Azure work item updates"))?;
        events.extend(rows.iter().flat_map(|r| update(r, closed)));
        if rows.len() < 100 {
            break;
        }
    }
    Ok(events)
}
fn linked_number(v: &Value) -> Option<u64> {
    if v["rel"] != "ArtifactLink" {
        return None;
    }
    let path = text(&v["url"])?.strip_prefix("vstfs:///Git/PullRequestId/")?;
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    let id = decoded.rsplit('/').next()?;
    let n = id.parse::<u64>().ok()?;
    (n > 0 && id.len() <= 10 && n.to_string() == id).then_some(n)
}
async fn pull(c: &Context, scope: &str, n: u64) -> Result<Option<Value>, ForgeError> {
    let raw = match get(c, organization_pull(c, n)?).await {
        Ok(v) => v,
        Err(e) if e.status == 404 => return Ok(None),
        Err(e) => return Err(e),
    };
    if number(&raw["pullRequestId"])? != n {
        return Err(invalid("linked Azure pull request number"));
    }
    let (Some(repo), Some(project)) = (
        text(&raw["repository"]["name"]),
        text(&raw["repository"]["project"]["name"]),
    ) else {
        return Ok(None);
    };
    let url = match text(&raw["_links"]["web"]["href"]) {
        Some(s) => s.to_owned(),
        None => {
            let root = raw["repository"]["webUrl"]
                .as_str()
                .map(Url::parse)
                .transpose()
                .map_err(|_| invalid("Azure repository URL"))?;
            let mut u = if let Some(u) = root {
                u
            } else {
                let host = if c.host == "ssh.dev.azure.com" {
                    "dev.azure.com"
                } else {
                    &c.host
                };
                let mut u =
                    Url::parse(&format!("https://{host}/")).map_err(|_| invalid("Azure host"))?;
                {
                    let mut parts = u.path_segments_mut().unwrap();
                    if let Some((org, _)) = scope.split_once('/') {
                        parts.push(org);
                    }
                    parts.extend([project, "_git", repo]);
                }
                u
            };
            u.path_segments_mut()
                .map_err(|_| invalid("Azure repository URL"))?
                .pop_if_empty()
                .extend(["pullrequest", &n.to_string()]);
            u.to_string()
        }
    };
    let state = match text(&raw["status"]) {
        Some("completed") => "merged",
        Some("abandoned") => "closed",
        Some("active") => "open",
        _ => return Err(invalid("Azure pull request state")),
    };
    Ok(Some(
        json!({"repository":format!("{project}/{repo}"),"number":n,"title":raw["title"],"url":url,"closesIssue":false,"isDraft":raw["isDraft"]==true,"state":state}),
    ))
}
pub(super) async fn read(c: &Context, scope: &str, n: u64) -> Result<Value, ForgeError> {
    let (raw, types) = tokio::try_join!(
        work_item(c, scope, n),
        get(c, wit_endpoint(c, &["workitemtypes"])?)
    )?;
    let closed = closed_states(&types)?;
    let mut result = summary(c, scope, &raw, &closed)?;
    let mut seen = BTreeSet::new();
    let ids = raw["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(linked_number)
        .filter(|n| seen.insert(*n))
        .take(10);
    let links = stream::iter(ids.map(|n| pull(c, scope, n)))
        .buffered(2)
        .try_collect::<Vec<_>>();
    let (comments, events, links) =
        tokio::try_join!(page(c, n, None), updates(c, n, &closed), links)?;
    let fields = &raw["fields"];
    let workflow = states(&types, &fields["System.WorkItemType"], &result["state"]);
    if let Some(state) = workflow
        .iter()
        .find(|s| s["name"] == result["state"]["name"])
    {
        result["state"] = state.clone();
    }
    result["states"] = json!(workflow);
    result["viewer"] = Value::Null;
    result["viewerPermissions"] =
        json!({"setState":true,"comment":true,"edit":true,"labels":true,"assignees":true});
    result["body"] = json!(
        fields[issue_body_field(fields)]
            .as_str()
            .unwrap_or_default()
    );
    result["comments"] = comments["comments"].clone();
    result["commentsCursor"] = comments["nextCursor"].clone();
    result["events"] = json!(events);
    result["linkedPullRequests"] = json!(links.into_iter().flatten().collect::<Vec<_>>());
    Ok(result)
}
