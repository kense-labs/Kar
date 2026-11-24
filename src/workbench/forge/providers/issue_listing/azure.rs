use super::super::azure::{issue_scope, wit_endpoint};
use super::*;
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
pub(super) fn actor(v: &Value) -> Option<Value> {
    let clean = |s: &str| !s.trim().is_empty();
    let login = [
        v.as_str(),
        v["uniqueName"].as_str(),
        v["displayName"].as_str(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|s| clean(s))?;
    Some(
        json!({"login":login,"name":v["displayName"].as_str().map(str::trim).filter(|s|clean(s)),
        "avatarUrl":v["imageUrl"].as_str().map(str::trim).filter(|s|clean(s))}),
    )
}

pub(super) fn closed_states(types: &Value) -> Result<BTreeSet<String>, ForgeError> {
    let mut closed = BTreeSet::new();
    for t in types["value"]
        .as_array()
        .ok_or_else(|| invalid("Azure work item types"))?
    {
        for s in t["states"].as_array().into_iter().flatten() {
            if matches!(
                s["category"].as_str(),
                Some("Resolved" | "Completed" | "Removed")
            ) && let Some(name) = s["name"].as_str()
            {
                closed.insert(name.to_lowercase());
            }
        }
    }
    if closed.is_empty() {
        closed.extend(["closed", "done", "removed", "resolved", "completed"].map(str::to_string));
    }
    Ok(closed)
}
pub(super) fn summary(
    c: &Context,
    scope: &str,
    item: &Value,
    closed: &BTreeSet<String>,
) -> Result<Value, ForgeError> {
    let project = scope.rsplit('/').next().unwrap();
    let n = number(&item["id"])?;
    let f = &item["fields"];
    if !f["System.TeamProject"]
        .as_str()
        .is_some_and(|p| p.eq_ignore_ascii_case(project))
    {
        return Err(ForgeError::remote(
            404,
            "work item is outside the Azure project",
        ));
    }
    let name = f["System.State"].as_str().unwrap_or("New");
    let tags = f["System.Tags"]
        .as_str()
        .unwrap_or("")
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>();
    let url = match item.pointer("/_links/html/href").and_then(Value::as_str) {
        Some(url) => url.to_string(),
        None => {
            let mut url = Url::parse(&format!(
                "https://{}/",
                if c.host == "ssh.dev.azure.com" {
                    "dev.azure.com"
                } else {
                    &c.host
                }
            ))
            .map_err(|_| invalid("Azure host"))?;
            url.path_segments_mut()
                .unwrap()
                .extend(scope.split('/'))
                .extend(["_workitems", "edit", &n.to_string()]);
            url.to_string()
        }
    };
    let created = f["System.CreatedDate"]
        .as_str()
        .ok_or_else(|| invalid("Azure issue creation date"))?;
    let value = json!({"key":n.to_string(),"displayKey":format!("#{n}"),"title":f["System.Title"],"url":url,"author":actor(&f["System.CreatedBy"]),
            "state":{"category":if closed.contains(&name.to_lowercase()){"closed"}else{"open"},"name":name},"createdAt":created,"updatedAt":f["System.ChangedDate"].as_str().unwrap_or(created),
            "closedAt":f["Microsoft.VSTS.Common.ClosedDate"],"assignees":actor(&f["System.AssignedTo"]).into_iter().collect::<Vec<_>>(),"labels":tags.into_iter().map(|s|json!({"name":s,"color":null})).collect::<Vec<_>>(),"commentCount":f["System.CommentCount"].as_u64().unwrap_or(0)});
    Ok(value)
}
pub(super) async fn listing(
    c: &Context,
    r: &Request,
    cursor: Option<&str>,
) -> Result<Page, ForgeError> {
    let created = r.sort == Sort::Created;
    let date = if created {
        "[System.CreatedDate]"
    } else {
        "[System.ChangedDate]"
    };
    let mut clauses = vec!["[System.TeamProject] = @project".to_string()];
    if let Some(token) = cursor {
        let (time, id) = token
            .rsplit_once('|')
            .ok_or_else(|| ForgeError::bad("invalid Azure issue cursor"))?;
        let n = id
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0 && id.len() <= 10 && n.to_string() == id);
        if n.is_none()
            || !time.ends_with('Z')
            || !time.starts_with(|ch: char| ch.is_ascii_digit())
            || time.parse::<jiff::Timestamp>().is_err()
        {
            return Err(ForgeError::bad("invalid Azure issue cursor"));
        }
        clauses.push(format!(
            "({date} < {at} OR ({date} = {at} AND [System.Id] < {id}))",
            at = quote(time)
        ));
    }
    let types = get(c, wit_endpoint(c, &["workitemtypes"])?).await?;
    let closed = closed_states(&types)?;
    if r.state != "all" {
        clauses.push(format!(
            "[System.State] {} IN ({})",
            if r.state == "open" { "NOT" } else { "" },
            closed
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if r.involvement == Involvement::Assigned {
        clauses.push("[System.AssignedTo] = @Me".into());
    }
    if r.involvement == Involvement::Authored {
        clauses.push("[System.CreatedBy] = @Me".into());
    }
    if let Some(q) = &r.query {
        clauses.push(format!("[System.Title] CONTAINS {}", quote(q)));
    }
    for l in &r.labels {
        clauses.push(format!("[System.Tags] CONTAINS {}", quote(l)));
    }
    if let Some(a) = &r.assignee {
        clauses.push(format!("[System.AssignedTo] = {}", quote(a)));
    }
    let limit = r.limit().min(100);
    let mut url = wit_endpoint(c, &["wiql"])?;
    url.query_pairs_mut()
        .append_pair("timePrecision", "true")
        .append_pair("$top", &(limit + 1).to_string());
    let query = format!(
        "SELECT [System.Id] FROM WorkItems WHERE {} ORDER BY {date} DESC, [System.Id] DESC",
        clauses.join(" AND ")
    );
    let response = send(c, Method::POST, url, Some(json!({"query":query})), false).await?;
    let ids = response["workItems"]
        .as_array()
        .ok_or_else(|| invalid("WIQL issue page"))?
        .iter()
        .map(|i| number(&i["id"]))
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(Page {
            items: vec![],
            next: None,
        });
    }
    if ids.len() > limit + 1 || ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
        return Err(invalid("WIQL work item membership"));
    }
    let ids_to_read = &ids[..ids.len().min(limit)];
    let reply = send(
        c,
        Method::POST,
        wit_endpoint(c, &["workitemsbatch"])?,
        Some(json!({"ids":ids_to_read,"errorPolicy":"Fail"})),
        false,
    )
    .await?;
    let scope = issue_scope(c)?;
    let mut found = BTreeMap::new();
    for item in reply["value"]
        .as_array()
        .ok_or_else(|| invalid("Azure work item page"))?
    {
        let n = number(&item["id"])?;
        let value = summary(c, &scope, item, &closed)?;
        if !ids_to_read.contains(&n) || found.insert(n, value).is_some() {
            return Err(invalid("Azure batch membership"));
        }
    }
    let items = ids_to_read
        .iter()
        .map(|n| {
            found
                .remove(n)
                .ok_or_else(|| invalid("missing Azure work item"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next = if ids.len() > limit {
        let v = items.last().unwrap();
        Some(format!(
            "{}|{}",
            v[if created { "createdAt" } else { "updatedAt" }]
                .as_str()
                .unwrap(),
            v["key"].as_str().unwrap()
        ))
    } else {
        None
    };
    Ok(Page { items, next })
}
