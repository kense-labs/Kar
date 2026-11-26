//! Explicit, credential-free public web sources. Pin the checked DNS answer for every request.
use crate::{Error, Result};
use reqwest::{StatusCode, Url};
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

fn denied() -> Error {
    Error::Refused("URL outside the granted public web origins".into())
}
pub(super) fn authorize(allowed: &BTreeSet<String>, url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !allowed.contains(&url.origin().ascii_serialization())
    {
        return Err(denied());
    }
    Ok(())
}
fn public(ip: IpAddr) -> bool {
    (match ip {
        IpAddr::V4(v) => {
            let [a, b, c, _] = v.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..128).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..32).contains(&b))
                || (a == 192
                    && (b == 168 || (b == 0 && (c == 0 || c == 2)) || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(v) => {
            let [a, b, ..] = v.segments();
            (0x2000..0x4000).contains(&a)
                && a != 0x2002
                && !(a == 0x2001 && (b < 0x200 || b == 0xdb8))
                && a != 0x3fff
        }
    }) && std::net::UdpSocket::bind(SocketAddr::new(ip, 0)).is_err()
}
pub(super) async fn addresses(url: &Url) -> Result<(String, Vec<SocketAddr>)> {
    let host = url
        .host_str()
        .ok_or_else(denied)?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = url.port_or_known_default().ok_or_else(denied)?;
    let ips = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| Error::Timeout("public hostname resolution".into()))??
    .collect::<Vec<_>>();
    if ips.is_empty() || ips.iter().any(|ip| !public(ip.ip())) {
        return Err(denied());
    }
    Ok((host.to_owned(), ips))
}
pub(super) struct Resource {
    pub status: StatusCode,
    pub mime: String,
    pub bytes: Vec<u8>,
    pub url: Url,
}
pub(super) async fn get(
    allowed: &BTreeSet<String>,
    mut url: Url,
    limit: usize,
) -> Result<Resource> {
    for _ in 0..6 {
        authorize(allowed, &url)?;
        let (host, ips) = addresses(&url).await?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&host, &ips)
            .timeout(Duration::from_secs(15))
            .build()?;
        let mut r = client.get(url.clone()).send().await?;
        if r.status().is_redirection() {
            let next = r
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Protocol("public origin omitted redirect location".into()))?;
            url = url.join(next).map_err(|_| denied())?;
            continue;
        }
        let status = r.status();
        if r.content_length().is_some_and(|n| n > limit as u64) {
            return Err(Error::Refused(
                "public web resource exceeds size limit".into(),
            ));
        }
        let mime = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let mut bytes = Vec::new();
        while let Some(chunk) = r.chunk().await? {
            if chunk.len() > limit - bytes.len() {
                return Err(Error::Refused(
                    "public web resource exceeds size limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(Resource {
            status,
            mime,
            bytes,
            url,
        });
    }
    Err(Error::Refused("too many public web redirects".into()))
}
// Replace explicitly authorized resource references with verified bytes. The final attachment
// is standalone and can be opened without giving scripts ambient network or file access.
pub(super) async fn bundle(allowed: &BTreeSet<String>, html: String) -> Result<String> {
    if allowed.is_empty() {
        return Ok(html);
    }
    static REFS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)(?:<(?:img|script|source|audio|video)\b[^>]*?\bsrc\s*=\s*["']|<link\b[^>]*?\bhref\s*=\s*["']|url\(\s*["']?)((?:https?://)[^\s"'()<>]+)"#,
        )
        .expect("web asset pattern")
    });
    let refs = REFS
        .captures_iter(&html)
        .filter_map(|c| c.get(1))
        .map(|m| (m.start(), m.end()))
        .collect::<Vec<_>>();
    if refs.len() > 128 {
        return Err(Error::Refused("too many external asset references".into()));
    }
    let mut page = String::new();
    let mut at = 0;
    let mut cache = std::collections::HashMap::new();
    let max = 25 * 1024 * 1024;
    for (lo, hi) in refs {
        let raw = &html[lo..hi];
        let Ok(url) = Url::parse(raw) else {
            continue;
        };
        if authorize(allowed, &url).is_err() {
            continue;
        }
        if !cache.contains_key(raw) {
            let asset = get(allowed, url, (max - page.len()).min(10 * 1024 * 1024)).await?;
            let mime = asset
                .mime
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !asset.status.is_success() {
                return Err(Error::Refused(format!(
                    "public asset returned {}",
                    asset.status
                )));
            }
            if !(mime.starts_with("image/")
                || mime.starts_with("font/")
                || matches!(
                    mime.as_str(),
                    "application/javascript"
                        | "text/javascript"
                        | "text/css"
                        | "application/font-woff"
                        | "audio/mpeg"
                        | "video/webm"
                ))
            {
                continue;
            }
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            let data = format!("data:{mime};base64,{}", STANDARD.encode(asset.bytes));
            cache.insert(raw, data);
        }
        let data = &cache[raw];
        if page.len() + lo - at + data.len() > max {
            return Err(Error::Refused("HTML with assets exceeds 25 MiB".into()));
        }
        page.push_str(&html[at..lo]);
        page.push_str(data);
        at = hi;
    }
    if page.len() + html.len() - at > max {
        return Err(Error::Refused("HTML with assets exceeds 25 MiB".into()));
    }
    page.push_str(&html[at..]);
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_origins_exclude_local_addresses_and_hidden_credentials() {
        for ip in [
            "127.0.0.1",
            "192.168.42.1",
            "100.64.0.2",
            "169.254.169.254",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
            "2001:db8::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        let allow = BTreeSet::from(["https://example.org".to_string()]);
        for raw in [
            "http://example.org",
            "https://example.org.example.net/",
            "https://user@example.org/",
            "https://example.org:444/",
            "file:///etc/hosts",
        ] {
            assert!(
                authorize(&allow, &Url::parse(raw).unwrap()).is_err(),
                "{raw}"
            );
        }
    }
}
