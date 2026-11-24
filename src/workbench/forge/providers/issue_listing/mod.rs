//! One independently resumable native search page per registered issue scope.
use super::*;
use crate::workbench::{Project, Workbench};
use axum::Json;
use axum::extract::State;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
mod azure;
mod git_hosts;
mod standalone;

#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Involvement {
    #[default]
    All,
    Assigned,
    Authored,
    Mentioned,
}
#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Sort {
    #[default]
    Updated,
    Created,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::workbench::forge) struct Request {
    state: String,
    #[serde(default)]
    involvement: Involvement,
    #[serde(default)]
    sort: Sort,
    project_id: Option<Uuid>,
    #[serde(default)]
    exclude_project_ids: Vec<Uuid>,
    host: Option<String>,
    limit: Option<usize>,
    cursors: Option<BTreeMap<String, String>>,
    #[serde(rename = "refresh")]
    _refresh: Option<bool>,
    query: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    assignee: Option<String>,
}
impl Request {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(50)
    }
    fn validate(&self) -> Result<(), ForgeError> {
        fn text(s: &str, n: usize) -> bool {
            !s.is_empty()
                && s.trim() == s
                && s.encode_utf16().count() <= n
                && !s.chars().any(char::is_control)
        }
        if !matches!(self.state.as_str(), "all" | "open" | "closed")
            || !(1..=200).contains(&self.limit())
            || self.exclude_project_ids.len() > 1000
            || self.query.as_ref().is_some_and(|s| !text(s, 200))
            || self.labels.len() > 20
            || self.labels.iter().any(|s| !text(s, 200))
            || self.assignee.as_ref().is_some_and(|s| !text(s, 320))
            || self.host.as_ref().is_some_and(|s| !text(s, 1024))
            || self
                .cursors
                .as_ref()
                .is_some_and(|cs| cs.iter().any(|(k, v)| !text(k, 4096) || !text(v, 4096)))
        {
            return Err(ForgeError::bad("invalid issue list filters or cursor"));
        }
        Ok(())
    }
}
struct Source {
    project: Uuid,
    title: String,
    scope: String,
    account: Option<Uuid>,
    ctx: Context,
}
impl Source {
    fn account_key(&self) -> String {
        match self.account {
            None => self.ctx.host.clone(),
            Some(a) => format!("{} {a}", self.ctx.host),
        }
    }
    fn cursor_key(&self) -> String {
        let scope = self.scope.to_lowercase();
        match self.account {
            None => format!("{} {scope}", self.ctx.host),
            Some(a) => format!("{} {a}/{scope}", self.ctx.host),
        }
    }
}
pub(super) struct Page {
    items: Vec<Value>,
    next: Option<String>,
}
fn scopes(
    app: &Workbench,
    project: &Project,
    config: &super::super::Config,
) -> Result<Vec<Source>, ForgeError> {
    let mut results = Vec::new();
    for (mut ctx, account, workspace) in super::linear::listing_sources(app, project, config)? {
        let scope = ctx.repository.clone();
        if let Some(workspace) = workspace {
            ctx.repository = format!("{workspace}/{scope}");
        }
        results.push(Source {
            project: project.id,
            title: project.name.clone(),
            scope,
            account,
            ctx,
        });
    }
    if let Ok(repo) = app.repository(project.id) {
        let raw = crate::workbench_git::git(repo.root(), &["remote"])?;
        let remote_names = std::str::from_utf8(&raw)
            .map_err(|_| invalid("Git remote names"))?
            .lines()
            .collect::<Vec<_>>();
        let primary = if remote_names.contains(&"origin") {
            Some("origin")
        } else {
            remote_names.first().copied()
        };
        let names = primary
            .into_iter()
            .chain(remote_names.contains(&"upstream").then_some("upstream"))
            .collect::<BTreeSet<_>>();
        for name in names {
            let bytes = crate::workbench_git::git(repo.root(), &["remote", "get-url", "--", name])?;
            let Some((host, repository)) = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| super::super::remote_identity(s.trim()).ok())
            else {
                continue;
            };
            let Ok(ctx) = Context::located(host, repository, repo.root().into()) else {
                continue;
            };
            let scope = if ctx.kind == "azure-devops" {
                super::azure::issue_scope(&ctx)?
            } else {
                ctx.repository.clone()
            };
            results.push(Source {
                project: project.id,
                title: project.name.clone(),
                scope,
                account: None,
                ctx,
            });
        }
    }
    Ok(results)
}
fn capabilities(kind: &str) -> Option<Value> {
    if !matches!(
        kind,
        "github" | "forgejo" | "gitlab" | "azure-devops" | "linear" | "tanstack-source"
    ) {
        return None;
    }
    Some(
        json!({"create":true,"edit":true,"comment":true,"labels":true,"assignees":true,"reactions":kind!="tanstack-source","templates":!matches!(kind,"tanstack-source"|"azure-devops"),"linkedPullRequests":true,"events":true,"search":true}),
    )
}

