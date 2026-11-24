//! Conditional responses shared by ticketed downloads and the optional web client.
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header::*},
    response::{IntoResponse, Response},
};
use cap_std::fs::Dir;
use std::{
    io,
    ops::Range,
    path::{Component, Path},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

struct Selection {
    status: StatusCode,
    bytes: Range<u64>,
    headers: HeaderMap,
}
impl Selection {
    fn new(
        method: &Method,
        request: &HeaderMap,
        size: u64,
        etag: &str,
        modified: Option<SystemTime>,
    ) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, etag.parse().expect("generated entity tag"));
        headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        let modified = modified.filter(|t| *t >= UNIX_EPOCH);
        if let Some(time) = modified {
            headers.insert(
                LAST_MODIFIED,
                httpdate::fmt_http_date(time).parse().unwrap(),
            );
        }
        let value = |name| request.get(name).and_then(|x| x.to_str().ok());
        let has_tag = request.contains_key(IF_NONE_MATCH);
        let same = value(IF_NONE_MATCH).is_some_and(|v| {
            v.split(',')
                .map(str::trim)
                .any(|t| t == "*" || t.trim_start_matches("W/") == etag.trim_start_matches("W/"))
        });
        let date_matches = |v: &str| {
            httpdate::parse_http_date(v)
                .ok()
                .zip(modified)
                .is_some_and(|(max, actual)| {
                    let seconds = |time: SystemTime| {
                        time.duration_since(UNIX_EPOCH).ok().map(|x| x.as_secs())
                    };
                    seconds(actual) <= seconds(max)
                })
        };
        if same || (!has_tag && value(IF_MODIFIED_SINCE).is_some_and(date_matches)) {
            return Self {
                status: StatusCode::NOT_MODIFIED,
                bytes: 0..0,
                headers,
            };
        }
        let allow_range = method == Method::GET
            && value(IF_RANGE)
                .is_none_or(|v| (!etag.starts_with("W/") && v == etag) || date_matches(v));
        let mut status = StatusCode::OK;
        let mut bytes = 0..size;
        if allow_range && let Some(range) = value(RANGE).and_then(|v| byte_range(v, size)) {
            match range {
                Ok(r) => {
                    status = StatusCode::PARTIAL_CONTENT;
                    headers.insert(
                        CONTENT_RANGE,
                        format!("bytes {}-{}/{size}", r.start, r.end - 1)
                            .parse()
                            .unwrap(),
                    );
                    bytes = r;
                }
                Err(()) => {
                    status = StatusCode::RANGE_NOT_SATISFIABLE;
                    headers.insert(CONTENT_RANGE, format!("bytes */{size}").parse().unwrap());
                    bytes = 0..0;
                }
            }
        }
        headers.insert(CONTENT_LENGTH, (bytes.end - bytes.start).into());
        Self {
            status,
            bytes,
            headers,
        }
    }
    fn response(self, body: Body) -> Response {
        (self.status, self.headers, body).into_response()
    }
}

fn byte_range(raw: &str, size: u64) -> Option<Result<Range<u64>, ()>> {
    let (unit, pair) = raw.trim().split_once('=')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (a, b) = pair.split_once('-')?;
    if (a.is_empty() && b.is_empty()) || !a.bytes().chain(b.bytes()).all(|x| x.is_ascii_digit()) {
        return None;
    }
    let n = |v: &str| v.parse::<u64>().unwrap_or(u64::MAX);
    let start = if a.is_empty() {
        size.saturating_sub(n(b))
    } else {
        n(a)
    };
    if !a.is_empty() && !b.is_empty() && n(b) < start {
        return None;
    }
    if start >= size {
        return Some(Err(()));
    }
    let end = if a.is_empty() || b.is_empty() {
        size
    } else {
        n(b).saturating_add(1).min(size)
    };
    Some(Ok(start..end))
}

pub(super) fn attachment(
    method: &Method,
    request: &HeaderMap,
    data: Vec<u8>,
    sha256: &str,
) -> Response {
    let part = Selection::new(
        method,
        request,
        data.len() as u64,
        &format!("\"{sha256}\""),
        None,
    );
    let body = if method == Method::HEAD || part.bytes.is_empty() {
        Body::empty()
    } else {
        Body::from(Bytes::from(data).slice(part.bytes.start as usize..part.bytes.end as usize))
    };
    part.response(body)
}

pub(super) fn static_site() -> crate::Result<Option<axum::Router>> {
    let Some(root) = std::env::var_os("KAR_STATIC_DIR") else {
        return Ok(None);
    };
    if root.is_empty() {
        return Err(crate::Error::Refused("KAR_STATIC_DIR is empty".into()));
    }
    let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    Ok(Some(
        axum::Router::new()
            .fallback(static_file)
            .with_state(Arc::new(dir)),
    ))
}
async fn static_file(
    State(dir): State<Arc<Dir>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Ok(decoded) = percent_encoding::percent_decode_str(uri.path()).decode_utf8() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path = Path::new(decoded.trim_start_matches('/'));
    if path
        .components()
        .any(|c| !matches!(c,Component::Normal(x) if !x.to_string_lossy().starts_with('.')))
        || decoded.contains(['\\', '\0'])
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut relative = path.to_owned();
    if relative.extension().is_none() {
        relative.push("index.html");
    }
    let opened = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let f = match dir.open(&relative) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                relative = "index.html".into();
                dir.open(&relative)?
            }
            Err(e) => return Err(e),
        }
        .into_std();
        let meta = f.metadata()?;
        if !meta.is_file() {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok((f, meta, relative))
    })
    .await;
    let Ok(Ok((file, meta, relative))) = opened else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mtime = meta.modified().ok();
    let version = mtime
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |v| v.as_nanos());
    let part = Selection::new(
        &method,
        &headers,
        meta.len(),
        &format!("W/\"{:x}-{version:x}\"", meta.len()),
        mtime,
    );
    let mut file = tokio::fs::File::from_std(file);
    let body = if method == Method::HEAD || part.bytes.is_empty() {
        Body::empty()
    } else {
        if part.bytes.start != 0
            && file
                .seek(io::SeekFrom::Start(part.bytes.start))
                .await
                .is_err()
        {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Body::from_stream(tokio_util::io::ReaderStream::new(
            file.take(part.bytes.end - part.bytes.start),
        ))
    };
    let mime = match relative
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        "txt" => "text/plain",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    };
    let mut response = part.response(body);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conditional_and_partial_downloads_validate_the_same_entity() {
        let mut h = HeaderMap::new();
        h.insert(RANGE, "bytes=-2".parse().unwrap());
        let select = |h: &HeaderMap| Selection::new(&Method::GET, h, 5, "\"a\"", None);
        assert_eq!(select(&h).bytes, 3..5);
        h.insert(IF_RANGE, "\"b\"".parse().unwrap());
        assert_eq!(select(&h).status, StatusCode::OK);
        h.insert(IF_RANGE, "\"a\"".parse().unwrap());
        h.insert(RANGE, "bytes=5-".parse().unwrap());
        assert_eq!(select(&h).headers[CONTENT_RANGE], "bytes */5");
        h.insert(IF_NONE_MATCH, "W/\"a\"".parse().unwrap());
        assert_eq!(select(&h).status, StatusCode::NOT_MODIFIED);
    }
}
