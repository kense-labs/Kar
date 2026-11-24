use super::issue::child;
use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use uuid::Uuid;

#[derive(serde::Deserialize)]
pub(in crate::workbench::forge) struct Options {
    include_details: Option<bool>,
}
pub(in crate::workbench::forge) async fn stack(
    State(app): State<crate::workbench::Workbench>,
    Path((project, n)): Path<(Uuid, u64)>,
    Query(opts): Query<Options>,
) -> Result<Json<Value>, ForgeError> {
    let mut c = Context::open(app, project).await?;
    if n == 0 {
        return Err(ForgeError::bad("pull number must be positive"));
    }
    if !matches!(c.kind.as_str(), "github" | "gitcafe" | "tanstack-source") {
        return Ok(Json(Value::Null));
    }
    if let Some(token) = credentials::resolve(&c).await? {
        c.token = Some(token);
    }
    let data = match c.kind.as_str() {
        "github" => match github_stack(&c, n, opts.include_details.unwrap_or(true)).await {
            Err(e) if e.status == 404 => Value::Null,
            other => other?,
        },
        "gitcafe" => cafe(&c, n).await?,
        _ => source(&c, n).await?,
    };
    Ok(Json(data))
}
fn string<'a>(v: &'a Value, field: &str) -> Result<&'a str, ForgeError> {
    v[field].as_str().ok_or_else(|| invalid(field))
}
fn array<'a>(v: &'a Value, name: &str) -> Result<&'a [Value], ForgeError> {
    v.as_array().map(Vec::as_slice).ok_or_else(|| invalid(name))
}
fn optional(src: &Value, key: &str, v: &mut Value, out: &str) {
    if !src[key].is_null() {
        v[out] = src[key].clone();
    }
}

async fn github_stack(c: &Context, n: u64, details: bool) -> Result<Value, ForgeError> {
    let (o, r) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("repository"))?;
    let mut url = path(c, &["repos", o, r, "stacks"])?;
    url.query_pairs_mut()
        .append_pair("pull_request", &n.to_string());
    let list = get(c, url).await?;
    let Some(summary) = array(&list, "stack list")?.first() else {
        return Ok(Value::Null);
    };
    let sn = number(&summary["number"])?;
    let full;
    let v = if details {
        full = get(c, path(c, &["repos", o, r, "stacks", &sn.to_string()])?).await?;
        if number(&full["number"])? != sn {
            return Err(invalid("stack detail identity"));
        }
        &full
    } else {
        summary
    };
    let id = match &v["id"] {
        Value::Number(num) => num.to_string(),
        Value::String(s) => s.clone(),
        _ => v["node_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| sn.to_string()),
    };
    let base = v["base"]
        .as_str()
        .or(v["base"]["ref"].as_str())
        .ok_or_else(|| invalid("GitHub stack base"))?;
    let url = v["html_url"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(v["url"].as_str())
        .ok_or_else(|| invalid("GitHub stack URL"))?;
    let mut layers = Vec::new();
    for pr in array(&v["pull_requests"], "GitHub stack layers")? {
        let state = if pr["merged_at"].is_string()
            || pr["state"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("merged"))
        {
            "merged"
        } else if pr["state"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("closed"))
        {
            "closed"
        } else {
            "open"
        };
        let mut layer = json!({"number":number(&pr["number"])?,"headBranch":string(&pr["head"],"ref")?,"state":state});
        optional(pr, "title", &mut layer, "title");
        optional(pr, "draft", &mut layer, "isDraft");
        optional(&pr["head"], "sha", &mut layer, "headSha");
        layers.push(layer);
    }
    Ok(json!({"id":id,"number":sn,"url":url,"base":base,"layers":layers}))
}
async fn cafe(c: &Context, n: u64) -> Result<Value, ForgeError> {
    let v = get(
        c,
        child(hosted::resource(c, Some("pulls"), Some(n))?, &["stack"]),
    )
    .await?;
    let stack = v
        .get("stack")
        .ok_or_else(|| invalid("GitCafe stack envelope"))?;
    if stack.is_null() {
        return Ok(Value::Null);
    }
    let sn = number(&stack["number"])?;
    let mut sorted = Vec::new();
    for item in array(&stack["members"], "GitCafe stack members")? {
        sorted.push((number(&item["position"])?, item));
    }
    sorted.sort_by_key(|(i, _)| *i);
    let mut layers = Vec::new();
    for (_, m) in sorted {
        let number = number(&m["pullRequestNumber"])?;
        let state = string(m, "state")?;
        if !matches!(state, "open" | "merged" | "closed") {
            return Err(invalid("GitCafe layer state"));
        }
        let mut item = json!({"number":number,"headBranch":string(m,"sourceBranch")?,"state":state,"title":string(m,"title")?,"isDraft":m["draft"].as_bool().ok_or_else(||invalid("GitCafe draft"))?});
        optional(m, "headOid", &mut item, "headSha");
        if state != "merged" {
            let detail = get(c, hosted::resource(c, Some("pulls"), Some(number))?).await?;
            if super::number(&detail["number"])? != number {
                return Err(invalid("GitCafe stack member identity"));
            }
            optional(&detail, "headOid", &mut item, "headSha");
        }
        layers.push(item);
    }
    Ok(
        json!({"number":sn,"id":string(stack,"id")?,"url":format!("https://{}/{}/stacks/{sn}",c.host,c.repository),"base":string(stack,"landingBase")?,"layers":layers}),
    )
}
async fn source(c: &Context, n: u64) -> Result<Value, ForgeError> {
    let stack = get(
        c,
        child(hosted::resource(c, Some("pulls"), Some(n))?, &["stack"]),
    )
    .await?;
    let members = array(&stack["members"], "Source stack members")?;
    if members.len() < 2 {
        return Ok(Value::Null);
    }
    let mut layers = Vec::new();
    for m in members {
        let head = string(m, "headRef")?;
        let head = head.strip_prefix("refs/heads/").unwrap_or(head);
        let head = head
            .split_once(':')
            .filter(|(owner, _)| !owner.is_empty() && !owner.contains('/'))
            .map_or(head, |(_, branch)| branch);
        layers.push(json!({"number":number(&m["number"])?,"title":string(m,"title")?,"isDraft":m["draft"].as_bool().ok_or_else(||invalid("Source stack draft"))?,"state":"open","headBranch":head}));
    }
    let bottom = &members[0];
    let n = number(&bottom["number"])?;
    Ok(
        json!({"id":format!("{}#{n}",c.repository),"number":n,"base":string(bottom,"baseRef")?,"url":format!("https://{}/@{}/pull/{n}",c.host,c.repository.trim_start_matches('@')),"layers":layers}),
    )
}
