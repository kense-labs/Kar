use super::issue::child;
use super::*;
use crate::workbench::Workbench;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get as route_get,
};
use uuid::Uuid;
mod templates;

fn rows<'a>(v: &'a Value, name: &str) -> Result<&'a [Value], ForgeError> {
    v.as_array().map(Vec::as_slice).ok_or_else(|| invalid(name))
}
fn label(v: &Value) -> Option<Value> {
    Some(
        json!({"name":v["name"].as_str().filter(|s|!s.trim().is_empty())?,"color":v["color"],"description":v["description"]}),
    )
}
fn member(v: &Value, login: &str, identifier: &str, name: &str, avatar: &str) -> Option<Value> {
    let id = v[identifier]
        .as_str()
        .map(str::to_string)
        .or_else(|| v[identifier].as_u64().map(|n| n.to_string()))?;
    Some(
        json!({"id":id,"login":v[login].as_str().filter(|s|!s.is_empty())?,"name":v[name],"avatarUrl":v[avatar]}),
    )
}
async fn context(app: Workbench, p: Uuid) -> Result<Context, ForgeError> {
    let mut c = linear::tracker(app, p).await?;
    if let Some(t) = credentials::resolve(&c).await? {
        c.token = Some(t);
    }
    Ok(c)
}
pub(in crate::workbench::forge) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/forge/issue-tracker/candidates",
            route_get(candidates),
        )
        .route(
            "/projects/{project}/forge/issue-tracker/templates",
            route_get(template_list),
        )
}
async fn candidates(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
) -> Result<Json<Value>, ForgeError> {
    directory(&context(app, p).await?, false).await.map(Json)
}
async fn template_list(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
) -> Result<Json<Value>, ForgeError> {
    directory(&context(app, p).await?, true).await.map(Json)
}

