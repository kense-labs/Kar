//! Linear tracks issues in a workspace/team independently of the Git remote.
use super::*;
use crate::workbench::Workbench;
use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
pub(in crate::workbench::forge) struct Team {
    project: Uuid,
    team: String,
    workspace: Option<String>,
    api_base: Option<String>,
    token_env: Option<String>,
}
pub(in crate::workbench::forge) async fn tracker(
    app: Workbench,
    project: Uuid,
) -> Result<Context, ForgeError> {
    let next = app.clone();
    let bound = tokio::task::spawn_blocking(move || -> Result<_, ForgeError> {
        let p = next.project(project)?;
        next.workspace(&p)?;
        let mut trackers = super::super::Config::load()?
            .issue_trackers
            .into_iter()
            .filter(|t| t.project == project);
        let Some(team) = trackers.next() else {
            return Ok(None);
        };
        if trackers.next().is_some() || team.team.is_empty() || team.team.contains('/') {
            return Err(ForgeError::bad(
                "ambiguous or invalid Linear team configuration",
            ));
        }
        let url = Context::base(
            team.api_base
                .as_deref()
                .unwrap_or("https://api.linear.app/graphql"),
        )?;
        let key = std::env::var(team.token_env.as_deref().unwrap_or("LINEAR_API_KEY"))
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ForgeError::remote(401, "Linear API key is unavailable"))?;
        let context = Context {
            kind: "linear".into(),
            host: url.authority().to_owned(),
            repository: team.team,
            api_base: url,
            token: Some(key),
            client: Context::client()?,
            cwd: p.root,
        };
        Ok(Some((context, team.workspace)))
    })
    .await
    .map_err(|e| ForgeError::bad(e.to_string()))??;
    let Some((mut c, workspace)) = bound else {
        return Context::open(app, project).await;
    };
    let data = scope(&c, &c.repository).await?;
    let org = data["organization"]["urlKey"]
        .as_str()
        .filter(|s| !s.is_empty() && !s.contains('/'))
        .ok_or_else(|| invalid("Linear workspace"))?;
    if workspace.as_deref().is_some_and(|v| v != org) {
        return Err(ForgeError::remote(
            403,
            "Linear key belongs to another workspace",
        ));
    }
    c.repository = format!("{org}/{}", c.repository);
    Ok(c)
}
async fn gql(
    c: &Context,
    query: &str,
    variables: Value,
    mutation: bool,
) -> Result<Value, ForgeError> {
    let reply = send(
        c,
        Method::POST,
        c.api_base.clone(),
        Some(json!({"query":query,"variables":variables})),
        mutation,
    )
    .await?;
    if let Some(errors) = reply["errors"].as_array().filter(|xs| !xs.is_empty()) {
        let codes: Vec<_> = errors
            .iter()
            .filter_map(|e| e["extensions"]["code"].as_str())
            .collect();
        let status = if codes
            .iter()
            .any(|c| c.contains("RATE_LIMIT") || c.contains("RATELIMIT"))
        {
            429
        } else if codes.contains(&"UNAUTHENTICATED") {
            401
        } else if codes.contains(&"FORBIDDEN") {
            403
        } else if codes.contains(&"ENTITY_NOT_FOUND") {
            404
        } else if codes.contains(&"BAD_USER_INPUT") {
            400
        } else if mutation {
            return Err(ForgeError::uncertain("Linear did not confirm the mutation"));
        } else {
            502
        };
        return Err(ForgeError::remote(
            status,
            "Linear rejected the GraphQL operation",
        ));
    }
    reply
        .get("data")
        .filter(|d| d.is_object())
        .cloned()
        .ok_or_else(|| {
            if mutation {
                ForgeError::uncertain("Linear mutation data missing")
            } else {
                invalid("Linear GraphQL data")
            }
        })
}
const SCOPE: &str = "query KarScope($team:String!) {viewer {id name email} organization {urlKey} teams(first:1,filter:{key:{eq:$team}}){nodes{id key name states(first:250){nodes{id name type}}}}}";
async fn scope(c: &Context, key: &str) -> Result<Value, ForgeError> {
    let value = gql(c, SCOPE, json!({"team":key}), false).await?;
    if value.pointer("/teams/nodes/0/key").and_then(Value::as_str) != Some(key) {
        return Err(ForgeError::remote(
            404,
            "Linear team not found in this workspace",
        ));
    }
    Ok(value)
}
const FIELDS: &str =
    "id number identifier title description url updatedAt state{id name type} team{id key}";
