use super::{issue::child, *};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use uuid::Uuid;

mod refs;
const TEXT_LIMIT: usize = 1024 * 1024;
#[allow(clippy::enum_variant_names)]
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Change {
    Change,
    RenamePure,
    RenameChanged,
    New,
    Deleted,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::workbench::forge) struct Request {
    commit: Option<String>,
    change_type: Change,
    old_path: String,
    new_path: String,
}
fn valid_path(path: &str) -> bool {
    path.len() <= 4096
        && !path.contains('\0')
        && path
            .split('/')
            .all(|s| !s.is_empty() && !matches!(s, "." | ".."))
}
fn valid_ref(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn oid(v: &Value) -> Result<&str, ForgeError> {
    v.as_str()
        .filter(|s| valid_ref(s))
        .ok_or_else(|| invalid("immutable diff revision"))
}
struct Sides {
    old: Option<String>,
    new: Option<String>,
    head_repo: Option<String>,
}
impl Sides {
    fn pair(base: &Value, head: &Value) -> Result<Self, ForgeError> {
        Ok(Self {
            old: Some(oid(base)?.into()),
            new: Some(oid(head)?.into()),
            head_repo: None,
        })
    }
}
async fn file(c: &Context, commit: Option<&str>, path: &str) -> Result<String, ForgeError> {
    let Some(commit) = commit else {
        return Ok(String::new());
    };
    let repo = match c.kind.as_str() {
        "azure-devops" => azure::git_endpoint(c, &[])?,
        "gitcafe" | "tanstack-source" => hosted::resource(c, None, None)?,
        _ => rest::project(c)?,
    };
    if matches!(c.kind.as_str(), "github" | "gitlab") {
        let (mut url, accept) = if c.kind == "gitlab" {
            (
                child(repo, &["repository", "files", path, "raw"]),
                "application/octet-stream",
            )
        } else {
            let contents = child(repo, &["contents"]);
            (
                child(contents, &path.split('/').collect::<Vec<_>>()),
                "application/vnd.github.raw+json",
            )
        };
        url.query_pairs_mut().append_pair("ref", commit);
        let res = response(c, Method::GET, url, None, false, Some(accept)).await?;
        let (data, _) = read_bytes(res, false, TEXT_LIMIT).await?;
        if data.contains(&0) {
            return Err(ForgeError::unsupported("binary diff file contents"));
        }
        return String::from_utf8(data)
            .map_err(|_| ForgeError::unsupported("binary diff file contents"));
    }
    let mut u = match c.kind.as_str() {
        "azure-devops" => child(repo, &["items"]),
        "gitcafe" => child(repo, &["blob"]),
        _ => child(
            child(repo, &["contents"]),
            &path.split('/').collect::<Vec<_>>(),
        ),
    };
    match c.kind.as_str() {
        "azure-devops" => {
            u.query_pairs_mut().extend_pairs([
                ("path", &format!("/{path}")[..]),
                ("versionDescriptor.version", commit),
                ("versionDescriptor.versionType", "commit"),
                ("includeContent", "true"),
                ("includeContentMetadata", "true"),
                ("$format", "json"),
            ]);
        }
        "gitcafe" => {
            u.query_pairs_mut()
                .extend_pairs([("path", path), ("oid", commit)]);
        }
        _ => {
            u.query_pairs_mut().append_pair("ref", commit);
        }
    }
    let data = get(c, u).await?;
    match c.kind.as_str() {
        "azure-devops" => Ok(data["content"].as_str().unwrap_or_default().to_owned()),
        "forgejo" => {
            if data["encoding"] != "base64" {
                return Err(invalid("Forgejo contents encoding"));
            }
            let content = data["content"]
                .as_str()
                .ok_or_else(|| invalid("Forgejo contents"))?;
            let bytes = STANDARD
                .decode(
                    content
                        .bytes()
                        .filter(|b| !b.is_ascii_whitespace())
                        .collect::<Vec<_>>(),
                )
                .map_err(|_| invalid("Forgejo base64 contents"))?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
        "gitcafe" => {
            if data["commit"] != commit || data["path"] != path {
                return Err(invalid("GitCafe blob revision or path"));
            }
            let size = data["size"]
                .as_u64()
                .ok_or_else(|| invalid("GitCafe blob size"))?;
            if data["binary"] == true || size > TEXT_LIMIT as u64 {
                return Err(ForgeError::unsupported("binary or oversized GitCafe blob"));
            }
            data["content"]
                .as_str()
                .filter(|s| !s.contains('\0') && s.len() <= TEXT_LIMIT)
                .map(str::to_owned)
                .ok_or_else(|| ForgeError::unsupported("GitCafe text blob unavailable"))
        }
        "tanstack-source" if data["binary"] != true && data["truncated"] != true => data["content"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ForgeError::unsupported("Source text unavailable")),
        _ => Err(ForgeError::unsupported("binary or truncated diff content")),
    }
}
pub(in crate::workbench::forge) async fn read(
    State(app): State<crate::workbench::Workbench>,
    headers: HeaderMap,
    Path((p, n)): Path<(Uuid, u64)>,
    Json(input): Json<Request>,
) -> Result<Json<Value>, ForgeError> {
    if n == 0
        || !valid_path(&input.old_path) && input.change_type != Change::New
        || !valid_path(&input.new_path) && input.change_type != Change::Deleted
    {
        return Err(ForgeError::bad(
            "expected a pull number and repository-relative paths",
        ));
    }
    let mut c = Context::open(app, p).await?;
    if !matches!(
        c.kind.as_str(),
        "github" | "gitlab" | "forgejo" | "azure-devops" | "gitcafe" | "tanstack-source"
    ) {
        return Err(ForgeError::unsupported(
            "host cannot expand unchanged pull request lines",
        ));
    }
    if let Some(t) = credentials::resolve(&c).await? {
        c.token = Some(t);
    }
    super::super::routing::verify(&c, super::super::routing::headers(&headers)?.as_ref()).await?;
    let sides = refs::resolve(&c, n, &input).await?;
    let head = Context {
        repository: sides.head_repo.unwrap_or_else(|| c.repository.clone()),
        ..c.clone()
    };
    let base_ref = sides
        .old
        .as_deref()
        .filter(|_| input.change_type != Change::New);
    let head_ref = sides
        .new
        .as_deref()
        .filter(|_| input.change_type != Change::Deleted);
    let (old, new) = tokio::try_join!(
        file(&c, base_ref, &input.old_path),
        file(&head, head_ref, &input.new_path)
    )?;
    Ok(Json(json!({"oldContents":old,"newContents":new})))
}
