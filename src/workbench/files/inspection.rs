//! Directory pickers, file metadata and Git ignore decisions for workbench clients.
mod search;
use super::*;
use crate::workbench::http::{ApiError, blocking};
use axum::{
    Json, Router,
    extract::{Path as Param, Query, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{project}/entries", get(entries))
        .route("/projects/{project}/ignored-files", get(ignored_files))
        .route("/projects/{project}/browse", get(browse))
        .route("/projects/{project}/search/entries", get(search::entries))
        .route("/projects/{project}/search/contents", get(search::contents))
        .route("/filesystem/metadata", post(metadata))
}
type Reply = std::result::Result<Json<Value>, ApiError>;
#[derive(Serialize)]
struct Entry {
    #[serde(with = "crate::chat::checkpoint::git_path")]
    path: PathBuf,
    kind: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    ignored: bool,
}
impl Workspace {
    fn directory(&self, relative: &Path) -> Result<(PathBuf, Dir)> {
        if relative == Path::new("") || relative == Path::new(".") {
            return Ok((PathBuf::new(), self.dir.try_clone()?));
        }
        let real = self.target(relative, false)?;
        let dir = self
            .parent(&real, false)?
            .open_dir_nofollow(real.file_name().ok_or_else(denied)?)?;
        Ok((real, dir))
    }
    fn entry_kind(&self, path: &Path, ancestors: &HashSet<PathBuf>) -> Option<&'static str> {
        let real = self.target(path, false).ok()?;
        let stat = self
            .parent(&real, false)
            .ok()?
            .symlink_metadata(real.file_name()?)
            .ok()?;
        if stat.is_file() {
            return Some("file");
        }
        if stat.is_dir() && !ancestors.contains(&self.root.join(real)) {
            return Some("directory");
        }
        None
    }
    fn entries_in(&self, requested: &Path) -> Result<Vec<Entry>> {
        let (real, dir) = self.directory(requested)?;
        // A link back to any folder on the traversal path would recur on every expansion.
        let ancestors = requested
            .ancestors()
            .map(|p| self.root.join(p).canonicalize())
            .collect::<std::io::Result<HashSet<_>>>()?;
        let mut entries = Vec::new();
        for child in dir.entries()? {
            let child = child?;
            let path = requested.join(child.file_name());
            if let Some(kind) = self.entry_kind(&path, &ancestors) {
                entries.push(Entry {
                    path,
                    kind,
                    ignored: false,
                });
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let folder = self.root.join(&real);
        // Using the listed folder as cwd also selects the correct repository in a submodule.
        let raw = crate::workbench_git::ignored_names(
            &folder,
            entries.iter().filter_map(|e| e.path.file_name()),
        )
        .unwrap_or_default();
        let names: HashSet<_> = raw
            .split(|b| *b == 0)
            .map(|n| n.strip_prefix(b"./").unwrap_or(n))
            .collect();
        for e in &mut entries {
            e.ignored = e
                .path
                .file_name()
                .is_some_and(|n| names.contains(n.as_encoded_bytes()));
        }
        Ok(entries)
    }

    fn indexed(&self) -> Result<(Vec<Entry>, bool)> {
        let mut entries = Vec::new();
        self.visit_indexed(|e| {
            entries.push(e);
            Ok(entries.len() <= 25_000)
        })?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let truncated = entries.len() > 25_000;
        entries.truncate(25_000);
        Ok((entries, truncated))
    }

    fn visit_indexed(&self, mut visit: impl FnMut(Entry) -> Result<bool>) -> Result<()> {
        let private = self.private.clone();
        let mut builder = ignore::WalkBuilder::new(&self.root);
        builder
            .hidden(false)
            .follow_links(false)
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(move |e| {
                !e.file_name().eq_ignore_ascii_case(".git") && !e.path().starts_with(&private)
            });
        for entry in builder.build() {
            let entry = entry.map_err(|e| Error::Protocol(e.to_string()))?;
            let path = entry
                .path()
                .strip_prefix(&self.root)
                .expect("workspace walk");
            if path.as_os_str().is_empty() {
                continue;
            }
            let kind = match entry.file_type() {
                Some(t) if t.is_dir() => "directory",
                Some(t) if t.is_file() => "file",
                Some(t) if t.is_symlink() => {
                    let ancestors = entry
                        .path()
                        .ancestors()
                        .skip(1)
                        .map(Path::to_owned)
                        .collect();
                    let Some(kind) = self.entry_kind(path, &ancestors) else {
                        continue;
                    };
                    kind
                }
                _ => continue,
            };
            if !visit(Entry {
                path: path.into(),
                kind,
                ignored: false,
            })? {
                break;
            }
        }
        Ok(())
    }

    fn ignored(&self, query: &str) -> Result<Value> {
        if query.chars().count() > 256 {
            return Err(Error::Refused(
                "ignored-file query exceeds 256 characters".into(),
            ));
        }
        let bytes = crate::workbench_git::git(
            &self.root,
            &[
                "-c",
                "core.fsmonitor=false",
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "--no-empty-directory",
                "-z",
            ],
        )
        .unwrap_or_default();
        let q = query.to_lowercase();
        let mut paths = Vec::new();
        for name in bytes
            .split(|b| *b == 0)
            .filter(|n| !n.is_empty() && !n.ends_with(b"/"))
        {
            let path = gix::path::try_from_byte_slice(name)
                .map_err(|_| Error::Protocol("Git returned an invalid path".into()))?;
            if path.to_string_lossy().to_lowercase().contains(&q)
                && !self.root.join(path).starts_with(&self.private)
            {
                paths.push(path.to_path_buf());
            }
        }
        paths.sort();
        let truncated = paths.len() > 200;
        let mut files = Vec::new();
        for path in paths.into_iter().take(200) {
            let Some(name) = path.file_name() else {
                continue;
            };
            // The listing is untrusted until the anchored no-follow metadata read succeeds.
            let Ok(parent) = self.parent(&checked(&path)?, false) else {
                continue;
            };
            if let Ok(m) = parent.symlink_metadata(name)
                && m.is_file()
            {
                let path = crate::chat::checkpoint::git_path::serialize(
                    &path,
                    serde_json::value::Serializer,
                )?;
                files.push(json!({"path":path,"byte_length":m.len()}));
            }
        }
        Ok(json!({"files":files,"truncated":truncated}))
    }
}

#[derive(Deserialize)]
struct List {
    directory: Option<PathBuf>,
}
async fn entries(
    State(app): State<Workbench>,
    Param(id): Param<Uuid>,
    Query(q): Query<List>,
) -> Reply {
    blocking(move || {
        let ws = app.workspace(&app.project(id)?)?;
        let (entries, truncated) = match q.directory {
            Some(p) => (ws.entries_in(&p)?, false),
            None => ws.indexed()?,
        };
        Ok(json!({"entries":entries,"truncated":truncated}))
    })
    .await
}
#[derive(Deserialize, Default)]
struct Filter {
    #[serde(default)]
    query: String,
}
async fn ignored_files(
    State(app): State<Workbench>,
    Param(id): Param<Uuid>,
    Query(q): Query<Filter>,
) -> Reply {
    blocking(move || app.workspace(&app.project(id)?)?.ignored(&q.query)).await
}
#[derive(Deserialize)]
struct Browse {
    partial_path: String,
}
fn expanded(path: &Path) -> Result<PathBuf> {
    if let Ok(rest) = path.strip_prefix("~") {
        Ok(std::env::home_dir().ok_or_else(denied)?.join(rest))
    } else {
        Ok(path.to_owned())
    }
}
async fn browse(
    State(app): State<Workbench>,
    Param(id): Param<Uuid>,
    Query(q): Query<Browse>,
) -> Reply {
    blocking(move || {
        if q.partial_path.trim().is_empty() || q.partial_path.chars().count() > 512 {
            return Err(Error::Refused("invalid directory prefix".into()));
        }
        let project = app.project(id)?;
        let is_dir = q.partial_path.ends_with('/') || q.partial_path == "~";
        let path = project.root.join(expanded(Path::new(&q.partial_path))?);
        let (parent, prefix) = if is_dir {
            (path.as_path(), String::new())
        } else {
            (
                path.parent().ok_or_else(denied)?,
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase(),
            )
        };
        app.allowed(parent)?;
        let real = parent.canonicalize()?;
        let ws = app.workspace_at(&real)?;
        let mut entries = BTreeMap::new();
        for child in ws.dir.entries()? {
            let child = child?;
            let name = child.file_name();
            let lower = name.to_string_lossy().to_lowercase();
            if !lower.starts_with(&prefix)
                || lower.starts_with('.') && !is_dir && !prefix.starts_with('.')
            {
                continue;
            }
            let child_path = real.join(&name);
            if app.allowed(&child_path).is_ok()
                && let Ok(target) = child_path.canonicalize()
                && app.allowed(&target).is_ok()
                && target.is_dir()
            {
                entries.insert(name.to_string_lossy().into_owned(), parent.join(name));
            }
        }
        let entries: Vec<_> = entries
            .into_iter()
            .map(|(name, full_path)| json!({"name":name,"full_path":full_path}))
            .collect();
        Ok(json!({"parent_path":parent,"entries":entries}))
    })
    .await
}
#[derive(Deserialize)]
struct Metadata {
    paths: Vec<PathBuf>,
}
fn file_metadata(app: &Workbench, path: &Path) -> Result<Value> {
    let path = expanded(path)?;
    if !path.is_absolute()
        || path.starts_with(app.0.chats.directory())
        || path
            .components()
            .any(|p| p.as_os_str().eq_ignore_ascii_case(".git"))
    {
        return Err(denied());
    }
    if app.allowed(&path).is_err() {
        let parent = path.parent().ok_or_else(denied)?.canonicalize()?;
        let candidate = match path.file_name() {
            Some(name) => parent.join(name),
            None => parent,
        };
        app.allowed(&candidate)?;
    }
    let real = path.canonicalize()?;
    let root = &app.allowed(&real)?.path;
    let ws = app.workspace_at(root)?;
    let relative = real.strip_prefix(root).expect("authorized root");
    let Some(name) = relative.file_name() else {
        return Ok(json!({"kind":"directory"}));
    };
    let meta = ws.parent(relative, false)?.symlink_metadata(name)?;
    if !meta.is_file() {
        return Ok(json!({"kind":if meta.is_dir() {"directory"} else {"other"}}));
    }
    let mut row = json!({"kind":"file","byte_length":meta.len()});
    if path.extension().is_none()
        && meta.len() > 0
        && let Ok(file) = ws.open_regular(relative)
    {
        let mut head = Vec::new();
        if file.take(512).read_to_end(&mut head).is_ok()
            && let Some(mime) = mime(&head)
        {
            row["mime_type"] = mime.into();
        }
    }
    Ok(row)
}
fn mime(b: &[u8]) -> Option<&'static str> {
    for (prefix, kind) in [
        (&b"\x89PNG\r\n\x1a\n"[..], "image/png"),
        (&b"\xff\xd8\xff"[..], "image/jpeg"),
        (&b"PK\x03\x04"[..], "application/zip"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"SQLite format 3\0", "application/vnd.sqlite3"),
    ] {
        if b.starts_with(prefix) {
            return Some(kind);
        }
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if b.starts_with(b"#!") {
        let line = std::str::from_utf8(b.split(|c| *c == b'\n').next()?).ok()?;
        for part in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '.') {
            match part {
                "sh" | "bash" | "zsh" | "fish" => return Some("text/x-shellscript"),
                "node" | "nodejs" => return Some("text/javascript"),
                "ruby" => return Some("text/x-ruby"),
                s if s
                    .strip_prefix("python")
                    .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.')) =>
                {
                    return Some("text/x-python");
                }
                _ => {}
            }
        }
    }
    None
}
async fn metadata(State(app): State<Workbench>, Json(q): Json<Metadata>) -> Reply {
    blocking(move || {
        if q.paths.is_empty()
            || q.paths.len() > 64
            || q.paths
                .iter()
                .any(|p| p.as_os_str().is_empty() || p.to_string_lossy().chars().count() > 512)
        {
            return Err(Error::Refused(
                "metadata requires 1 to 64 paths of at most 512 characters".into(),
            ));
        }
        let mut seen = HashMap::new();
        let entries: Vec<_> = q
            .paths
            .into_iter()
            .map(|path| {
                seen.entry(path.clone())
                    .or_insert_with(|| file_metadata(&app, &path).unwrap_or(Value::Null))
                    .clone()
            })
            .collect();
        Ok(json!({"entries":entries}))
    })
    .await
}
