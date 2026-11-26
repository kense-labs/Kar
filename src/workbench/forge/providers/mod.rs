//! REST and native CLI drivers for non-GitHub hosts in the fixed workbench contract.
pub(super) mod azure;
pub(in crate::workbench::forge) mod create_repo;
pub(super) mod credentials;
pub(in crate::workbench::forge) mod diff_contents;
pub(in crate::workbench::forge) mod file_stats;
pub(super) mod hosted;
pub(super) mod issue;
pub(super) mod issue_directory;
pub(in crate::workbench::forge) mod issue_listing;
pub(super) mod linear;
pub(in crate::workbench::forge) mod pull_tools;
pub(in crate::workbench::forge) mod reactions;
pub(super) mod rest;
pub(super) mod reviews;
pub(in crate::workbench::forge) mod stack;
pub(in crate::workbench::forge) mod viewed;
use super::{Context, ForgeError, Operation};
use reqwest::Method;
use serde_json::{Value, json};
use url::Url;

fn invalid(what: &str) -> ForgeError {
    ForgeError::remote(502, format!("forge returned invalid {what}"))
}
fn number(v: &Value) -> Result<u64, ForgeError> {
    v.as_u64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| invalid("object number"))
}
fn verify(v: Value, n: u64) -> Result<Value, ForgeError> {
    if number(&v["number"])? == n {
        Ok(v)
    } else {
        Err(invalid("object identity"))
    }
}
pub(super) fn path(c: &Context, parts: &[&str]) -> Result<Url, ForgeError> {
    let mut url = c.api_base.clone();
    url.path_segments_mut()
        .map_err(|_| ForgeError::bad("invalid forge API URL"))?
        .pop_if_empty()
        .extend(parts);
    Ok(url)
}
async fn send(
    c: &Context,
    method: Method,
    url: Url,
    body: Option<Value>,
    mutation: bool,
) -> Result<Value, ForgeError> {
    send_with_headers(c, method, url, body, mutation)
        .await
        .map(|(body, _)| body)
}
async fn send_with_headers(
    c: &Context,
    method: Method,
    url: Url,
    body: Option<Value>,
    mutation: bool,
) -> Result<(Value, reqwest::header::HeaderMap), ForgeError> {
    let res = response(c, method, url, body, mutation, None).await?;
    let (bytes, headers) = read_bytes(res, mutation, 16 * 1024 * 1024).await?;
    if bytes.is_empty() {
        return Ok((Value::Null, headers));
    }
    serde_json::from_slice(&bytes)
        .map(|value| (value, headers))
        .map_err(|_| response_error(mutation, "forge returned invalid JSON"))
}
fn response_error(mutation: bool, reason: &str) -> ForgeError {
    if mutation {
        ForgeError::uncertain(reason)
    } else {
        ForgeError::remote(502, reason)
    }
}
async fn response(
    c: &Context,
    method: Method,
    url: Url,
    body: Option<Value>,
    mutation: bool,
    accept: Option<&str>,
) -> Result<reqwest::Response, ForgeError> {
    let azure_patch = c.kind == "azure-devops"
        && matches!(method, Method::PATCH | Method::POST)
        && body.as_ref().is_some_and(Value::is_array);
    let mut req = c.client.request(method, url);
    if azure_patch {
        req = req.header("Content-Type", "application/json-patch+json");
    }
    if let Some(token) = &c.token {
        req = match c.kind.as_str() {
            "bitbucket" if std::env::var_os("KAR_BITBUCKET_EMAIL").is_some() => {
                let email = std::env::var("KAR_BITBUCKET_EMAIL")
                    .map_err(|_| ForgeError::bad("invalid Bitbucket account email"))?;
                req.basic_auth(email, Some(token))
            }
            "linear" => req.header("Authorization", token),
            "azure-devops" => req.basic_auth("", Some(token)),
            _ => req.bearer_auth(token),
        };
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    if let Some(accept) = accept {
        req = req.header("Accept", accept);
    }
    let res = if c.kind == "bitbucket" && accept == Some("text/x-diff") {
        // Bitbucket Cloud redirects its PR diff to the repository diff for the same revisions.
        let diff_prefix = format!("{}/diff/", rest::project(c)?.path().trim_end_matches('/'));
        let mut request = req.build().map_err(|_| invalid("Bitbucket diff request"))?;
        let mut count = 0;
        loop {
            let outgoing = request
                .try_clone()
                .ok_or_else(|| invalid("Bitbucket diff request"))?;
            let result = c
                .client
                .execute(outgoing)
                .await
                .map_err(|_| response_error(false, "forge did not return a response"))?;
            if !result.status().is_redirection() {
                break result;
            }
            let next = result
                .headers()
                .get("location")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| request.url().join(s).ok())
                .filter(|u| {
                    u.origin() == c.api_base.origin()
                        && u.path().starts_with(&diff_prefix)
                        && u.username().is_empty()
                        && u.password().is_none()
                })
                .ok_or_else(|| invalid("Bitbucket diff redirect"))?;
            if count == 3 {
                return Err(invalid("repeated Bitbucket diff redirect"));
            }
            count += 1;
            *request.url_mut() = next;
        }
    } else {
        req.send()
            .await
            .map_err(|_| response_error(mutation, "forge did not return a response"))?
    };
    let status = res.status();
    if status.as_u16() == 401 {
        super::signin::reject(c);
    }
    if !status.is_success() {
        if c.kind == "github"
            && status.as_u16() == 403
            && (res.headers().contains_key("retry-after")
                || res
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0"))
        {
            return Err(ForgeError::remote(429, "GitHub rate limit reached"));
        }
        if mutation && status.is_server_error() {
            return Err(ForgeError::uncertain(format!(
                "{} returned {status} after submission",
                c.kind
            )));
        }
        return Err(ForgeError::remote(
            if status.is_redirection() {
                502
            } else {
                status.as_u16()
            },
            format!("{} returned {status}", c.kind),
        ));
    }
    Ok(res)
}
async fn read_bytes(
    mut res: reqwest::Response,
    mutation: bool,
    limit: usize,
) -> Result<(Vec<u8>, reqwest::header::HeaderMap), ForgeError> {
    let headers = res.headers().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|_| response_error(mutation, "forge response interrupted"))?
    {
        if bytes.len() + chunk.len() > limit {
            return Err(response_error(
                mutation,
                "forge response exceeded its byte limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, headers))
}
pub(super) async fn get(c: &Context, url: Url) -> Result<Value, ForgeError> {
    send(c, Method::GET, url, None, false).await
}
fn account(v: &Value, field: &str) -> Result<Value, ForgeError> {
    v[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| json!(s))
        .ok_or_else(|| invalid("authenticated account"))
}
pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let alternate;
    let c = if let Some(token) = credentials::resolve(c).await? {
        alternate = Context {
            token: Some(token),
            ..c.clone()
        };
        &alternate
    } else {
        c
    };
    match c.kind.as_str() {
        "gitcafe" | "cursor-origin" | "tanstack-source" => hosted::execute(c, op).await,
        "azure-devops" => azure::execute(c, op).await,
        "linear" => linear::execute(c, op).await,
        "gitlab" | "bitbucket" | "forgejo" => rest::execute(c, op).await,
        _ => Err(ForgeError::unsupported(format!(
            "{} driver is unavailable",
            c.kind
        ))),
    }
}

pub(super) async fn gitlab_project_matches(
    c: &Context,
    path_name: &str,
    id: u64,
) -> Result<bool, ForgeError> {
    let mut target = c.clone();
    if let Some(token) = credentials::resolve(c).await? {
        target.token = Some(token);
    }
    let project = get(&target, path(&target, &["projects", path_name])?).await?;
    Ok(number(&project["id"])? == id)
}
