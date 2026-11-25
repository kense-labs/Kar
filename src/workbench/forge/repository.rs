use super::*;
pub(in crate::workbench::forge) mod publication;
use axum::{Router, extract::State, routing::post};
use serde::Deserialize;

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Lookup {
    provider: String,
    host: Option<String>,
    repository: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CloneUrls {
    provider: String,
    name_with_owner: String,
    url: String,
    ssh_url: String,
}

fn hosted(kind: &str) -> Result<&'static str, ForgeError> {
    match kind {
        "github" => Ok("github.com"),
        "gitlab" => Ok("gitlab.com"),
        "forgejo" => Ok("codeberg.org"),
        "bitbucket" => Ok("bitbucket.org"),
        "azure-devops" => Ok("dev.azure.com"),
        "gitcafe" => Ok("git.cafe"),
        "cursor-origin" => Ok("origin.cursor.com"),
        "tanstack-source" => Ok("source.tanstack.com"),
        _ => Err(ForgeError::unsupported("unknown source control driver")),
    }
}
impl Lookup {
    async fn context(&self, app: &Workbench, cwd: PathBuf) -> Result<Context, ForgeError> {
        let default_host = hosted(&self.provider)?;
        let (host, path) = if self.repository.contains("://") || self.repository.starts_with("git@")
        {
            let (host, repo) = remote_identity(&self.repository)?;
            if self
                .host
                .as_ref()
                .is_some_and(|h| !h.eq_ignore_ascii_case(&host))
            {
                return Err(ForgeError::bad("repository URL belongs to another host"));
            }
            (host, repo)
        } else {
            let host = self
                .host
                .as_deref()
                .unwrap_or(default_host)
                .to_ascii_lowercase();
            let path = self
                .repository
                .trim()
                .trim_matches('/')
                .trim_end_matches(".git")
                .to_owned();
            if path.split('/').any(|v| {
                v.is_empty() || matches!(v, "." | "..") || v.contains(['\\', '\0', '?', '#'])
            }) {
                return Err(ForgeError::bad("invalid repository name"));
            }
            (host, path)
        };
        let mut c = Context::located(host, path, cwd)?;
        if c.kind != self.provider {
            return Err(ForgeError::bad("provider does not match this forge host"));
        }
        signin::attach(app, &mut c).await?;
        if let Some(t) = providers::credentials::resolve(&c).await? {
            c.token = Some(t);
        }
        Ok(c)
    }
}
fn text_field<'a>(v: &'a Value, field: &str) -> Result<&'a str, ForgeError> {
    v[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ForgeError::remote(502, format!("repository response omitted {field}")))
}
async fn resolve(c: &Context) -> Result<CloneUrls, ForgeError> {
    let r = match c.kind.as_str() {
        "azure-devops" => providers::azure::repository(c).await?,
        "gitcafe" | "tanstack-source" | "cursor-origin" => {
            providers::get(c, providers::hosted::resource(c, None, None)?).await?
        }
        _ => providers::get(c, providers::rest::project(c)?).await?,
    };
    clone_urls(c, &r)
}
pub(in crate::workbench::forge) fn clone_urls(
    c: &Context,
    r: &Value,
) -> Result<CloneUrls, ForgeError> {
    let (name, url, ssh) = match c.kind.as_str() {
        "github" => (
            text_field(r, "full_name")?.to_owned(),
            text_field(r, "html_url")?.to_owned(),
            text_field(r, "ssh_url")?.to_owned(),
        ),
        "gitlab" => (
            text_field(r, "path_with_namespace")?.into(),
            text_field(r, "web_url")?.into(),
            text_field(r, "ssh_url_to_repo")?.into(),
        ),
        "forgejo" => (
            text_field(r, "full_name")?.into(),
            text_field(r, "clone_url")?.into(),
            text_field(r, "ssh_url")?.into(),
        ),
        "azure-devops" => (
            format!(
                "{}/{}",
                text_field(&r["project"], "name")?,
                text_field(r, "name")?
            ),
            text_field(r, "remoteUrl")?.into(),
            text_field(r, "sshUrl")?.into(),
        ),
        "gitcafe" => {
            let name = c.repository.clone();
            if r["name"] != name.split('/').next_back().unwrap_or("") {
                return Err(ForgeError::remote(
                    502,
                    "GitCafe returned another repository",
                ));
            }
            (
                name.clone(),
                format!("https://{}/{name}", c.host),
                format!("ssh@{}:{name}.git", c.host),
            )
        }
        "bitbucket" => {
            let name = text_field(r, "full_name")?.to_owned();
            let clone = |k: &str| {
                r["links"]["clone"].as_array()?.iter().find(|link| {
                    link["name"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(k))
                })?["href"]
                    .as_str()
            };
            let url = clone("https")
                .or_else(|| r.pointer("/links/html/href").and_then(Value::as_str))
                .ok_or_else(|| ForgeError::remote(502, "Bitbucket returned no HTTPS clone URL"))?;
            (name, url.into(), clone("ssh").unwrap_or(url).into())
        }
        "tanstack-source" | "cursor-origin" => {
            let name = if c.kind == "tanstack-source" {
                format!("{}/{}", text_field(r, "owner")?, text_field(r, "name")?)
            } else {
                r["fullName"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        Some(format!(
                            "{}/{}",
                            r.pointer("/owner/slug")?.as_str()?,
                            r["name"].as_str()?
                        ))
                    })
                    .unwrap_or_else(|| c.repository.clone())
            };
            let path = if c.kind == "tanstack-source" {
                format!("@{name}")
            } else {
                name.clone()
            };
            let url = r["cloneUrl"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("https://{}/{path}.git", c.host));
            (name, url.clone(), url)
        }
        _ => return Err(ForgeError::unsupported("no repository lookup driver")),
    };
    if c.kind != "azure-devops" && !name.eq_ignore_ascii_case(c.repository.trim_start_matches('@'))
    {
        return Err(ForgeError::remote(
            502,
            "lookup returned a different repository",
        ));
    }
    Ok(CloneUrls {
        provider: c.kind.clone(),
        name_with_owner: name,
        url,
        ssh_url: ssh,
    })
}