async fn directory(c: &Context, want_templates: bool) -> Result<Value, ForgeError> {
    match c.kind.as_str() {
        "github" => {
            let (owner, name) = c
                .repository
                .split_once('/')
                .ok_or_else(|| invalid("repository"))?;
            let selections = if want_templates {
                r#"templateDirectory:object(expression:"HEAD:.github/ISSUE_TEMPLATE"){__typename ...on Tree {entries{name object{__typename ...on Blob {text isTruncated}}}}} issueTemplates{filename name about title body assignees(first:25){nodes{login}} labels(first:50){nodes{name}}}"#
            } else {
                "labels(first:100){nodes{name color description} pageInfo{hasNextPage}} assignableUsers(first:100){nodes{login name avatarUrl} pageInfo{hasNextPage}}"
            };
            let q = format!(
                "query KarIssueDirectory($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{selections}}}}}"
            );
            let v = reviews::github::gql(c, &q, json!({"owner":owner,"name":name}), false).await?;
            let repo = &v["repository"];
            if repo.is_null() {
                return Err(ForgeError::remote(404, "GitHub repository not found"));
            }
            if want_templates {
                return templates::github(repo);
            }
            let labels = repo["labels"]["nodes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(label)
                .collect::<Vec<_>>();
            let users = rows(&repo["assignableUsers"]["nodes"], "GitHub assignees")?
                .iter()
                .filter_map(|u| member(u, "login", "login", "name", "avatarUrl"))
                .collect::<Vec<_>>();
            Ok(
                json!({"labels":labels,"assignees":users,"truncated":repo["labels"]["pageInfo"]["hasNextPage"]==true||repo["assignableUsers"]["pageInfo"]["hasNextPage"]==true}),
            )
        }
        "forgejo" | "gitlab" => {
            let base = rest::project(c)?;
            if want_templates {
                return templates::rest(c, base).await;
            }
            let gl = c.kind == "gitlab";
            let limit = if gl { 100 } else { 50 };
            let page = |parts: &[&str]| {
                let mut u = child(base.clone(), parts);
                u.query_pairs_mut()
                    .append_pair("page", "1")
                    .append_pair(if gl { "per_page" } else { "limit" }, &limit.to_string());
                u
            };
            let (labels, assignees) = tokio::try_join!(
                send_with_headers(c, Method::GET, page(&["labels"]), None, false),
                send_with_headers(
                    c,
                    Method::GET,
                    page(if gl {
                        &["members", "all"]
                    } else {
                        &["assignees"]
                    }),
                    None,
                    false
                )
            )?;
            let more = |rows: &[Value], headers: &reqwest::header::HeaderMap| {
                if gl {
                    return rows.len() >= limit;
                }
                !rows.is_empty()
                    && headers
                        .get(reqwest::header::LINK)
                        .and_then(|h| h.to_str().ok())
                        .is_none_or(|links| {
                            links
                                .split(',')
                                .flat_map(|link| link.split(';').skip(1))
                                .any(|p| {
                                    let p = p.trim();
                                    p.eq_ignore_ascii_case("rel=next")
                                        || p.eq_ignore_ascii_case("rel=\"next\"")
                                })
                        })
            };
            let users = rows(&assignees.0, "assignees")?;
            let label_rows = rows(&labels.0, "labels")?;
            let truncated = more(label_rows, &labels.1) || more(users, &assignees.1);
            let labels = label_rows;
            let named = labels.iter().filter_map(label).collect::<Vec<_>>();
            let members = users
                .iter()
                .filter_map(|u| {
                    if gl {
                        member(u, "username", "id", "name", "avatar_url")
                    } else {
                        member(u, "login", "login", "full_name", "avatar_url")
                    }
                })
                .collect::<Vec<_>>();
            Ok(json!({"labels":named,"assignees":members,"truncated":truncated}))
        }
        "tanstack-source" if !want_templates => {
            let items = get(c, child(hosted::resource(c, None, None)?, &["labels"])).await?;
            let labels = rows(&items, "Source label list")?
                .iter()
                .filter_map(label)
                .collect::<Vec<_>>();
            let user = get(c, path(c, &["user"])?).await.ok();
            let users = user
                .and_then(|u| {
                    let name = u["username"].as_str()?.trim();
                    if name.is_empty() {
                        return None;
                    }
                    let mut url = Url::parse(&format!("https://{}/", c.host)).ok()?;
                    url.path_segments_mut().ok()?.extend(["avatars", name]);
                    let mut v =
                        json!({"id":name,"login":name,"name":null,"avatarUrl":url.as_str()});
                    if name.ends_with("[bot]") {
                        v["isBot"] = true.into();
                    }
                    Some(v)
                })
                .into_iter()
                .collect::<Vec<_>>();
            Ok(json!({"labels":labels,"assignees":users,"truncated":true}))
        }
        "azure-devops" if !want_templates => azure::candidates(c).await,
        "linear" => linear_directory(c, want_templates).await,
        _ => Err(ForgeError::unsupported(format!(
            "{} does not register this issue directory",
            c.kind
        ))),
    }
}
async fn linear_directory(c: &Context, templates_requested: bool) -> Result<Value, ForgeError> {
    let (_, key) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("Linear team"))?;
    const QUERY: &str = "query KarTeamDirectory($key:String!){teams(first:1,filter:{key:{eq:$key}}){nodes{key states(first:100){pageInfo{hasNextPage}} labels(first:250){nodes{id name color description} pageInfo{hasNextPage}} members(first:250){nodes{id name displayName avatarUrl} pageInfo{hasNextPage}} templates{nodes{id type name description content hasFormFields templateData}}}} issueLabels(first:250,filter:{team:{null:true}}){nodes{id name color description} pageInfo{hasNextPage}}}";
    let data = linear::gql(c, QUERY, json!({"key":key}), false).await?;
    let team = &data["teams"]["nodes"][0];
    if team["key"] != key {
        return Err(ForgeError::remote(404, "Linear team not found"));
    }
    let labels = rows(&team["labels"]["nodes"], "Linear labels")?
        .iter()
        .chain(rows(
            &data["issueLabels"]["nodes"],
            "Linear workspace labels",
        )?);
    if templates_requested {
        return templates::linear(team, key, labels.collect());
    }
    let members = rows(&team["members"]["nodes"], "Linear team members")?
        .iter()
        .filter_map(|u| {
            member(u, "displayName", "id", "name", "avatarUrl")
                .or_else(|| member(u, "id", "id", "name", "avatarUrl"))
        })
        .collect::<Vec<_>>();
    let truncated = ["states", "labels", "members"]
        .iter()
        .any(|k| team[k]["pageInfo"]["hasNextPage"] == true)
        || data["issueLabels"]["pageInfo"]["hasNextPage"] == true;
    Ok(
        json!({"labels":labels.filter_map(label).collect::<Vec<_>>(),"assignees":members,"truncated":truncated}),
    )
}
