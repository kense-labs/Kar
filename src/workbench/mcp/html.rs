//! Prepare chat-bound HTML attachments from inline text or a granted workspace file.
use super::Workbench;
use crate::{
    Error, Result,
    chat::AttachmentKind,
    workbench::{files::Workspace, terminal_preview::html_browser},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};
use uuid::Uuid;
const MAX_PAGE: usize = 25 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    html: Option<String>,
    path: Option<PathBuf>,
    width: Option<u32>,
    appearance: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Render {
    html: Option<String>,
    path: Option<PathBuf>,
    height: u32,
    title: String,
}
fn refuse(s: &str) -> Error {
    Error::Refused(s.into())
}
fn read(work: &Workspace, cwd: &Path, path: &Path, max: usize) -> Result<Vec<u8>> {
    let rel = path
        .strip_prefix(cwd)
        .map_err(|_| refuse("HTML file or image is outside this chat's workspace"))?;
    let resolved = work.target(rel, false)?;
    work.read_bytes(&resolved, max)
}
fn mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF89a") || bytes.starts_with(b"GIF87a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}
fn prepare(
    app: &Workbench,
    cwd: &Path,
    html: Option<String>,
    path: Option<PathBuf>,
    files: bool,
) -> Result<(String, Vec<String>)> {
    let work = files.then(|| app.workspace_at(cwd)).transpose()?;
    let source = match (html, path) {
        (Some(s), None) if !s.trim().is_empty() && s.chars().count() <= 512000 => s,
        (None, Some(path)) => {
            let w = work
                .as_ref()
                .ok_or_else(|| refuse("use inline HTML or grant workspace file reads"))?;
            if !path.is_absolute()
                || !path.extension().is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm")
                })
            {
                return Err(refuse("expected an absolute .html path in the workspace"));
            }
            let s = String::from_utf8(read(w, cwd, &path, MAX_PAGE)?)
                .map_err(|_| refuse("HTML file must contain UTF-8"))?;
            let first = s
                .trim_start_matches('\u{feff}')
                .trim_start()
                .to_ascii_lowercase();
            if !(first.starts_with("<!doctype html") || first.starts_with("<html")) {
                return Err(refuse("file does not start with an HTML document"));
            }
            s
        }
        _ => {
            return Err(refuse(
                "pass exactly one nonempty HTML source or a workspace .html path",
            ));
        }
    };
    // Only image attributes and CSS image URLs are eligible for local-file expansion.
    static IMAGES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:<img\b[^>]*?\bsrc\s*=\s*["']|url\(\s*["']?)((?:file:///|/)[^"'()<>\r\n]+)"#,
        )
        .unwrap()
    });
    let mut page = String::new();
    let mut copied = 0;
    let mut missing = Vec::new();
    for capture in IMAGES.captures_iter(&source) {
        let span = capture.get(1).unwrap();
        let reference = span.as_str().trim();
        if reference.starts_with("//") {
            continue;
        }
        let path = if reference.starts_with("file:") {
            reqwest::Url::parse(reference)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .ok_or_else(|| refuse("invalid local image path"))?
        } else {
            PathBuf::from(reference)
        };
        let work = work.as_ref().ok_or_else(|| {
            refuse("local images require the workspace file grant; use a data URI for inline HTML")
        })?;
        let data = match read(work, cwd, &path, 10 * 1024 * 1024) {
            Ok(bytes) => bytes,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(reference.to_owned());
                continue;
            }
            Err(e) => return Err(e),
        };
        let ty = mime(&data)
            .ok_or_else(|| refuse("local HTML assets must be PNG, JPEG, GIF or WebP images"))?;
        let encoded = 4 * data.len().div_ceil(3);
        if page.len() + span.start() - copied + encoded + 64 > MAX_PAGE {
            return Err(refuse("HTML with images exceeds 25 MiB"));
        }
        page.push_str(&source[copied..span.start()]);
        page.push_str(&format!("data:{ty};base64,"));
        STANDARD.encode_string(data, &mut page);
        copied = span.end();
    }
    if page.len() + source.len() - copied > MAX_PAGE {
        return Err(refuse("HTML with images exceeds 25 MiB"));
    }
    page.push_str(&source[copied..]);
    Ok((page, missing))
}
pub(super) async fn call(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
    name: &str,
    input: Value,
    files: bool,
    check: impl Fn() -> Result<()>,
) -> Result<Value> {
    let cwd = app.binding(project, chat)?.workspace;
    if name == "kar_html_preview" {
        let q: Preview = serde_json::from_value(input)?;
        let width = q.width.unwrap_or(728);
        if !(240..=1600).contains(&width) {
            return Err(refuse(
                "preview width must be between 240 and 1600 CSS pixels",
            ));
        }
        let dark = match q.appearance.as_deref() {
            None | Some("dark") => true,
            Some("light") => false,
            _ => return Err(refuse("appearance must be dark or light")),
        };
        let (html, missing) = prepare(app, &cwd, q.html, q.path, files)?;
        let mut view = html_browser::capture(&html, &[width], dark, true).await?;
        check()?;
        if !missing.is_empty() {
            view["missingImages"] = json!(missing);
        }
        return Ok(view);
    }
    let q: Render = serde_json::from_value(input)?;
    let title = q.title.trim();
    if title.is_empty() || title.chars().count() > 200 || !(80..=2000).contains(&q.height) {
        return Err(refuse(
            "supply a title of 1–200 characters and height of 80–2000 CSS pixels",
        ));
    }
    let (html, missing) = prepare(app, &cwd, q.html, q.path, files)?;
    if !missing.is_empty() {
        return Err(refuse(&format!(
            "missing local images: {}",
            missing.join(", ")
        )));
    }
    let heights = html_browser::capture(
        &html,
        &[320, 375, 430, 520, 640, 728, 860, 1000, 1144],
        true,
        false,
    )
    .await;
    check()?;
    let id = Uuid::new_v4();
    let a = app.0.chats.persist_attachment(
        chat,
        id,
        "kar-render.html",
        "text/html",
        AttachmentKind::File,
        html.as_bytes(),
    )?;
    let mut view = json!({"attachmentId":a.id,"title":title,"height":q.height,"view_url_endpoint":format!("/projects/{project}/chats/{chat}/html-renders/{id}/url")});
    match heights {
        Ok(v) => view["heights"] = v,
        Err(e) => view["measurement_error"] = json!(e.to_string()),
    };
    Ok(json!({"htmlRender":view,"message":"HTML is available in this chat."}))
}