pub(in crate::workbench::forge) async fn list(
    State(app): State<Workbench>,
    Json(input): Json<Request>,
) -> super::super::Reply<Value> {
    input.validate()?;
    let targets = app
        .projects()
        .into_iter()
        .filter(|p| {
            !p.metadata.deleted
                && input.project_id.is_none_or(|id| id == p.id)
                && !input.exclude_project_ids.contains(&p.id)
        })
        .collect::<Vec<_>>();
    let discover = app.clone();
    let (sources, mut errors) = tokio::task::spawn_blocking(move || -> Result<_, ForgeError> {
        let config = super::super::Config::load()?;
        let mut sources = Vec::new();
        let mut errors = Vec::new();
        for p in targets {
            match scopes(&discover, &p, &config) {
                Ok(mut rows) => sources.append(&mut rows),
                Err(e) => {
                    errors.push(json!({"projectId":p.id,"projectTitle":p.name,"message":e.detail}))
                }
            }
        }
        Ok((sources, errors))
    })
    .await
    .map_err(|e| ForgeError::bad(e.to_string()))??;
    let mut hosts: BTreeMap<String, (String, BTreeSet<Uuid>)> = BTreeMap::new();
    let mut distinct = BTreeMap::new();
    for s in sources {
        hosts
            .entry(s.ctx.host.clone())
            .or_insert_with(|| (s.ctx.kind.clone(), BTreeSet::new()))
            .1
            .insert(s.project);
        if capabilities(&s.ctx.kind).is_some()
            && input
                .host
                .as_deref()
                .is_none_or(|h| s.ctx.host.eq_ignore_ascii_case(h))
        {
            distinct.entry(s.cursor_key()).or_insert(s);
        }
    }
    let mut scope_info = Vec::new();
    let mut futures = Vec::new();
    let mut accounts: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut viewer_cache = BTreeMap::new();
    for (key, mut source) in distinct {
        let account = source.account_key();
        accounts
            .entry(source.ctx.host.clone())
            .or_default()
            .insert(account.clone());
        scope_info.push(json!({"projectId":source.project,"tracker":source.ctx.kind,"host":source.ctx.host,"scope":source.scope}));
        let cursor = input.cursors.as_ref().and_then(|cs| cs.get(&key)).cloned();
        if input.cursors.is_some() && cursor.is_none() {
            continue;
        }
        let shared = viewer_cache
            .entry(account)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::OnceCell::<Result<String, ForgeError>>::new())
            })
            .clone();
        let app = &app;
        let input = &input;
        futures.push(async move {
            let result = async {
                super::super::signin::attach(app, &mut source.ctx).await?;
                if let Some(token) = super::credentials::resolve(&source.ctx).await? {
                    source.ctx.token = Some(token)
                }
                let viewer = if input.involvement == Involvement::All {
                    None
                } else {
                    Some(shared.get_or_init(|| viewer(&source.ctx)).await.clone()?)
                };
                let page = page(&source, input, cursor.as_deref(), viewer.as_deref()).await;
                Ok::<_, ForgeError>((viewer, page))
            }
            .await;
            (key, source, cursor, result)
        });
    }
    let mut next = BTreeMap::new();
    let mut entries = Vec::new();
    let mut viewers = BTreeMap::new();
    let mut good = BTreeSet::new();
    let mut unusable = BTreeMap::new();
    let mut jobs = stream::iter(futures).buffer_unordered(4);
    while let Some((key, s, cursor, result)) = jobs.next().await {
        let result = result.and_then(|(viewer, page)| {
            if let Some(viewer) = viewer {
                viewers.insert(s.account_key(), viewer);
            }
            page
        });
        match result {
            Ok(page) => {
                good.insert(s.ctx.host.clone());
                for mut row in page.items {
                    row["tracker"] = json!(s.ctx.kind);
                    row["host"] = json!(s.ctx.host);
                    row["scope"] = json!(s.scope);
                    row["projectId"] = json!(s.project);
                    row["projectTitle"] = json!(s.title);
                    entries.push(row)
                }
                if let Some(cursor) = page.next {
                    next.insert(key, cursor);
                }
            }
            Err(e) => {
                if e.status == 401 {
                    unusable.insert(s.account_key(), e.detail.clone());
                } else {
                    good.insert(s.ctx.host.clone());
                }
                if let Some(cursor) = cursor {
                    next.insert(key, cursor);
                }
                errors.push(json!({"projectId":s.project,"projectTitle":format!("{} ({})",s.title,s.scope),"message":e.detail}));
            }
        }
    }
    entries.sort_by(|a, b| {
        b["updatedAt"]
            .as_str()
            .cmp(&a["updatedAt"].as_str())
            .then_with(|| a["url"].as_str().cmp(&b["url"].as_str()))
    });
    let trackers=hosts.into_iter().map(|(host,(kind,projects))|{
        let caps=capabilities(&kind);
        let error=if caps.is_none(){Some("This Git host has no issue tracker.".to_owned())}
        else if !good.contains(&host) && let Some(keys)=accounts.get(&host) && keys.iter().all(|k|unusable.contains_key(k)){keys.iter().find_map(|k|unusable.get(k).cloned())}else{None};
        json!({"tracker":kind,"host":host,"projectCount":projects.len(),"capabilities":caps,"configured":error.is_none(),"detail":error})
    }).collect::<Vec<_>>();
    Ok(Json(
        json!({"entries":entries,"viewers":viewers,"nextCursors":next,"trackers":trackers,"errors":errors,"scopes":scope_info}),
    ))
}
async fn viewer(c: &Context) -> Result<String, ForgeError> {
    let raw = match c.kind.as_str() {
        "github" | "forgejo" => get(c, path(c, &["user"])?).await?["login"].clone(),
        "gitlab" | "tanstack-source" => get(c, path(c, &["user"])?).await?["username"].clone(),
        "linear" => super::linear::gql(
            c,
            "query KarIssueListViewer { viewer { displayName } }",
            json!({}),
            false,
        )
        .await?["viewer"]["displayName"]
            .clone(),
        "azure-devops" => json!("@Me"),
        _ => return Err(ForgeError::unsupported("no native issue viewer")),
    };
    raw.as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid("signed-in issue viewer"))
}
async fn page(
    s: &Source,
    r: &Request,
    cursor: Option<&str>,
    viewer: Option<&str>,
) -> Result<Page, ForgeError> {
    match s.ctx.kind.as_str() {
        "github" => git_hosts::github(&s.ctx, r, cursor, viewer).await,
        "gitlab" | "forgejo" => git_hosts::rest(&s.ctx, r, cursor, viewer).await,
        "linear" => standalone::linear(&s.ctx, &s.scope, r, cursor, viewer).await,
        "tanstack-source" => standalone::source(&s.ctx, r, cursor, viewer).await,
        "azure-devops" => azure::listing(&s.ctx, r, cursor).await,
        _ => Err(ForgeError::unsupported(
            "no native paged issue search for this host",
        )),
    }
}
