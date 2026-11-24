use super::*;
use axum::{Router, extract::State, routing::post};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
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
    async fn context(&self, cwd: PathBuf) -> Result<Context, ForgeError> {
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
    let (name, url, ssh) = match c.kind.as_str() {
        "github" => (
            text_field(&r, "full_name")?.to_owned(),
            text_field(&r, "html_url")?.to_owned(),
            text_field(&r, "ssh_url")?.to_owned(),
        ),
        "gitlab" => (
            text_field(&r, "path_with_namespace")?.into(),
            text_field(&r, "web_url")?.into(),
            text_field(&r, "ssh_url_to_repo")?.into(),
        ),
        "forgejo" => (
            text_field(&r, "full_name")?.into(),
            text_field(&r, "clone_url")?.into(),
            text_field(&r, "ssh_url")?.into(),
        ),
        "azure-devops" => (
            format!(
                "{}/{}",
                text_field(&r["project"], "name")?,
                text_field(&r, "name")?
            ),
            text_field(&r, "remoteUrl")?.into(),
            text_field(&r, "sshUrl")?.into(),
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
            let name = text_field(&r, "full_name")?.to_owned();
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
                format!("{}/{}", text_field(&r, "owner")?, text_field(&r, "name")?)
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

#[derive(Deserialize, Clone, Copy, Default)]
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
fn clone_source(app: &Workbench, src: &str) -> Result<std::ffi::OsString, ForgeError> {
    if std::path::Path::new(src).is_absolute() {
        let p = std::path::Path::new(src)
            .canonicalize()
            .map_err(Error::Io)?;
        app.allowed(&p)?;
        return Ok(p.into_os_string());
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
        return Ok(src.into());
    }
    let mut u = Url::parse(src).map_err(|_| ForgeError::bad("invalid clone URL"))?;
    if !matches!(u.scheme(), "https" | "ssh")
        || u.host().is_none()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(ForgeError::bad(
            "expected HTTPS or SSH clone URL without embedded credentials",
        ));
    }
    if u.scheme() == "https" {
        u.set_username("")
            .map_err(|_| ForgeError::bad("invalid HTTPS clone address"))?;
    }
    Ok(u.to_string().into())
}
async fn lookup(State(app): State<Workbench>, Json(req): Json<Lookup>) -> Reply<CloneUrls> {
    let cwd = app
        .0
        .roots
        .first()
        .ok_or_else(|| ForgeError::bad("missing workspace root"))?
        .path
        .clone();
    resolve(&req.context(cwd).await?).await.map(Json)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Prepared {
    pub(super) dest: PathBuf,
    pub(super) source: String,
    pub(super) repo: Option<CloneUrls>,
}
pub(super) async fn prepare(app: &Workbench, req: CloneInput) -> Result<Prepared, ForgeError> {
    if std::fs::symlink_metadata(&req.directory).is_ok_and(|meta| meta.is_symlink()) {
        return Err(ForgeError::bad("clone destination is a symbolic link"));
    }
    let root = app.allowed(&req.directory)?;
    let relative = req
        .directory
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
        Some(request) => {
            Some(resolve(&request.context(dest.parent().unwrap().to_owned()).await?).await?)
        }
    };
    let source = repo
        .as_ref()
        .map(|v| match req.protocol {
            Protocol::Https => v.url.clone(),
            Protocol::Auto | Protocol::Ssh => v.ssh_url.clone(),
        })
        .or(req.source)
        .ok_or_else(|| ForgeError::bad("missing repository or URL"))?;
    let selected = clone_source(&app, &source)?;
    let source = selected.to_string_lossy().into_owned();
    Ok(Prepared { dest, source, repo })
}
async fn clone(State(app): State<Workbench>, Json(req): Json<CloneInput>) -> Reply<Value> {
    let Prepared { dest, source, repo } = prepare(&app, req).await?;
    let selected = std::ffi::OsString::from(&source);
    let installed = tokio::task::spawn_blocking(move || {
        Repository::clone_into(&selected, &dest).map(|r| r.root().to_owned())
    })
    .await
    .map_err(|_| ForgeError::remote(500, "clone worker stopped"))??;
    Ok(Json(
        json!({"cwd":installed,"remoteUrl":source,"repository":repo}),
    ))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/forge/repositories/lookup", post(lookup))
        .route("/forge/repositories/clone", post(clone))
}
