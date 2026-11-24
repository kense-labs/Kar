use super::super::super::issue::child;
use super::super::standalone::{source_actor, source_row};
use super::*;
pub(super) async fn read(
    c: &Context,
    n: u64,
    cursor: Option<&str>,
    comments_only: bool,
) -> Result<Value, ForgeError> {
    let mut url = child(
        super::super::super::hosted::resource(c, None, None)?,
        &["issues", &n.to_string()],
    );
    if let Some(cursor) = cursor {
        url.query_pairs_mut().append_pair("cursor", cursor);
    }
    let v = get(c, url).await?;
    if v["number"] != n
        || v["kind"] == "pull"
        || v["moderation"].as_str().is_some_and(|s| s != "visible")
    {
        return Err(ForgeError::remote(404, "Source issue not found"));
    }
    let mut row = source_row(c, &v)?;
    let mut notes = Vec::new();
    let mut events = Vec::new();
    let mut latest = v["createdAt"]
        .as_i64()
        .ok_or_else(|| invalid("Source creation date"))?;
    for (index, entry) in v["timeline"]
        .as_array()
        .ok_or_else(|| invalid("Source issue timeline"))?
        .iter()
        .enumerate()
    {
        let at = entry["at"]
            .as_i64()
            .ok_or_else(|| invalid("Source event timestamp"))?;
        latest = latest.max(at);
        if entry["moderation"].as_str().is_some_and(|s| s != "visible") {
            continue;
        }
        let time = jiff::Timestamp::from_millisecond(at)
            .map_err(|_| invalid("Source event timestamp"))?
            .to_string();
        if let Some(kind) = entry["event"].as_str() {
            let kind = match kind {
                "closed" | "reopened" | "labeled" | "unlabeled" | "assigned" | "unassigned"
                | "renamed" => kind,
                "linked_from" | "referenced" => "referenced",
                _ => continue,
            };
            events.push(json!({"id":format!("{n}:{at}:{index}"),"kind":kind,"actor":source_actor(c,entry["actor"].as_str()),"createdAt":time,"detail":null}));
        } else if let Some(id) = entry["id"].as_u64() {
            notes.push(json!({"id":id.to_string(),"author":source_actor(c,entry["author"].as_str()),"body":entry["comment"].as_str().unwrap_or(""),"createdAt":time,
                "url":format!("{}#comment-{id}",row["url"].as_str().unwrap())}));
        }
    }
    if comments_only {
        return Ok(json!({"comments":notes,"nextCursor":v["next"]}));
    }
    let me = get(c, path(c, &["user"])?)
        .await
        .ok()
        .and_then(|u| u["username"].as_str().map(str::to_owned))
        .filter(|s| !s.is_empty());
    let authenticated = me.is_some();
    let links=v["fixedBy"].as_array().into_iter().flatten().filter_map(|p|{
        let pn=p["number"].as_u64()?;let state=match p["state"].as_str(){Some("merged")=>"merged",Some("closed")=>"closed",_=>"open"};
        let base=row["url"].as_str()?.rsplit_once("/issues/")?.0;
        Some(json!({"repository":c.repository,"number":pn,"title":p["title"].as_str().map(str::trim).filter(|s|!s.is_empty()).map(str::to_string).unwrap_or_else(||format!("#{pn}")),"url":format!("{base}/pull/{pn}"),"state":state,"isDraft":false,"closesIssue":true}))
    }).collect::<Vec<_>>();
    row["updatedAt"] = json!(
        jiff::Timestamp::from_millisecond(latest)
            .map_err(|_| invalid("Source update time"))?
            .to_string()
    );
    row["commentCount"] = json!(v["comments"].as_u64().unwrap_or(notes.len() as u64));
    row["comments"] = json!(notes);
    row["commentsCursor"] = v["next"].clone();
    row["body"] = json!(v["body"].as_str().unwrap_or(""));
    row["states"] =
        json!([{"category":"open","name":"Open"},{"category":"closed","name":"Closed"}]);
    row["viewer"] = json!(me);
    row["viewerPermissions"] = json!({"setState":authenticated,"edit":authenticated,"labels":authenticated,"assignees":authenticated,"comment":authenticated&&v["locked"]!=true});
    row["events"] = json!(events);
    row["linkedPullRequests"] = json!(links);
    Ok(row)
}
