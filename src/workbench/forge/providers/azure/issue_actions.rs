use super::*;

pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let location = Location::from(c)?;
    match op {
        Operation::CreateIssue {
            title,
            body,
            labels,
            assignees,
        } => {
            if assignees.len() > 1 {
                return Err(ForgeError::bad("Azure work items have one assignee"));
            }
            let data = get(c, location.url(c, true, "wit", &["workitemtypes"])?).await?;
            let names: Vec<_> = data["value"]
                .as_array()
                .ok_or_else(|| invalid("Azure work item types"))?
                .iter()
                .filter(|r| r["isDisabled"] != true)
                .filter_map(|r| r["name"].as_str())
                .collect();
            let selected = ["Issue", "Bug", "Task", "User Story", "Product Backlog Item"]
                .into_iter()
                .find(|s| names.contains(s))
                .or_else(|| names.first().copied())
                .ok_or_else(|| ForgeError::bad("Azure project offers no work item types"))?;
            let mut fields = vec![json!({"op":"add","path":"/fields/System.Title","value":title})];
            for (field, value) in [
                ("System.Description", body.clone()),
                ("System.Tags", labels.join("; ")),
                (
                    "System.AssignedTo",
                    assignees.first().cloned().unwrap_or_default(),
                ),
            ] {
                if !value.is_empty() {
                    fields
                        .push(json!({"op":"add","path":format!("/fields/{field}"),"value":value}));
                }
            }
            let raw = send(
                c,
                Method::POST,
                location.url(c, true, "wit", &["workitems", &format!("${selected}")])?,
                Some(json!(fields)),
                true,
            )
            .await?;
            let item = location.issue(&raw).map_err(|_| {
                ForgeError::uncertain("Azure did not confirm the created work item in this project")
            })?;
            Ok(
                json!({"number":item["number"],"key":item["number"].to_string(),"displayKey":format!("#{}",item["number"]),"url":item["url"]}),
            )
        }
        Operation::CommentIssue { number: n, .. } | Operation::IssueComments { number: n, .. } => {
            location.detail(c, *n).await?;
            let mut url =
                location.url(c, true, "wit", &["workitems", &n.to_string(), "comments"])?;
            url.query_pairs_mut()
                .clear()
                .append_pair("api-version", "7.1-preview.4");
            if let Operation::CommentIssue { body, .. } = op {
                url.query_pairs_mut().append_pair("format", "markdown");
                let r = send(c, Method::POST, url, Some(json!({"text":body})), true).await?;
                return Ok(json!({"number":n,"comment_id":r["id"]}));
            }
            let Operation::IssueComments { cursor, .. } = op else {
                unreachable!()
            };
            url.query_pairs_mut()
                .append_pair("$top", "50")
                .append_pair("order", "desc")
                .append_pair("$expand", "reactions");
            if let Some(cursor) = cursor {
                url.query_pairs_mut()
                    .append_pair("continuationToken", cursor);
            }
            let result = get(c, url).await?;
            let mut comments = vec![];
            if let Some(rows) = result["comments"].as_array() {
                for r in rows.iter().rev().filter(|r| r["isDeleted"] != true) {
                    comments.push(json!({"id":number(&r["id"])?.to_string(),"body":r["text"],"author":r["createdBy"]["displayName"],"createdAt":r["createdDate"]}));
                }
            }
            Ok(json!({"comments":comments,"nextCursor":result["continuationToken"]}))
        }
        _ => unreachable!(),
    }
}
