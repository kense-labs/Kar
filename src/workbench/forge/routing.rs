//! Pin the host and user on the same captured credential used to contact the remote.
use super::*;
use axum::{
    Router,
    extract::{Path, Query, State},
    routing::{get, put},
};
use reqwest::Method;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Expected {
    pub(super) host: String,
    pub(super) account_id: String,
}

pub(super) fn headers(headers: &axum::http::HeaderMap) -> Result<Option<Expected>, ForgeError> {
    match (
        headers.get("x-kar-forge-host"),
        headers.get("x-kar-forge-account-id"),
    ) {
        (None, None) => Ok(None),
        (Some(host), Some(id)) => Ok(Some(Expected {
            host: host
                .to_str()
                .map_err(|_| ForgeError::bad("invalid forge host header"))?
                .into(),
            account_id: id
                .to_str()
                .map_err(|_| ForgeError::bad("invalid forge account header"))?
                .into(),
        })),
        _ => Err(ForgeError::bad(
            "the expected forge host and account ID must be supplied together",
        )),
    }
}
async fn identity(c: &Context) -> Result<Value, ForgeError> {
    if c.kind != "github" {
        return Err(ForgeError::unsupported(
            "this driver does not implement verified account routing",
        ));
    }
    let bearer = c
        .token
        .as_ref()
        .ok_or_else(|| ForgeError::remote(401, "a verified GitHub credential is required"))?;
    let rest = github::request(c, Method::GET, &["user"], &[], None).await;
    let (id, viewer) = match rest {
        Ok(r) => (r["id"].as_u64(), r["login"].as_str().map(str::to_owned)),
        Err(first) => {
            let mut u = c.api_base.clone();
            if u.path().trim_end_matches('/').ends_with("/api/v3") {
                u.path_segments_mut().unwrap().pop_if_empty().pop();
            }
            u.path_segments_mut()
                .unwrap()
                .pop_if_empty()
                .push("graphql");
            let response = c
                .client
                .post(u)
                .bearer_auth(bearer)
                .json(&json!({"query":"query KarRoutingIdentity { viewer { databaseId login } }"}))
                .send()
                .await
                .ok()
                .filter(|r| r.status().is_success())
                .ok_or(first.clone())?;
            let v: Value = response.json().await.map_err(|_| first)?;
            (
                v["data"]["viewer"]["databaseId"].as_u64(),
                v["data"]["viewer"]["login"].as_str().map(str::to_owned),
            )
        }
    };
    let account = id
        .filter(|n| *n > 0)
        .ok_or_else(|| ForgeError::remote(403, "GitHub omitted its authenticated user ID"))?;
    let viewer = viewer
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ForgeError::remote(403, "GitHub omitted its authenticated login"))?;
    Ok(json!({"host":c.host,"provider":c.kind,"accountId":account.to_string(),"viewer":viewer}))
}
pub(super) async fn verify(c: &Context, expected: Option<&Expected>) -> Result<(), ForgeError> {
    if let Some(e) = expected {
        if !e.host.eq_ignore_ascii_case(&c.host) || e.account_id.trim().is_empty() {
            return Err(ForgeError::remote(
                403,
                "requested account route does not match the project host",
            ));
        }
        if identity(c).await?["accountId"] != e.account_id {
            return Err(ForgeError::remote(
                403,
                "authenticated account does not match the requested account ID",
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct HostQuery {
    host: String,
}
async fn host(State(app): State<Workbench>, Query(q): Query<HostQuery>) -> Reply<Value> {
    for project in app.projects() {
        let Ok(c) = Context::open(app.clone(), project.id).await else {
            continue;
        };
        if c.kind == "github" && c.host.eq_ignore_ascii_case(&q.host) {
            return identity(&c).await.map(Json);
        }
    }
    Err(ForgeError::unsupported(
        "no registered project offers this host's account route",
    ))
}
async fn project(State(app): State<Workbench>, Path(project): Path<Uuid>) -> Reply<Value> {
    let c = Context::open(app.clone(), project).await?;
    let mut info = identity(&c).await?;
    info["projectTitle"] = app.project(project)?.name.into();
    info["workspaceRoot"] = json!(c.cwd);
    Ok(Json(info))
}

async fn star(c: &Context, write: bool) -> bool {
    if c.kind != "github" {
        return false;
    }
    let parts: Vec<_> = c.repository.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|s| matches!(*s, "" | "." | "..")) {
        return false;
    }
    let mut url = c.api_base.clone();
    url.path_segments_mut()
        .unwrap()
        .pop_if_empty()
        .extend(["user", "starred", parts[0], parts[1]]);
    let mut req = c
        .client
        .request(if write { Method::PUT } else { Method::GET }, url);
    if let Some(t) = &c.token {
        req = req.bearer_auth(t);
    }
    if write {
        req = req.header("content-length", "0");
    }
    req.send()
        .await
        .is_ok_and(|r| r.status() == StatusCode::NO_CONTENT)
}
async fn starred(State(app): State<Workbench>, Path(project): Path<Uuid>) -> Reply<bool> {
    let c = Context::open(app, project).await?;
    Ok(Json(star(&c, false).await))
}
async fn add_star(State(app): State<Workbench>, Path(project): Path<Uuid>) -> Reply<bool> {
    let c = Context::open(app, project).await?;
    Ok(Json(star(&c, true).await))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/forge/routing-identity", get(host))
        .route("/projects/{project}/forge/routing", get(project))
        .route(
            "/projects/{project}/forge/starred",
            get(starred).merge(put(add_star)),
        )
}
