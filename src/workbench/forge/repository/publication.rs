use super::*;
use crate::workbench_git::{StackCheckout, command, git};
use axum::extract::Path;
use std::path::Path as FsPath;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Visibility {
    Public,
    Private,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::workbench::forge) struct Request {
    id: Uuid,
    provider: String,
    host: Option<String>,
    repository: String,
    visibility: Visibility,
    remote_name: Option<String>,
    #[serde(default)]
    protocol: Protocol,
}

#[derive(Clone, Serialize, Deserialize)]
pub(in crate::workbench::forge) struct Record {
    project: Uuid,
    request: Request,
    created: Option<CloneUrls>,
    done: Option<Result<Value, ForgeError>>,
}

pub(in crate::workbench::forge) async fn publish(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(req): Json<Request>,
) -> Reply<Value> {
    tokio::spawn(run(app, project, req))
        .await
        .map_err(|_| ForgeError::uncertain("repository publication worker stopped"))?
        .map(Json)
}

async fn run(app: Workbench, project: Uuid, req: Request) -> Result<Value, ForgeError> {
    let app_owner = app.clone();
    let checkout =
        tokio::task::spawn_blocking(move || StackCheckout::acquire(app_owner.repository(project)?))
            .await
            .map_err(|_| ForgeError::remote(500, "could not open publication checkout"))??;
    let cwd = checkout.repo.root().to_path_buf();
    let name = req
        .remote_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    if name.starts_with('-')
        || name == "."
        || git(
            &cwd,
            &["check-ref-format", &format!("refs/remotes/{name}/HEAD")],
        )
        .is_err()
    {
        return Err(ForgeError::bad("invalid publication remote name"));
    }
    let lookup = Lookup {
        provider: req.provider.clone(),
        host: req.host.clone(),
        repository: req.repository.clone(),
    };
    let c = lookup.context(cwd.clone()).await?;
    let id = req.id;
    let record = app.update(|db| {
        if let Some(r) = db.forge.publications.get(&id) {
            if r.project != project || r.request != req {
                return Err(Error::Refused(
                    "publication ID belongs to another request".into(),
                ));
            }
            return Ok(Some(r.clone()));
        }
        db.forge.publications.insert(
            id,
            Record {
                project,
                request: req.clone(),
                created: None,
                done: None,
            },
        );
        Ok(None)
    })?;
    let urls = if let Some(r) = record {
        if let Some(done) = r.done {
            return done;
        }
        r.created.ok_or_else(|| {
            ForgeError::uncertain(
                "repository create was already requested; verify its status on the forge",
            )
        })?
    } else {
        match providers::create_repo::create(&c, req.visibility == Visibility::Private, id).await {
            Ok(urls) => {
                app.update(|db| {
                    db.forge.publications.get_mut(&id).unwrap().created = Some(urls.clone());
                    Ok(())
                })?;
                urls
            }
            Err(e) => {
                app.update(|db| {
                    db.forge.publications.get_mut(&id).unwrap().done = Some(Err(e.clone()));
                    Ok(())
                })?;
                return Err(e);
            }
        }
    };
    let preferred = name.to_owned();
    let protocol = req.protocol;
    let result =
        tokio::task::spawn_blocking(move || install(checkout, &c, &urls, &preferred, protocol))
            .await
            .map_err(|_| ForgeError::uncertain("remote created; local publish worker stopped"))??;
    app.update(|db| {
        db.forge.publications.get_mut(&id).unwrap().done = Some(Ok(result.clone()));
        Ok(())
    })?;
    Ok(result)
}

fn config(cwd: &FsPath, name: &str, field: &str) -> Result<Vec<String>, ForgeError> {
    let r = command(cwd)
        .args(["config", "--get-all", &format!("remote.{name}.{field}")])
        .output()
        .map_err(Error::Io)?;
    if r.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    if !r.status.success() {
        return Err(ForgeError::bad("could not read Git remote configuration"));
    }
    Ok(std::str::from_utf8(&r.stdout)
        .map_err(|_| ForgeError::bad("invalid remote configuration"))?
        .lines()
        .map(str::to_owned)
        .collect())
}

fn ensure(
    cwd: &FsPath,
    preferred: &str,
    selected: &str,
    alternate: Option<&str>,
) -> Result<(String, String), ForgeError> {
    let r = git(cwd, &["remote"])?;
    let remotes: Vec<_> = std::str::from_utf8(&r)
        .map_err(|_| ForgeError::bad("invalid Git remote name"))?
        .lines()
        .collect();
    for name in
        std::iter::once(preferred).chain(remotes.iter().copied().filter(|r| *r != preferred))
    {
        if !remotes.contains(&name) {
            continue;
        }
        let urls = config(cwd, name, "url")?;
        if let [url] = urls.as_slice()
            && (url == selected || Some(url.as_str()) == alternate)
            && config(cwd, name, "pushurl")?.is_empty()
        {
            return Ok((name.to_owned(), url.clone()));
        }
    }
    let mut name = preferred.to_string();
    let mut i = 2;
    while remotes.contains(&name.as_str()) {
        name = format!("{preferred}-{i}");
        i += 1;
    }
    git(cwd, &["remote", "add", "--", &name, selected])?;
    Ok((name, selected.to_owned()))
}