#[derive(Deserialize, Serialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Protocol {
    #[default]
    Auto,
    Https,
    Ssh,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CloneInput {
    directory: PathBuf,
    source: Option<String>,
    lookup: Option<Lookup>,
    #[serde(default)]
    protocol: Protocol,
}
fn clone_source(app: &Workbench, src: &str) -> Result<(String, Option<String>), ForgeError> {
    if src.chars().any(char::is_control) {
        return Err(ForgeError::bad("invalid clone address"));
    }
    if std::path::Path::new(src).is_absolute() {
        let p = std::path::Path::new(src)
            .canonicalize()
            .map_err(Error::Io)?;
        app.allowed(&p)?;
        return Ok((p.to_string_lossy().into_owned(), None));
    }
    if !src.contains("://") {
        let (prefix, path) = src
            .split_once(':')
            .ok_or_else(|| ForgeError::bad("expected a clone URL"))?;
        let (user, host) = prefix
            .split_once('@')
            .ok_or_else(|| ForgeError::bad("expected an SSH user@host:path clone address"))?;
        if !matches!(user, "git" | "ssh")
            || host.starts_with('-')
            || host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
            || path.starts_with('-')
            || path
                .split('/')
                .any(|s| s.is_empty() || matches!(s, "." | ".."))
        {
            return Err(ForgeError::bad("invalid SSH repository address"));
        }
        return Ok((src.into(), None));
    }
    let mut u = Url::parse(src).map_err(|_| ForgeError::bad("invalid clone URL"))?;
    if !matches!(u.scheme(), "https" | "ssh")
        || u.host().is_none()
        || u.fragment().is_some()
        || (u.scheme() == "ssh" && (u.password().is_some() || u.query().is_some()))
    {
        return Err(ForgeError::bad("expected an HTTPS or SSH clone URL"));
    }
    let transport = u.to_string();
    if u.scheme() == "https" {
        u.set_password(None)
            .map_err(|_| ForgeError::bad("invalid clone address"))?;
        u.set_username("")
            .map_err(|_| ForgeError::bad("invalid clone address"))?;
        u.set_query(None);
    }
    let source = u.to_string();
    Ok((source.clone(), (transport != source).then_some(transport)))
}
async fn lookup(State(app): State<Workbench>, Json(req): Json<Lookup>) -> Reply<CloneUrls> {
    let cwd = app
        .0
        .roots
        .first()
        .ok_or_else(|| ForgeError::bad("missing workspace root"))?
        .path
        .clone();
    resolve(&req.context(&app, cwd).await?).await.map(Json)
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Prepared {
    pub(super) dest: PathBuf,
    pub(super) source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) fallback: Option<String>,
    // Inline transfer credentials have the lifetime of the host process, not the project record.
    #[serde(skip)]
    pub(super) credential_url: Option<String>,
    pub(super) repo: Option<CloneUrls>,
}
pub(super) async fn prepare(app: &Workbench, req: CloneInput) -> Result<Prepared, ForgeError> {
    if !req.directory.is_absolute()
        || req
            .directory
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(super::super::denied().into());
    }
    if std::fs::symlink_metadata(&req.directory).is_ok_and(|meta| meta.is_symlink()) {
        return Err(ForgeError::bad("clone destination is a symbolic link"));
    }
    let mut ancestor = req.directory.as_path();
    let resolved = loop {
        match ancestor.canonicalize() {
            Ok(existing) => {
                break existing.join(
                    req.directory
                        .strip_prefix(ancestor)
                        .map_err(|_| ForgeError::bad("invalid clone destination"))?,
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| ForgeError::from(super::super::denied()))?;
            }
            Err(e) => return Err(Error::Io(e).into()),
        }
    };
    let root = app.allowed(&resolved)?;
    let relative = resolved
        .strip_prefix(&root.path)
        .map_err(|_| ForgeError::bad("invalid clone destination"))?;
    let ws = app.workspace_at(&root.path)?;
    let relative = ws.target(relative, true)?;
    let _parent = ws.parent(&relative, true)?;
    let dest = app.destination(&ws.root.join(relative))?;
    if dest.exists()
        && (!dest.is_dir()
            || std::fs::read_dir(&dest)
                .map_err(Error::Io)?
                .next()
                .is_some())
    {
        return Err(ForgeError::bad("clone destination must be empty"));
    }

    let repo = match req.lookup {
        None => None,
        Some(request) => Some(
            resolve(
                &request
                    .context(&app, dest.parent().unwrap().to_owned())
                    .await?,
            )
            .await?,
        ),
    };
    let source = repo
        .as_ref()
        .map(|v| match req.protocol {
            Protocol::Https => v.url.clone(),
            Protocol::Auto | Protocol::Ssh => v.ssh_url.clone(),
        })
        .or(req.source)
        .ok_or_else(|| ForgeError::bad("missing repository or URL"))?;
    let (source, credential_url) = clone_source(app, &source)?;
    let fallback = if matches!(req.protocol, Protocol::Ssh) {
        None
    } else if let Some(repo) = &repo {
        (source == repo.ssh_url && repo.url != source).then(|| repo.url.clone())
    } else {
        ssh_to_https(&source)
    };
    let fallback = fallback
        .map(|url| -> Result<String, ForgeError> {
            let (candidate, credentials) = clone_source(app, &url)?;
            if !candidate.starts_with("https://") || credentials.is_some() {
                return Err(ForgeError::bad("invalid HTTPS clone fallback"));
            }
            Ok(candidate)
        })
        .transpose()?;
    Ok(Prepared {
        dest,
        source,
        credential_url,
        fallback,
        repo,
    })
}
/// A named SSH port does not identify the HTTPS endpoint. Host-specific forms are mapped before general SCP/URI translation.
fn ssh_to_https(source: &str) -> Option<String> {
    let (host, path, is_scp) = if source.starts_with("ssh://") {
        let u = Url::parse(source).ok()?;
        if u.port().is_some() {
            return None;
        }
        (
            u.host_str()?.to_string(),
            u.path().trim_start_matches('/').to_owned(),
            false,
        )
    } else {
        if source.contains("://") || std::path::Path::new(source).is_absolute() {
            return None;
        }
        let (authority, path) = source.split_once(':')?;
        (
            authority.rsplit('@').next()?.to_ascii_lowercase(),
            path.to_string(),
            true,
        )
    };
    if host == "ssh.dev.azure.com" {
        let p: Vec<_> = path.split('/').collect();
        return match p.as_slice() {
            ["v3", org, project, repo] => {
                Some(format!("https://dev.azure.com/{org}/{project}/_git/{repo}"))
            }
            _ => None,
        };
    }
    let host = if matches!(host.as_str(), "ssh.bitbucket.org" | "altssh.bitbucket.org") {
        "bitbucket.org"
    } else {
        &host
    };
    if is_scp {
        let mut u = Url::parse(&format!("https://{host}/")).ok()?;
        u.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .extend(path.split('/'));
        Some(u.to_string())
    } else {
        Some(format!("https://{host}/{path}"))
    }
}
async fn clone(State(app): State<Workbench>, Json(req): Json<CloneInput>) -> Reply<Value> {
    let input = prepare(&app, req).await?;
    let dest = &input.dest;
    let parent = dest
        .parent()
        .ok_or_else(|| ForgeError::bad("invalid clone destination"))?;
    let stage = tempfile::Builder::new()
        .prefix(".kar-clone-")
        .tempdir_in(parent)
        .map_err(Error::Io)?;
    let (_guard, mut stop) = tokio::sync::watch::channel(false);
    let (checkout, selected) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        clone_jobs::download(None, &input, stage.path(), &mut stop),
    )
    .await
    .map_err(|_| ForgeError::remote(504, "git clone timed out"))??;
    if app.destination(dest)? != *dest || std::fs::symlink_metadata(dest).is_ok_and(|m| !m.is_dir())
    {
        return Err(ForgeError::bad("clone destination changed"));
    }
    // Rename preserves a concurrently populated destination. Both attempts write only inside our staging directory.
    std::fs::rename(checkout, dest).map_err(Error::Io)?;
    Ok(Json(
        json!({"cwd":dest,"remoteUrl":selected,"repository":input.repo}),
    ))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/forge/publish",
            post(publication::publish),
        )
        .route("/forge/repositories/lookup", post(lookup))
        .route("/forge/repositories/clone", post(clone))
}
