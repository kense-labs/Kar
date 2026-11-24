use super::super::super::reviews::github::gql;
use super::super::git_hosts::{actor, summary};
use super::*;

const GROUPS: &str = "reactionGroups{content viewerHasReacted reactors(first:10){totalCount nodes{... on User{login} ... on Organization{login} ... on Bot{login} ... on Mannequin{login}}}}";
const PULL: &str = "number title url state isDraft repository{nameWithOwner}";
const USER: &str = "login avatarUrl ... on User{name}";
const BACK: &str = "pageInfo{hasPreviousPage startCursor}";
const FORWARD: &str = "pageInfo{hasNextPage endCursor}";
fn note_fields() -> String {
    format!("id author{{{USER}}} body createdAt url {GROUPS}")
}
fn vars(c: &Context, n: u64) -> Result<Value, ForgeError> {
    let (owner, repo) = c
        .repository
        .split_once('/')
        .filter(|(o, r)| !o.is_empty() && !r.is_empty() && !r.contains('/'))
        .ok_or_else(|| ForgeError::bad("invalid GitHub issue scope"))?;
    if n > i32::MAX as u64 {
        return Err(ForgeError::remote(404, "GitHub issue number outside range"));
    }
    Ok(json!({"owner":owner,"repo":repo,"n":n}))
}
fn reactions(v: &Value, viewer: Option<&str>) -> Vec<Value> {
    v.as_array().into_iter().flatten().filter_map(|group|{
        let content=group["content"].as_str()?.to_ascii_lowercase().replace('_',"-");
        if !matches!(content.as_str(),"thumbs-up"|"thumbs-down"|"laugh"|"hooray"|"confused"|"heart"|"rocket"|"eyes"){return None}
        let all=group["reactors"]["nodes"].as_array().into_iter().flatten().filter_map(|a|a["login"].as_str()).collect::<Vec<_>>();
        let count=group["reactors"]["totalCount"].as_u64().unwrap_or(0).max(all.len() as u64);
        (count>0).then(||json!({"content":content,"count":count,"actors":all.into_iter().filter(|a|viewer.is_none_or(|v|!v.eq_ignore_ascii_case(a))).collect::<Vec<_>>(),"viewerHasReacted":group["viewerHasReacted"]==true}))
    }).collect()
}
fn comment_page(v: &Value, viewer: Option<&str>) -> Result<Value, ForgeError> {
    let notes=v["nodes"].as_array().ok_or_else(||invalid("GitHub issue comment connection"))?.iter().filter_map(|n|{
        Some(json!({"id":n["id"].as_str()?,"author":actor(&n["author"],"login"),"createdAt":n["createdAt"].as_str()?,"url":n["url"],
            "body":n["body"].as_str().unwrap_or(""),"reactions":reactions(&n["reactionGroups"],viewer)}))
    }).collect::<Vec<_>>();
    let cursor = if v["pageInfo"]["hasPreviousPage"] == true {
        Some(
            v["pageInfo"]["startCursor"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 4096)
                .ok_or_else(|| invalid("GitHub previous comments cursor"))?,
        )
    } else {
        None
    };
    Ok(json!({"comments":notes,"nextCursor":cursor}))
}
pub(super) async fn comments(
    c: &Context,
    n: u64,
    cursor: Option<&str>,
) -> Result<Value, ForgeError> {
    let mut args = vars(c, n)?;
    args["before"] = json!(cursor);
    let fields = note_fields();
    let q = format!(
        "query KarIssueCommentPage($owner:String!,$repo:String!,$n:Int!,$before:String){{viewer{{login}} repository(owner:$owner,name:$repo){{issue(number:$n){{number comments(last:100,before:$before){{{BACK} nodes{{{fields}}}}}}}}}}}"
    );
    let data = gql(c, &q, args, false).await?;
    let issue = &data["repository"]["issue"];
    if issue["number"] != n {
        return Err(ForgeError::remote(404, "issue not found"));
    }
    comment_page(&issue["comments"], data["viewer"]["login"].as_str())
}
fn linked(v: &Value, closes: bool) -> Option<Value> {
    let repository = v["repository"]["nameWithOwner"].as_str()?;
    let n = v["number"].as_u64()?;
    let state = match v["state"].as_str()? {
        "MERGED" => "merged",
        "CLOSED" => "closed",
        "OPEN" => "open",
        _ => return None,
    };
    Some(
        json!({"repository":repository,"number":n,"title":v["title"].as_str()?,"url":v["url"].as_str()?,"state":state,"isDraft":v["isDraft"]==true,"closesIssue":closes}),
    )
}
fn event_fields() -> String {
    let mut fields = String::from("__typename");
    for (name, extra) in [
        ("ClosedEvent", ""),
        ("ReopenedEvent", ""),
        ("LabeledEvent", "label{name}"),
        ("UnlabeledEvent", "label{name}"),
        (
            "AssignedEvent",
            "assignee{... on User{login} ... on Bot{login}}",
        ),
        (
            "UnassignedEvent",
            "assignee{... on User{login} ... on Bot{login}}",
        ),
        ("RenamedTitleEvent", "currentTitle"),
    ] {
        fields.push_str(&format!(
            " ... on {name}{{id createdAt actor{{login avatarUrl}} {extra}}}"
        ));
    }
    fields.push_str(&format!(" ... on CrossReferencedEvent{{id createdAt actor{{login avatarUrl}} source{{__typename ... on PullRequest{{{PULL}}} ... on Issue{{number url repository{{nameWithOwner}}}}}}}}"));
    fields
}
fn event(v: &Value) -> Option<Value> {
    let node = &v["source"];
    let mut reference = None;
    let (name, detail) = match v["__typename"].as_str()? {
        "ClosedEvent" => ("closed", Value::Null),
        "ReopenedEvent" => ("reopened", Value::Null),
        "LabeledEvent" => ("labeled", v["label"]["name"].clone()),
        "UnlabeledEvent" => ("unlabeled", v["label"]["name"].clone()),
        "AssignedEvent" => ("assigned", v["assignee"]["login"].clone()),
        "UnassignedEvent" => ("unassigned", v["assignee"]["login"].clone()),
        "RenamedTitleEvent" => ("renamed", v["currentTitle"].clone()),
        "CrossReferencedEvent" => {
            let desc = if let (Some(repo), Some(n), Some(url)) = (
                node["repository"]["nameWithOwner"].as_str(),
                node["number"].as_u64(),
                node["url"].as_str(),
            ) {
                reference = Some(
                    json!({"repository":repo,"number":n,"url":url,"kind":if node["__typename"]=="PullRequest"{"pull-request"}else{"issue"}}),
                );
                json!(format!("{repo}#{n}"))
            } else {
                Value::Null
            };
            ("referenced", desc)
        }
        _ => return None,
    };
    let mut result = json!({"id":v["id"].as_str()?,"createdAt":v["createdAt"].as_str()?,"actor":actor(&v["actor"],"login"),"kind":name,"detail":detail});
    if let Some(reference) = reference {
        result["reference"] = reference
    }
    Some(result)
}
async fn complete(
    c: &Context,
    args: &Value,
    node: &mut Value,
    name: &str,
    fields: &str,
    options: &str,
) -> Result<(), ForgeError> {
    // Each directory advances independently. Keep the accumulated nodes and reject a non-progressing cursor.
    let mut seen = BTreeSet::new();
    while node[name]["pageInfo"]["hasNextPage"] == true {
        let cursor = node[name]["pageInfo"]["endCursor"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .ok_or_else(|| invalid("GitHub issue collection cursor"))?;
        if !seen.insert(cursor.to_owned()) {
            return Err(invalid("repeated GitHub issue collection cursor"));
        }
        let mut args = args.clone();
        args["after"] = json!(cursor);
        let q = format!(
            "query KarIssueCollection($owner:String!,$repo:String!,$n:Int!,$after:String!){{repository(owner:$owner,name:$repo){{issue(number:$n){{id {name}(first:100,after:$after{options}){{{FORWARD} nodes{{{fields}}}}}}}}}}}"
        );
        let data = gql(c, &q, args, false).await?;
        let issue = &data["repository"]["issue"];
        if issue["id"] != node["id"] {
            return Err(invalid("GitHub issue collection identity"));
        }
        let entries = issue[name]["nodes"]
            .as_array()
            .ok_or_else(|| invalid("GitHub issue collection nodes"))?;
        node[name]["nodes"]
            .as_array_mut()
            .ok_or_else(|| invalid("GitHub issue collection"))?
            .extend(entries.iter().cloned());
        node[name]["pageInfo"] = issue[name]["pageInfo"].clone();
        if !node[name]["pageInfo"]["hasNextPage"].is_boolean() {
            return Err(invalid("GitHub issue collection pagination"));
        }
    }
    Ok(())
}
pub(super) async fn read(c: &Context, n: u64) -> Result<Value, ForgeError> {
    let args = vars(c, n)?;
    let fields = note_fields();
    let events = event_fields();
    let q = format!(
        "query KarFullIssue($owner:String!,$repo:String!,$n:Int!){{viewer{{login}} repository(owner:$owner,name:$repo){{viewerPermission issue(number:$n){{id number title url state stateReason createdAt updatedAt closedAt author{{{USER}}} labels(first:100){{{FORWARD} nodes{{name color}}}} assignees(first:100){{{FORWARD} nodes{{login name avatarUrl}}}} body viewerCanUpdate viewerCanClose viewerCanReopen {GROUPS} comments(last:100){{totalCount {BACK} nodes{{{fields}}}}} closedByPullRequestsReferences(first:20,includeClosedPrs:true){{{FORWARD} nodes{{{PULL}}}}} timelineItems(last:100,itemTypes:[CLOSED_EVENT REOPENED_EVENT LABELED_EVENT UNLABELED_EVENT ASSIGNED_EVENT UNASSIGNED_EVENT RENAMED_TITLE_EVENT CROSS_REFERENCED_EVENT]){{nodes{{{events}}}}}}}}}}}"
    );
    let data = gql(c, &q, args.clone(), false).await?;
    let mut issue = data["repository"]["issue"].clone();
    if issue["number"] != n || !issue["id"].is_string() {
        return Err(ForgeError::remote(404, "GitHub issue not found"));
    }
    for (key, fields, options) in [
        ("labels", "name color", ""),
        ("assignees", "login name avatarUrl", ""),
        (
            "closedByPullRequestsReferences",
            PULL,
            ",includeClosedPrs:true",
        ),
    ] {
        complete(c, &args, &mut issue, key, fields, options).await?;
    }
    let viewer = data["viewer"]["login"].as_str();
    let notes = comment_page(&issue["comments"], viewer)?;
    let mut links = BTreeMap::new();
    let mut events = Vec::new();
    for row in issue["closedByPullRequestsReferences"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| linked(v, true))
    {
        links.insert(
            (
                row["repository"].as_str().unwrap().to_lowercase(),
                row["number"].as_u64().unwrap(),
            ),
            row,
        );
    }
    for v in issue["timelineItems"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if v["source"]["__typename"] == "PullRequest"
            && let Some(row) = linked(&v["source"], false)
        {
            links
                .entry((
                    row["repository"].as_str().unwrap().to_lowercase(),
                    row["number"].as_u64().unwrap(),
                ))
                .or_insert(row);
        }
        if let Some(e) = event(v) {
            events.push(e);
        }
    }
    let mut row = summary(&issue, "github").ok_or_else(|| invalid("GitHub issue summary"))?;
    let triage = matches!(
        data["repository"]["viewerPermission"].as_str(),
        Some("ADMIN" | "MAINTAIN" | "WRITE" | "TRIAGE")
    );
    row["viewerPermissions"] = json!({"setState":issue[if issue["state"]=="CLOSED"{"viewerCanReopen"}else{"viewerCanClose"}],"edit":issue["viewerCanUpdate"],"labels":triage,"assignees":triage,"comment":true});
    row["states"] = json!([{"category":"open","name":"Open"},{"category":"closed","name":"Completed","closeReason":"completed"},{"category":"closed","name":"Not planned","closeReason":"not-planned"},{"category":"closed","name":"Duplicate","closeReason":"duplicate"}]);
    row["body"] = json!(issue["body"].as_str().unwrap_or(""));
    row["viewer"] = json!(viewer);
    row["reactions"] = json!(reactions(&issue["reactionGroups"], viewer));
    row["commentsCursor"] = notes["nextCursor"].clone();
    row["comments"] = notes["comments"].clone();
    row["events"] = json!(events);
    row["linkedPullRequests"] = json!(links.into_values().collect::<Vec<_>>());
    Ok(row)
}
