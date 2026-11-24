use super::*;

const DIRECTORY: &str = "query KarCreateDirectory($team:String!) { teams(first:1,filter:{key:{eq:$team}}){nodes{id key labels(first:250){nodes{id name}} members(first:250){nodes{id email displayName}}}} issueLabels(first:250,filter:{team:{null:true}}){nodes{id name}} }";
const COMMENTS: &str = "query KarIssueComments($id:String!,$after:String){issue(id:$id){id team{key} comments(first:50,after:$after){nodes{id body createdAt url user{displayName} botActor{name}} pageInfo{hasNextPage endCursor}}}}";

pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let (_, key) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("Linear team"))?;
    match op {
        Operation::CreateIssue {
            title,
            body,
            labels,
            assignees,
        } => {
            if assignees.len() > 1 {
                return Err(ForgeError::bad("Linear issues have one assignee"));
            }
            let data = gql(c, DIRECTORY, json!({"team":key}), false).await?;
            let team = &data["teams"]["nodes"][0];
            if team["key"] != key || !team["id"].is_string() {
                return Err(invalid("Linear team directory"));
            }
            let mut ids = vec![];
            for label in labels {
                let id = [&team["labels"]["nodes"], &data["issueLabels"]["nodes"]]
                    .into_iter()
                    .filter_map(Value::as_array)
                    .flatten()
                    .find(|r| {
                        r["name"]
                            .as_str()
                            .is_some_and(|s| s.eq_ignore_ascii_case(label))
                    })
                    .and_then(|r| r["id"].as_str())
                    .ok_or_else(|| ForgeError::bad(format!("unknown Linear label {label}")))?;
                ids.push(id);
            }
            let mut input =
                json!({"teamId":team["id"],"title":title,"description":body,"labelIds":ids});
            if let Some(assignee) = assignees.first() {
                let id = team["members"]["nodes"]
                    .as_array()
                    .and_then(|rows| {
                        rows.iter().find(|r| {
                            ["id", "displayName", "email"].iter().any(|f| {
                                r[f].as_str()
                                    .is_some_and(|s| s.eq_ignore_ascii_case(assignee))
                            })
                        })
                    })
                    .and_then(|r| r["id"].as_str())
                    .ok_or_else(|| ForgeError::bad("unknown Linear team member"))?;
                input["assigneeId"] = id.into();
            }
            let data=gql(c,"mutation KarCreateIssue($input:IssueCreateInput!){issueCreate(input:$input){success issue {number identifier url team{key}}}}",json!({"input":input}),true).await?;
            let value = &data["issueCreate"]["issue"];
            if data["issueCreate"]["success"] != true
                || value["team"]["key"] != key
                || !value["url"].is_string()
                || !value["identifier"].is_string()
                || number(&value["number"]).is_err()
            {
                return Err(ForgeError::uncertain(
                    "Linear did not confirm issue creation for the bound team",
                ));
            }
            Ok(
                json!({"number":value["number"],"key":value["identifier"],"displayKey":value["identifier"],"url":value["url"]}),
            )
        }
        Operation::CommentIssue { number, body } => {
            let item = detail(c, key, *number).await?;
            let reply=gql(c,"mutation KarCommentIssue($input:CommentCreateInput!){commentCreate(input:$input){success comment{id}}}",json!({"input":{"issueId":item["native_id"],"body":body}}),true).await?;
            if reply["commentCreate"]["success"] != true
                || !reply["commentCreate"]["comment"]["id"].is_string()
            {
                return Err(ForgeError::uncertain(
                    "Linear did not confirm its comment ID",
                ));
            }
            Ok(json!({"number":number,"comment_id":reply["commentCreate"]["comment"]["id"]}))
        }
        Operation::IssueComments { number, cursor } => {
            let data = gql(
                c,
                COMMENTS,
                json!({"id":format!("{key}-{number}"),"after":cursor}),
                false,
            )
            .await?;
            if data["issue"]["team"]["key"] != key {
                return Err(ForgeError::remote(404, "issue outside bound Linear team"));
            }
            let page = &data["issue"]["comments"];
            let rows=page["nodes"].as_array().ok_or_else(||invalid("Linear comment connection"))?.iter().map(|r|json!({"id":r["id"],"body":r["body"],"author":r["user"]["displayName"].as_str().or_else(||r["botActor"]["name"].as_str()),"url":r["url"],"createdAt":r["createdAt"]})).collect::<Vec<_>>();
            let next =
                (page["pageInfo"]["hasNextPage"] == true).then_some(&page["pageInfo"]["endCursor"]);
            Ok(json!({"comments":rows,"nextCursor":next}))
        }
        _ => unreachable!(),
    }
}
