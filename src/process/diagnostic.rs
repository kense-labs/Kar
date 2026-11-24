use regex::Regex;
use std::sync::LazyLock;

/// Sanitize the complete native output before selecting a line or shortening it.
fn sanitized(raw: &str, secrets: &[String]) -> Option<String> {
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
        let escaped = serde_json::to_string(secret).ok()?;
        forms.push(escaped[1..escaped.len() - 1].to_owned());
        forms.push(percent_encoding::utf8_percent_encode(secret, URI).to_string());
        forms.push(secret.to_owned());
    }
    forms.sort_unstable_by_key(|s| std::cmp::Reverse(s.len()));
    forms.dedup();
    for s in forms.iter().filter(|s| !s.is_empty()) {
        text = text.replace(s, "[redacted]");
    }
    static SHAPES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\b(?:[\w.-]*(?:key|token|secret|password|passwd|credential)s?|authorization|proxy-authorization|(?:set-)?cookie)["']?\s*[:=][^\r\n]*|\b(?:bearer|basic)\s+[\w+/=.\-]+|\b[a-z][a-z\d+.\-]*://[^\s<>"']+|\b(?:gh[pousr]_|github_pat_|AIza|ya29\.)[\w.\-]+|\bsk-[\w\-]{16,}|\beyJ[\w-]+\.[\w-]+\.[\w-]+"#).unwrap()
    });
    Some(SHAPES.replace_all(&text, "[redacted]").into_owned())
}

pub(crate) fn excerpt(raw: &str, secrets: &[String]) -> Option<String> {
    let redacted = sanitized(raw, secrets)?;
    let line = redacted.lines().map(str::trim).find(|s| !s.is_empty())?;
    let mut chars = line.chars();
    let mut result: String = chars.by_ref().take(500).collect();
    if chars.next().is_some() {
        result.push_str("...");
    }
    Some(result)
}

/// Keep bounded startup diagnostics in memory. Continue draining oversized
/// stderr so the native process can finish without blocking on a full pipe.
pub(crate) async fn startup_tail(
    mut pipe: tokio::process::ChildStderr,
    secrets: Vec<String>,
) -> Option<String> {
    use tokio::io::AsyncReadExt;
    const LIMIT: u64 = 128 * 1024;
    let mut bytes = Vec::new();
    (&mut pipe)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() as u64 > LIMIT {
        drop(bytes);
        let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
        None
    } else {
        let text = sanitized(std::str::from_utf8(&bytes).ok()?, &secrets)?;
        let text = text.trim_end();
        if text.is_empty() {
            return None;
        }
        let start = text.char_indices().rev().nth(2047).map_or(0, |(i, _)| i);
        Some(text[start..].to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_diagnostics_redact_known_values_and_token_shapes_before_shortening() {
        let source = "startup\nprivate-argument user@example.test Bearer unused-credential\nOPENAI_API_KEY=more-private\nmissing session";
        let value = sanitized(
            source,
            &["private-argument".into(), "user@example.test".into()],
        )
        .unwrap();
        for secret in [
            "private-argument",
            "user@example.test",
            "unused-credential",
            "more-private",
        ] {
            assert!(!value.contains(secret));
        }
        assert!(value.ends_with("missing session"));
        assert_eq!(excerpt(source, &[]).as_deref(), Some("startup"));
    }
}
