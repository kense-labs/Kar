//! Repository-scoped change requests, issue bindings and revision-specific review state.
mod github;
mod providers;
mod review;
mod routes;
pub(super) fn routes() -> axum::Router<Workbench> {
    routes::routes()
}
use super::Workbench;
use crate::{Error, workbench_git::Repository};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
use url::Url;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Identity,
    Pull {
        number: u64,
    },
    Pulls,
    CreatePull {
        head: String,
        base: String,
        title: String,
        body: String,
    },
    Issues,
    Issue {
        number: u64,
    },
    SetIssueState {
        number: u64,
        state: String,
    },
}
impl Operation {
    fn mutates(&self) -> bool {
        matches!(self, Self::CreatePull { .. } | Self::SetIssueState { .. })
    }
}
#[derive(Clone)]
pub(crate) struct Context {
    kind: String,
    host: String,
    repository: String,
    api_base: Url,
    token: Option<String>,
    client: Client,
    cwd: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ForgeError {
    pub kind: String,
    pub status: u16,
    pub detail: String,
}
impl ForgeError {
    fn new(kind: &str, status: u16, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            status,
            detail: detail.into(),
        }
    }
    pub(crate) fn unsupported(s: impl Into<String>) -> Self {
        Self::new("unsupported", 422, s)
    }
    pub(crate) fn bad(s: impl Into<String>) -> Self {
        Self::new("invalid", 400, s)
    }
    pub(crate) fn remote(status: u16, s: impl Into<String>) -> Self {
        let kind = match status {
            401 | 403 => "permission",
            429 => "rate_limited",
            404 => "not_found",
            _ => "remote_error",
        };
        Self::new(kind, status, s)
    }
    pub(crate) fn uncertain(s: impl Into<String>) -> Self {
        Self::new("needs_reconciliation", 409, s)
    }
}
impl From<Error> for ForgeError {
    fn from(e: Error) -> Self {
        let status = match &e {
            Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => 404,
            Error::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => 403,
            Error::Io(_) | Error::Json(_) => 500,
            Error::Busy => 409,
            _ => 400,
        };
        Self::new("workbench", status, e.to_string())
    }
}
impl IntoResponse for ForgeError {
    fn into_response(self) -> Response {
        (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(self),
        )
            .into_response()
    }
}
type Reply<T> = Result<Json<T>, ForgeError>;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct ForgeState {
    operations: BTreeMap<Uuid, Mutation>,
    bindings: BTreeMap<Uuid, Binding>,
    reviews: BTreeMap<String, Review>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Mutation {
    project: Uuid,
    chat: Uuid,
    repository: String,
    operation: Operation,
    result: Option<Result<Value, ForgeError>>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Binding {
    project: Uuid,
    chat: Uuid,
    repository: String,
    kind: String,
    number: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Review {
    requested: u64,
    applied: u64,
    pull: Value,
    checked: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    hosts: Vec<Host>,
    #[serde(default)]
    issue_trackers: Vec<providers::linear::Team>,
}
impl Config {
    fn load() -> Result<Self, ForgeError> {
        match std::env::var_os("KAR_FORGE_CONFIG") {
            None => Ok(Self {
                hosts: vec![],
                issue_trackers: vec![],
            }),
            Some(path) => toml::from_str(&std::fs::read_to_string(path).map_err(Error::Io)?)
                .map_err(|_| ForgeError::bad("invalid host forge configuration")),
        }
    }
}
#[derive(Deserialize)]
struct Host {
    host: String,
    kind: String,
    api_base: String,
    token_env: Option<String>,
}

const DRIVER_KINDS: &[&str] = &[
    "github",
    "cursor-origin",
    "gitlab",
    "gitcafe",
    "tanstack-source",
    "forgejo",
    "bitbucket",
    "azure-devops",
    "linear",
];
// Actions offered by each fixed upstream definition. The Kar call surface is reported separately.
fn declared_pull_actions(kind: &str) -> &'static [&'static str] {
    match kind {
        "github" => &[
            "merge",
            "ready",
            "draft",
            "close",
            "reopen",
            "delete-branch",
            "update-branch",
            "enable-auto-merge",
            "disable-auto-merge",
            "revert",
            "approve-workflows",
        ],
        "cursor-origin" => &["merge", "ready", "draft", "close", "reopen"],
        "gitlab" => &[
            "merge",
            "ready",
            "draft",
            "close",
            "reopen",
            "update-branch",
            "enable-auto-merge",
            "disable-auto-merge",
            "revert",
            "delete-branch",
        ],
        "gitcafe" => &[
            "ready",
            "draft",
            "close",
            "reopen",
            "merge",
            "update-branch",
        ],
        "tanstack-source" => &[
            "merge",
            "ready",
            "draft",
            "close",
            "reopen",
            "update-branch",
            "enable-auto-merge",
            "disable-auto-merge",
            "revert",
        ],
        "forgejo" => &["merge", "close", "reopen", "update-branch"],
        "bitbucket" => &["merge", "close"],
        "azure-devops" => &[
            "merge",
            "ready",
            "draft",
            "close",
            "reopen",
            "enable-auto-merge",
            "disable-auto-merge",
        ],
        _ => &[],
    }
}

fn default_host(host: &str) -> Option<(&'static str, String)> {
    let (kind, base) = match host {
        "github.com" => ("github", "https://api.github.com".into()),
        "gitlab.com" => ("gitlab", "https://gitlab.com/api/v4".into()),
        "codeberg.org" => ("forgejo", "https://codeberg.org/api/v1".into()),
        "bitbucket.org" => ("bitbucket", "https://api.bitbucket.org/2.0".into()),
        "git.cafe" | "staging.git.cafe" => ("gitcafe", format!("https://{host}/api")),
        "origin.cursor.com" => ("cursor-origin", "https://api.cursor.com/v1/origin".into()),
        "source.tanstack.com" => (
            "tanstack-source",
            "https://source.tanstack.com/api/v1".into(),
        ),
        "dev.azure.com" | "ssh.dev.azure.com" => ("azure-devops", "https://dev.azure.com".into()),
        s if s.ends_with(".ghe.com") => ("github", format!("https://{s}/api/v3")),
        s if s.ends_with(".visualstudio.com") => ("azure-devops", format!("https://{s}")),
        _ => return None,
    };
    Some((kind, base))
}
fn remote_identity(remote: &str) -> Result<(String, String), ForgeError> {
    let (host, path) = if remote.contains("://") {
        let url = Url::parse(remote).map_err(|_| ForgeError::bad("invalid origin URL"))?;
        if !matches!(url.scheme(), "https" | "http" | "ssh" | "git") {
            return Err(ForgeError::unsupported(
                "origin does not identify a network forge",
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| ForgeError::bad("origin does not have a host"))?;
        let authority = match url.port() {
            Some(port) if !(url.scheme() == "ssh" && port == 22) => format!("{host}:{port}"),
            _ => host.into(),
        };
        let components = url
            .path()
            .trim_matches('/')
            .split('/')
            .map(|s| {
                let decoded = percent_encoding::percent_decode_str(s)
                    .decode_utf8()
                    .map_err(|_| ForgeError::bad("origin path is not UTF-8"))?;
                if decoded.contains('/') {
                    return Err(ForgeError::bad(
                        "encoded slash changes the repository identity",
                    ));
                }
                Ok(decoded)
            })
            .collect::<Result<Vec<_>, ForgeError>>()?;
        (authority, components.join("/"))
    } else {
        let (authority, path) = remote
            .split_once(':')
            .ok_or_else(|| ForgeError::unsupported("origin does not identify a network forge"))?;
        (
            authority
                .rsplit('@')
                .next()
                .unwrap_or(authority)
                .to_ascii_lowercase(),
            path.trim_matches('/').to_owned(),
        )
    };
    let path = path.strip_suffix(".git").unwrap_or(&path);
    if host.is_empty()
        || path.is_empty()
        || path.split('/').any(|s| {
            matches!(s, "" | "." | "..")
                || s.contains(['?', '#', '\\'])
                || s.chars().any(char::is_control)
        })
    {
        return Err(ForgeError::bad("invalid repository identity"));
    }
    Ok((host.to_ascii_lowercase(), path.into()))
}

impl Context {
    fn base(base: &str) -> Result<Url, ForgeError> {
        let api_base =
            Url::parse(base).map_err(|_| ForgeError::bad("invalid host API base URL"))?;
        if !(api_base.scheme() == "https"
            || (api_base.scheme() == "http"
                && api_base.host_str().is_some_and(|h| {
                    h == "localhost"
                        || h.parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                })))
            || !api_base.username().is_empty()
            || api_base.password().is_some()
            || api_base.query().is_some()
            || api_base.fragment().is_some()
        {
            return Err(ForgeError::bad(
                "forge API requires HTTPS or a loopback fixture",
            ));
        }
        Ok(api_base)
    }
    fn client() -> Result<Client, ForgeError> {
        let client = Client::builder()
            .user_agent("Kar")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .map_err(Error::Http)?;
        Ok(client)
    }
    async fn for_operation(
        app: Workbench,
        project: Uuid,
        op: &Operation,
    ) -> Result<Self, ForgeError> {
        if matches!(
            op,
            Operation::Issues | Operation::Issue { .. } | Operation::SetIssueState { .. }
        ) {
            providers::linear::tracker(app, project).await
        } else {
            Self::open(app, project).await
        }
    }
    async fn open(app: Workbench, project: Uuid) -> Result<Self, ForgeError> {
        tokio::task::spawn_blocking(move || {
            let repo = app.repository(project)?;
            let remote = crate::workbench_git::git(repo.root(), &["remote", "get-url", "origin"])?;
            let (host, repository) = remote_identity(
                std::str::from_utf8(&remote)
                    .map_err(|_| ForgeError::bad("origin is not UTF-8"))?
                    .trim(),
            )?;
            let config = Config::load()?;
            let (kind, base, key) = if let Some(h) = config
                .hosts
                .into_iter()
                .find(|h| h.host.eq_ignore_ascii_case(&host))
            {
                (h.kind, h.api_base, h.token_env)
            } else {
                let (kind, base) = default_host(&host).ok_or_else(|| {
                    ForgeError::unsupported(
                        "unknown forge host; register it in the host configuration",
                    )
                })?;
                let key = match host.as_str() {
                    "github.com" => Some("GH_TOKEN".to_owned()),
                    "gitlab.com" => Some("GITLAB_TOKEN".to_owned()),
                    "git.cafe" => Some("CAFE_TOKEN".to_owned()),
                    "source.tanstack.com" => Some("SRC_TOKEN".to_owned()),
                    "origin.cursor.com" => Some("KAR_CURSOR_ORIGIN_TOKEN".to_owned()),
                    "bitbucket.org" => Some("KAR_BITBUCKET_TOKEN".to_owned()),
                    "codeberg.org" => Some("KAR_FORGEJO_TOKEN".to_owned()),
                    "dev.azure.com" | "ssh.dev.azure.com" => {
                        Some("AZURE_DEVOPS_EXT_PAT".to_owned())
                    }
                    _ => None,
                };
                (kind.into(), base, key)
            };
            if !DRIVER_KINDS.contains(&kind.as_str()) {
                return Err(ForgeError::bad("unknown configured driver"));
            }
            if kind == "tanstack-source"
                && std::env::var("KAR_TANSTACK_SOURCE").as_deref() != Ok("1")
            {
                return Err(ForgeError::unsupported(
                    "TanStack Source requires KAR_TANSTACK_SOURCE=1",
                ));
            }
            let api_base = Self::base(&base)?;
            let token = key
                .and_then(|key| std::env::var(key).ok())
                .filter(|s| !s.is_empty());
            let client = Self::client()?;
            Ok(Self {
                kind,
                host,
                repository,
                api_base,
                token,
                client,
                cwd: repo.root().into(),
            })
        })
        .await
        .map_err(|e| ForgeError::new("workbench", 500, e.to_string()))?
    }
    fn identity(&self) -> String {
        format!("{}://{}/{}", self.kind, self.host, self.repository)
    }
    async fn execute(&self, op: &Operation) -> Result<Value, ForgeError> {
        if self.kind == "github" {
            github::execute(self, op).await
        } else {
            providers::execute(self, op).await
        }
    }
}

fn checked_number(value: &Value) -> Result<u64, ForgeError> {
    value["number"].as_u64().filter(|n| *n > 0).ok_or_else(|| {
        ForgeError::new(
            "invalid_provider_response",
            502,
            "provider reply lacks a positive number",
        )
    })
}