fn normalized_repo(s: &str) -> &str {
    s.trim_start_matches('@').trim_end_matches(".git")
}
fn validate_url(app_host: &Context, urls: &CloneUrls, selected: &str) -> Result<(), ForgeError> {
    if selected.contains("://") {
        let u = Url::parse(selected).map_err(|_| ForgeError::bad("invalid publication URL"))?;
        if !matches!(u.scheme(), "https" | "ssh")
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err(ForgeError::bad("invalid publication transport"));
        }
    } else if !selected.starts_with("git@") && !selected.starts_with("ssh@") {
        return Err(ForgeError::bad("invalid SSH publication address"));
    }
    let (h, repo) = remote_identity(selected)?;
    if app_host.kind == "azure-devops" {
        // Azure HTTPS: org/project/_git/name; SSH: v3/org/project/name; legacy host contains the org.
        let parse = |h: &str, r: &str| -> Option<(String, String, String)> {
            let p: Vec<_> = r.split('/').collect();
            let (o, p, n) = match (h, p.as_slice()) {
                ("dev.azure.com", [o, p, "_git", n]) | ("ssh.dev.azure.com", ["v3", o, p, n]) => {
                    (*o, *p, *n)
                }
                (host, [p, "_git", n]) if host.ends_with(".visualstudio.com") => {
                    (host.trim_end_matches(".visualstudio.com"), *p, *n)
                }
                (_, _) => return None,
            };
            Some((
                o.to_ascii_lowercase(),
                p.to_ascii_lowercase(),
                n.to_ascii_lowercase(),
            ))
        };
        if let Some(expected) = parse(&app_host.host, &app_host.repository)
            && parse(&h, &repo) == Some(expected)
        {
            return Ok(());
        }
        // A self-hosted Azure endpoint uses its own registered authority.
        if h == app_host.host && repo.eq_ignore_ascii_case(&app_host.repository) {
            return Ok(());
        }
    } else if h.eq_ignore_ascii_case(&app_host.host)
        && normalized_repo(&repo).eq_ignore_ascii_case(normalized_repo(&urls.name_with_owner))
    {
        return Ok(());
    }
    Err(ForgeError::bad(
        "remote created; clone URL does not identify the created repository",
    ))
}

fn install(
    checkout: StackCheckout,
    c: &Context,
    urls: &CloneUrls,
    preferred: &str,
    protocol: Protocol,
) -> Result<Value, ForgeError> {
    let cwd = checkout.repo.root();
    let first = match protocol {
        Protocol::Https => &urls.url,
        Protocol::Ssh | Protocol::Auto => &urls.ssh_url,
    };
    validate_url(c, urls, first)?;
    let alternate = if protocol == Protocol::Auto && first != &urls.url {
        validate_url(c, urls, &urls.url)?;
        Some(urls.url.as_str())
    } else {
        None
    };
    let (remote, mut selected) = ensure(cwd, preferred, first, alternate)?;
    let r = command(cwd)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .map_err(Error::Io)?;
    if !r.status.success() {
        return Err(ForgeError::bad(format!(
            "repository created and remote {remote} configured; select a local branch to push"
        )));
    }
    let branch = std::str::from_utf8(&r.stdout)
        .map_err(|_| ForgeError::bad("invalid branch name"))?
        .trim_end_matches('\n');
    let head = command(cwd)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .output()
        .map_err(Error::Io)?;
    if !matches!(head.status.code(), Some(0 | 1)) {
        return Err(ForgeError::bad("remote created; Git could not read HEAD"));
    }
    let mut status = "remote_added";
    if head.status.success() {
        let suffix = branch
            .strip_prefix(&format!("{remote}/"))
            .filter(|s| !s.is_empty())
            .unwrap_or(branch);
        let spec = format!("refs/heads/{branch}:refs/heads/{suffix}");
        let push = || {
            command(cwd)
                .env_remove("KAR_WORKBENCH_TOKEN")
                .env("LC_ALL", "C")
                .args(["push", "--set-upstream", "--", &remote, &spec])
                .output()
                .map_err(Error::Io)
        };
        let mut result = push()?;
        let diagnostic = String::from_utf8_lossy(&result.stderr).to_ascii_lowercase();
        if !result.status.success()
            && protocol == Protocol::Auto
            && selected != urls.url
            && (diagnostic.contains("permission denied (publickey")
                || diagnostic.contains("host key verification failed")
                || diagnostic.contains("authentication failed"))
        {
            validate_url(c, urls, &urls.url)?;
            git(cwd, &["remote", "set-url", "--", &remote, &urls.url])?;
            selected = urls.url.clone();
            result = push()?;
        }
        if !result.status.success() {
            return Err(ForgeError::remote(
                409,
                format!(
                    "{} was created and remote {remote} was configured; push of {branch} failed ({}). Correct the Git error and retry this publication ID",
                    urls.name_with_owner, result.status
                ),
            ));
        }
        status = "pushed";
    }
    Ok(
        json!({"status":status,"repository":urls,"remoteName":remote,"remoteUrl":selected,"branch":branch}),
    )
}
