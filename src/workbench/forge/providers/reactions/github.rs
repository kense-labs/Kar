use super::super::reviews::github::gql;
use super::*;

pub(super) async fn apply(
    c: &Context,
    n: u64,
    subject: Option<&str>,
    content: Content,
    want: bool,
    write: bool,
) -> Result<(), ForgeError> {
    let (owner, name) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("GitHub repository"))?;
    let mut vars = json!({"owner":owner,"name":name,"n":n});
    let groups = "reactionGroups{content viewerHasReacted}";
    let mut node = subject.map(str::to_owned);
    if let Some(id) = subject
        && id.bytes().all(|b| b.is_ascii_digit())
    {
        positive(id)?;
        let repo = super::super::rest::project(c)?;
        let raw = get(
            c,
            super::super::issue::child(repo, &["issues", "comments", id]),
        )
        .await?;
        node = Some(
            raw["node_id"]
                .as_str()
                .ok_or_else(|| invalid("GitHub comment node ID"))?
                .into(),
        );
    }
    let query = if let Some(node) = &node {
        vars["node"] = json!(node);
        format!(
            "query KarIssueReaction($owner:String!,$name:String!,$n:Int!,$node:ID!){{repository(owner:$owner,name:$name){{issue(number:$n){{id}}}} node(id:$node){{__typename ... on IssueComment{{id issue{{id}} {groups}}}}}}}"
        )
    } else {
        format!(
            "query KarIssueReaction($owner:String!,$name:String!,$n:Int!){{repository(owner:$owner,name:$name){{issue(number:$n){{id {groups}}}}}}}"
        )
    };
    let data = gql(c, &query, vars, false).await?;
    let issue = &data["repository"]["issue"];
    let root = issue["id"]
        .as_str()
        .ok_or_else(|| ForgeError::remote(404, "unknown GitHub issue"))?;
    let target = if let Some(id) = node {
        let comment = &data["node"];
        if comment["__typename"] != "IssueComment"
            || comment["id"] != id
            || comment["issue"]["id"] != root
        {
            return Err(ForgeError::remote(
                404,
                "comment does not belong to the selected issue",
            ));
        }
        comment
    } else {
        issue
    };
    let groups = target["reactionGroups"]
        .as_array()
        .ok_or_else(|| invalid("GitHub reaction groups"))?;
    let current = groups
        .iter()
        .find(|g| g["content"] == content.github())
        .map(|g| {
            g["viewerHasReacted"]
                .as_bool()
                .ok_or_else(|| invalid("GitHub viewer reaction"))
        })
        .transpose()?
        .unwrap_or(false);
    if satisfied(current, want, write)? {
        return Ok(());
    }
    let operation = if want {
        "addReaction"
    } else {
        "removeReaction"
    };
    let query = format!(
        "mutation KarSetIssueReaction($node:ID!,$content:ReactionContent!){{{operation}(input:{{subjectId:$node,content:$content}}){{subject{{id}}}}}}"
    );
    let result = gql(
        c,
        &query,
        json!({"node":target["id"],"content":content.github()}),
        true,
    )
    .await?;
    if result[operation]["subject"]["id"] != target["id"] {
        return Err(ForgeError::uncertain(
            "GitHub did not confirm the reaction target",
        ));
    }
    Ok(())
}
