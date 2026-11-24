use super::super::reviews::github::gql;
use super::*;
use std::collections::BTreeSet;

const PAGE: &str = "query KarViewedPage($owner:String!,$name:String!,$n:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$n){files(first:100,after:$after){nodes{path viewerViewedState} pageInfo{hasNextPage endCursor}}}}}";
fn vars(c: &Context, n: u64) -> Result<Value, ForgeError> {
    let (owner, name) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("GitHub repository"))?;
    Ok(json!({"owner":owner,"name":name,"n":n}))
}
pub(super) async fn read(c: &Context, n: u64) -> Result<Value, ForgeError> {
    let mut input = vars(c, n)?;
    let mut files = Vec::new();
    let mut cursors = BTreeSet::new();
    input["after"] = Value::Null;
    for _ in 0..5 {
        let result = gql(c, PAGE, input.clone(), false).await?;
        let page = &result["repository"]["pullRequest"]["files"];
        if page.is_null() {
            return Ok(json!({"files":files,"truncated":false}));
        }
        let nodes = page["nodes"]
            .as_array()
            .ok_or_else(|| invalid("GitHub viewed file page"))?;
        for r in nodes {
            let Some(path) = r["path"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let state = match r["viewerViewedState"].as_str() {
                Some("VIEWED") => "viewed",
                Some("DISMISSED") => "dismissed",
                _ => "unviewed",
            };
            files.push(json!({"path":path,"state":state}));
        }
        if page["pageInfo"]["hasNextPage"] != true {
            return Ok(json!({"files":files,"truncated":false}));
        }
        let cursor = page["pageInfo"]["endCursor"]
            .as_str()
            .filter(|s| !s.is_empty() && cursors.insert(s.to_string()))
            .ok_or_else(|| invalid("GitHub viewed file cursor"))?;
        input["after"] = json!(cursor);
    }
    Ok(json!({"files":files,"truncated":true}))
}
pub(super) async fn set(c: &Context, n: u64, files: &[Press]) -> Result<(), ForgeError> {
    if files.is_empty() {
        return Ok(());
    }
    let data=gql(c,"query KarViewedPull($owner:String!,$name:String!,$n:Int!){repository(owner:$owner,name:$name){pullRequest(number:$n){id}}}",vars(c,n)?,false).await?;
    let id = data["repository"]["pullRequest"]["id"]
        .as_str()
        .ok_or_else(|| ForgeError::remote(404, "unknown GitHub pull request"))?;
    for group in files.chunks(100) {
        let mut variables = json!({"id":id});
        let mut args = String::from("$id:ID!");
        let mut fields = String::new();
        for (i, f) in group.iter().enumerate() {
            let k = format!("p{i}");
            args.push_str(&format!(",$p{i}:String!"));
            variables[k] = json!(f.path);
            let op = if f.viewed {
                "markFileAsViewed"
            } else {
                "unmarkFileAsViewed"
            };
            fields.push_str(&format!(
                "f{i}:{op}(input:{{pullRequestId:$id,path:$p{i}}}){{clientMutationId}} "
            ));
        }
        let query = format!("mutation KarSetViewed({args}){{{fields}}}");
        let answer = gql(c, &query, variables, true).await?;
        if (0..group.len()).any(|i| !answer[format!("f{i}")].is_object()) {
            return Err(ForgeError::uncertain(
                "GitHub did not confirm every viewed file press",
            ));
        }
    }
    Ok(())
}
