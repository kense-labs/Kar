use super::*;
use crate::workbench_git::{PublishTarget, Repository};

pub(in crate::workbench) struct PullTarget {
    pub head: String,
    pub base: String,
    pub repository: Option<String>,
    pub context: Context,
}

pub(super) fn check_source(c: &Context, repository: &str) -> Result<(), ForgeError> {
    let segments: Vec<_> = repository.split('/').collect();
    if segments.len() < 2
        || segments.iter().any(|s| {
            s.is_empty()
                || matches!(*s, "." | "..")
                || s.contains([':', '?', '#', '\\'])
                || s.chars().any(char::is_control)
        })
    {
        return Err(ForgeError::bad("invalid pull request source repository"));
    }
    if repository.eq_ignore_ascii_case(&c.repository) {
        return Ok(());
    }
    match c.kind.as_str() {
        "gitlab" => Ok(()),
        "github" | "gitcafe" | "bitbucket" | "forgejo" if segments.len() == 2 => Ok(()),
        _ => Err(ForgeError::unsupported(format!(
            "{} does not select a fork repository when creating a pull request",
            c.kind
        ))),
    }
}

impl PullTarget {
    pub async fn resolve(
        app: Workbench,
        project: Uuid,
        checkout: &Repository,
        local: &str,
        target: &PublishTarget,
        requested_base: Option<&str>,
    ) -> Result<Self, ForgeError> {
        let c = Context::open(app.clone(), project).await?;
        let (fetch, pushes) = checkout.publish_urls(target)?;
        let mut destination = None;
        for url in pushes {
            // For a local transport the hosted identity is the named remote's fetch URL.
            let local_transport = std::path::Path::new(&url).is_absolute()
                || url.starts_with("file://")
                || !url.contains(':');
            let (host, repo) = remote_identity(if local_transport { &fetch } else { &url })?;
            if !host.eq_ignore_ascii_case(&c.host) {
                return Err(ForgeError::bad(
                    "pull request source and target have different hosts",
                ));
            }
            if destination
                .as_ref()
                .is_some_and(|previous: &String| !previous.eq_ignore_ascii_case(&repo))
            {
                return Err(ForgeError::bad(
                    "publish URLs identify multiple pull request sources",
                ));
            }
            destination = Some(repo);
        }
        let source =
            destination.ok_or_else(|| ForgeError::bad("no push URL for the selected remote"))?;
        check_source(&c, &source)?;
        let repository = (!source.eq_ignore_ascii_case(&c.repository)).then_some(source);
        let base = match requested_base
            .map(str::to_owned)
            .or(checkout.comparison_branch(local, target)?)
        {
            Some(b) => b,
            None => super::default_branch(app, project).await?,
        };
        if repository.is_none() && target.branch == base {
            return Err(ForgeError::bad("pull request head and base are the same"));
        }
        Ok(Self {
            head: target.branch.clone(),
            base,
            repository,
            context: c,
        })
    }
}

/// GitLab merge requests encode their source by project ID even when two forks have the same branch.
pub(super) async fn matches_repository(
    c: &Context,
    expected: &str,
    pull: &Value,
) -> Result<bool, ForgeError> {
    if let Some(source) = pull["head_repository"].as_str() {
        return Ok(source.eq_ignore_ascii_case(expected));
    }
    if c.kind == "azure-devops" && pull["single_repository"] == true {
        return Ok(expected.eq_ignore_ascii_case(&c.repository));
    }
    if c.kind == "gitlab"
        && let Some(id) = pull["head_project_id"].as_u64()
    {
        return providers::gitlab_project_matches(c, expected, id).await;
    }
    Ok(false)
}
