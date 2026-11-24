//! Expanded tracker reads. Foreign scopes can be inspected using the project's host account.
use super::*;
use axum::extract::{Path, Query};
mod azure;
mod forgejo;
mod github;
mod gitlab;
mod linear;
mod source;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(in crate::workbench::forge) struct Target {
    tracker: Option<String>,
    host: Option<String>,
    scope: Option<String>,
    cursor: Option<String>,
}

async fn resolve(app: &Workbench, p: Uuid, q: &Target) -> Result<(Source, bool), ForgeError> {
    let project = app.project(p)?;
    let db = app.clone();
    let mut sources = tokio::task::spawn_blocking(move || {
        scopes(&db, &project, &super::super::super::Config::load()?)
    })
    .await
    .map_err(|e| ForgeError::bad(e.to_string()))??;
    sources.retain(|s| {
        capabilities(&s.ctx.kind).is_some()
            && q.tracker.as_ref().is_none_or(|t| t == &s.ctx.kind)
            && q.host
                .as_ref()
                .is_none_or(|h| s.ctx.host.eq_ignore_ascii_case(h))
    });
    let mut writable = true;
    let index = q.scope.as_ref().and_then(|scope| {
        sources
            .iter()
            .position(|s| s.scope.eq_ignore_ascii_case(scope))
    });
    if sources.is_empty() {
        return Err(ForgeError::remote(
            404,
            "project does not serve this issue tracker and host",
        ));
    }
    let mut source = sources.swap_remove(index.unwrap_or(0));
    if let Some(scope) = &q.scope
        && index.is_none()
    {
        if scope.is_empty()
            || scope.len() > 1024
            || scope.split('/').any(|p| {
                p.is_empty()
                    || matches!(p, "." | "..")
                    || p.chars().any(|ch| {
                        ch.is_control() || ch.is_whitespace() || matches!(ch, '\\' | '?' | '#')
                    })
            })
        {
            return Err(ForgeError::bad("invalid tracker scope"));
        }
        writable = false;
        source.ctx.repository = match source.ctx.kind.as_str() {
            "linear" => {
                if scope.contains('/') {
                    return Err(ForgeError::bad("expected a Linear team key"));
                }
                let team = scope.to_ascii_uppercase();
                source
                    .ctx
                    .repository
                    .split_once('/')
                    .map_or_else(|| team.clone(), |(org, _)| format!("{org}/{team}"))
            }
            "azure-devops" => super::super::azure::repository_for_issue_scope(&source.ctx, scope)?,
            _ => scope.clone(),
        };
        source.scope = scope.clone();
    }
    super::super::super::signin::attach(app, &mut source.ctx).await?;
    if let Some(token) = super::super::credentials::resolve(&source.ctx).await? {
        source.ctx.token = Some(token);
    }
    Ok((source, writable))
}
pub(in crate::workbench::forge) async fn read(
    State(app): State<Workbench>,
    Path((p, n)): Path<(Uuid, u64)>,
    Query(q): Query<Target>,
) -> super::super::super::Reply<Value> {
    if n == 0 || q.cursor.is_some() {
        return Err(ForgeError::bad("invalid issue detail number or cursor"));
    }
    let (s, writable) = resolve(&app, p, &q).await?;
    let mut result = match s.ctx.kind.as_str() {
        "azure-devops" => azure::read(&s.ctx, &s.scope, n).await?,
        "gitlab" => gitlab::read(&s.ctx, n).await?,
        "github" => github::read(&s.ctx, n).await?,
        "forgejo" => forgejo::read(&s.ctx, n, None, false).await?,
        "linear" => linear::read(&s.ctx, &s.scope.to_ascii_uppercase(), n).await?,
        "tanstack-source" => source::read(&s.ctx, n, None, false).await?,
        _ => {
            return Err(ForgeError::unsupported(
                "expanded issue details are unavailable for this tracker",
            ));
        }
    };
    result["projectId"] = json!(p);
    result["projectTitle"] = json!(s.title);
    result["scope"] = json!(s.scope);
    result["tracker"] = json!(s.ctx.kind);
    result["host"] = json!(s.ctx.host);
    result["writable"] = json!(writable);
    result["capabilities"] = capabilities(&s.ctx.kind).unwrap();
    Ok(Json(result))
}
pub(in crate::workbench::forge) async fn comments(
    State(app): State<Workbench>,
    Path((p, n)): Path<(Uuid, u64)>,
    Query(q): Query<Target>,
) -> super::super::super::Reply<Value> {
    if n == 0
        || q.cursor
            .as_deref()
            .is_some_and(|s| s.is_empty() || s.len() > 4096)
    {
        return Err(ForgeError::bad("invalid issue comment page"));
    }
    let (s, _) = resolve(&app, p, &q).await?;
    match s.ctx.kind.as_str() {
        "azure-devops" => azure::comments(&s.ctx, &s.scope, n, q.cursor.as_deref())
            .await
            .map(Json),
        "gitlab" => gitlab::comments(&s.ctx, n, q.cursor.as_deref())
            .await
            .map(Json),
        "forgejo" => forgejo::read(&s.ctx, n, q.cursor.as_deref(), true)
            .await
            .map(Json),
        "linear" => linear::comments(
            &s.ctx,
            &s.scope.to_ascii_uppercase(),
            n,
            q.cursor.as_deref(),
        )
        .await
        .map(Json),
        "tanstack-source" => source::read(&s.ctx, n, q.cursor.as_deref(), true)
            .await
            .map(Json),
        "github" => github::comments(&s.ctx, n, q.cursor.as_deref())
            .await
            .map(Json),
        _ => Err(ForgeError::unsupported(
            "expanded issue comments are unavailable for this tracker",
        )),
    }
}