fn issue(row: &Value, key: &str) -> Result<Value, ForgeError> {
    if row.pointer("/team/key").and_then(Value::as_str) != Some(key) {
        return Err(invalid("Linear issue team"));
    }
    let n = number(&row["number"])?;
    Ok(
        json!({"number":n,"identifier":row["identifier"],"native_id":row["id"],"title":row["title"],"body":row["description"],"url":row["url"],"updated_at":row["updatedAt"],"state":row["state"]["name"],"category":if matches!(row["state"]["type"].as_str(),Some("completed"|"canceled")){"closed"}else{"open"}}),
    )
}
async fn detail(c: &Context, key: &str, n: u64) -> Result<Value, ForgeError> {
    let q = format!("query KarIssue($id:String!){{issue(id:$id){{{FIELDS}}}}}");
    let data = gql(c, &q, json!({"id":format!("{key}-{n}")}), false).await?;
    if data["issue"].is_null() {
        return Err(ForgeError::remote(404, "unknown Linear issue"));
    }
    verify(issue(&data["issue"], key)?, n)
}
pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let (org, key) = c
        .repository
        .split_once('/')
        .ok_or_else(|| ForgeError::bad("Linear needs a workspace/team binding"))?;
    match op {
        Operation::Identity => {
            let s = scope(c, key).await?;
            if s["organization"]["urlKey"] != org {
                return Err(ForgeError::remote(403, "Linear workspace identity changed"));
            }
            Ok(
                json!({"host":c.host,"repository":c.repository,"team":s.pointer("/teams/nodes/0"),"account":s["viewer"]["email"],"permissions":null}),
            )
        }
        Operation::Pulls | Operation::Pull { .. } | Operation::CreatePull { .. } => Err(
            ForgeError::unsupported("Linear registers an issue tracker only"),
        ),
        Operation::Issues => {
            let q = format!(
                "query KarIssues($team:String!,$after:String){{issues(first:100,after:$after,filter:{{team:{{key:{{eq:$team}}}},state:{{type:{{nin:[\"completed\",\"canceled\"]}}}}}}){{nodes{{{FIELDS}}} pageInfo{{hasNextPage endCursor}}}}}}"
            );
            let mut cursor = Value::Null;
            let mut result = vec![];
            let mut seen = std::collections::HashSet::new();
            loop {
                let data = gql(c, &q, json!({"team":key,"after":cursor}), false).await?;
                let page = &data["issues"];
                for row in page["nodes"]
                    .as_array()
                    .ok_or_else(|| invalid("Linear listing"))?
                {
                    result.push(issue(row, key)?);
                }
                if page["pageInfo"]["hasNextPage"] == false {
                    break;
                }
                let next = page["pageInfo"]["endCursor"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("Linear pagination"))?;
                if !seen.insert(next.to_owned()) {
                    return Err(invalid("repeated Linear cursor"));
                }
                cursor = json!(next);
            }
            Ok(json!(result))
        }
        Operation::Issue { number: n } => detail(c, key, *n).await,
        Operation::SetIssueState { number: n, state } => {
            let current = detail(c, key, *n).await?;
            let data = scope(c, key).await?;
            let target = data
                .pointer("/teams/nodes/0/states/nodes")
                .and_then(Value::as_array)
                .and_then(|states| states.iter().find(|s| s["name"] == *state))
                .and_then(|s| s["id"].as_str())
                .ok_or_else(|| ForgeError::bad("unknown state name for the Linear team"))?;
            let q = format!(
                "mutation KarUpdate($id:String!,$input:IssueUpdateInput!){{issueUpdate(id:$id,input:$input){{success issue{{{FIELDS}}}}}}}"
            );
            let data = gql(
                c,
                &q,
                json!({"id":current["native_id"],"input":{"stateId":target}}),
                true,
            )
            .await?;
            let changed = &data["issueUpdate"]["issue"];
            if data["issueUpdate"]["success"] != true
                || changed["id"] != current["native_id"]
                || changed["state"]["id"] != target
            {
                return Err(ForgeError::uncertain(
                    "Linear did not confirm the requested state on the issue",
                ));
            }
            issue(changed, key)
                .map_err(|_| ForgeError::uncertain("Linear mutation returned an unexpected issue"))
        }
    }
}
