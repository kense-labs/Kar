use super::super::linear::gql;
use super::*;
pub(super) async fn apply(
    c: &Context,
    n: u64,
    comment: Option<&str>,
    content: Content,
    want: bool,
    write: bool,
) -> Result<(), ForgeError> {
    let (_, team) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("Linear team"))?;
    let mut vars = json!({"issue":format!("{team}-{n}")});
    let fields = "reactions{id emoji user{id}}";
    let q = if let Some(id) = comment {
        vars["comment"] = json!(id);
        format!(
            "query KarIssueReaction($issue:String!,$comment:String!){{viewer{{id}} issue(id:$issue){{id team{{key}}}} comment(id:$comment){{id issue{{id}} {fields}}}}}"
        )
    } else {
        format!(
            "query KarIssueReaction($issue:String!){{viewer{{id}} issue(id:$issue){{id team{{key}} {fields}}}}}"
        )
    };
    let data = gql(c, &q, vars, false).await?;
    let issue = &data["issue"];
    if issue["team"]["key"] != team || !issue["id"].is_string() {
        return Err(ForgeError::remote(
            404,
            "issue is outside the bound Linear team",
        ));
    }
    let target = if let Some(id) = comment {
        if data["comment"]["id"] != id || data["comment"]["issue"]["id"] != issue["id"] {
            return Err(ForgeError::remote(
                404,
                "comment does not belong to the selected issue",
            ));
        }
        &data["comment"]
    } else {
        issue
    };
    let viewer = data["viewer"]["id"]
        .as_str()
        .ok_or_else(|| invalid("Linear viewer"))?;
    let own = target["reactions"]
        .as_array()
        .ok_or_else(|| invalid("Linear reactions"))?
        .iter()
        .find(|r| {
            r["user"]["id"] == viewer
                && (r["emoji"] == content.linear() || r["emoji"] == content.linear_name())
        });
    if satisfied(own.is_some(), want, write)? {
        return Ok(());
    }
    let (op, reply) = if want {
        let key = if comment.is_some() {
            "commentId"
        } else {
            "issueId"
        };
        ("reactionCreate",gql(c,"mutation KarAddIssueReaction($input:ReactionCreateInput!){reactionCreate(input:$input){success}}",json!({"input":{(key):target["id"],"emoji":content.linear()}}),true).await?)
    } else {
        let id = own
            .and_then(|r| r["id"].as_str())
            .ok_or_else(|| invalid("Linear reaction ID"))?;
        (
            "reactionDelete",
            gql(
                c,
                "mutation KarRemoveIssueReaction($id:String!){reactionDelete(id:$id){success}}",
                json!({"id":id}),
                true,
            )
            .await?,
        )
    };
    if reply[op]["success"] != true {
        return Err(ForgeError::uncertain("Linear did not confirm the reaction"));
    }
    Ok(())
}
