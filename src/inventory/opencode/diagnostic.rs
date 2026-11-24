use crate::{SessionOptions, installation::Probe};
use regex::Regex;
use std::{io::SeekFrom, sync::LazyLock};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
};

/// Drop oversized stderr as a whole. Truncating before redaction could expose part of a credential.
pub(super) async fn excerpt(f: &std::fs::File, probe: &Probe) -> Option<String> {
    let mut f = File::from_std(f.try_clone().ok()?);
    f.seek(SeekFrom::Start(0)).await.ok()?;
    const LIMIT: u64 = 128 * 1024;
    let mut bytes = Vec::new();
    f.take(LIMIT + 1).read_to_end(&mut bytes).await.ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    let raw = std::str::from_utf8(&bytes).ok()?;
    static ESCAPES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap()
    });
    let mut text = ESCAPES.replace_all(raw, "").into_owned();
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return None;
    }
    let mut opts = SessionOptions::new(&probe.cwd);
    opts.env.clone_from(&probe.env);
    let mut secrets = crate::options::session_secrets(&opts);
    for (key, value) in &probe.env {
        let key = key.to_string_lossy().to_ascii_uppercase();
        let words = key.split(|c: char| !c.is_ascii_alphanumeric());
        if words.into_iter().any(|w| {
            matches!(
                w,
                "KEY"
                    | "APIKEY"
                    | "TOKEN"
                    | "PASSWORD"
                    | "PASSWD"
                    | "PASSPHRASE"
                    | "CREDENTIAL"
                    | "CREDENTIALS"
                    | "COOKIE"
                    | "SECRET"
                    | "AUTHORIZATION"
            )
        }) && let Some(value) = value.to_str().filter(|v| !v.is_empty())
        {
            secrets.push(value.to_owned());
            if let Some((_, auth)) = value.split_once(' ')
                && key.ends_with("AUTHORIZATION")
                && !auth.is_empty()
            {
                secrets.push(auth.to_owned());
            }
        }
    }
    const URI: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    let mut forms = Vec::new();
    for secret in secrets {
        let escaped = serde_json::to_string(&secret).ok()?;
        forms.push(escaped[1..escaped.len() - 1].to_owned());
        forms.push(percent_encoding::utf8_percent_encode(&secret, URI).to_string());
        forms.push(secret);
    }
    forms.sort_unstable_by_key(|s| std::cmp::Reverse(s.len()));
    forms.dedup();
    for s in forms.iter().filter(|s| !s.is_empty()) {
        text = text.replace(s, "[redacted]");
    }
    static SHAPES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\b(?:[\w.-]*(?:key|token|secret|password|passwd|credential)s?|authorization|proxy-authorization|(?:set-)?cookie)["']?\s*[:=][^\r\n]*|\b(?:bearer|basic)\s+[\w+/=.\-]+|\b[a-z][a-z\d+.\-]*://[^\s<>"']+|\b(?:gh[pousr]_|github_pat_|AIza|ya29\.)[\w.\-]+|\bsk-[\w\-]{16,}|\beyJ[\w-]+\.[\w-]+\.[\w-]+"#).unwrap()
    });
    let redacted = SHAPES.replace_all(&text, "[redacted]");
    let line = redacted.lines().map(str::trim).find(|s| !s.is_empty())?;
    let mut chars = line.chars();
    let mut result: String = chars.by_ref().take(500).collect();
    if chars.next().is_some() {
        result.push_str("...");
    }
    Some(result)
}
