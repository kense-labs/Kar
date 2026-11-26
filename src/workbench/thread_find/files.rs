//! File-link parsing, visible chip formatting, and duplicate-basename disambiguation.
use super::render::SegmentBuilder;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FilePosition {
    pub(super) line: u64,
    pub(super) col: Option<u64>,
    pub(super) end_line: Option<u64>,
}

impl FilePosition {
    pub(super) fn format(self) -> String {
        if let Some(end) = self.end_line {
            if end > self.line {
                format!("L{}\u{2013}{end}", self.line)
            } else {
                format!("L{}", self.line)
            }
        } else if let Some(col) = self.col {
            format!("L{}:C{col}", self.line)
        } else {
            format!("L{}", self.line)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileTarget {
    pub(super) normalized_path: String,
    pub(super) basename: String,
    pub(super) parent_segments: Vec<String>,
    pub(super) position: Option<FilePosition>,
}

impl FileTarget {
    pub(super) fn chip_label(&self, suffixes: &HashMap<String, String>) -> String {
        let mut parts = Vec::with_capacity(3);
        parts.push(self.basename.clone());
        if let Some(suffix) = suffixes.get(&self.normalized_path) {
            parts.push(suffix.clone());
        }
        if let Some(pos) = self.position {
            parts.push(pos.format());
        }
        parts.join(" \u{00b7} ")
    }
}

const ALLOWED_POSIX_ROOTS: &[&str] = &[
    "/Users/",
    "/home/",
    "/tmp/",
    "/private/tmp/",
    "/var/",
    "/private/var/",
    "/etc/",
    "/private/etc/",
    "/opt/",
    "/Volumes/",
    "/workspace/",
    "/workspaces/",
    "/root/",
    "/mnt/",
    "/srv/",
    "/usr/",
    "/Library/",
    "/Applications/",
];

const KNOWN_EXTENSIONLESS_FILES: &[&str] = &[
    "Makefile",
    "GNUmakefile",
    "makefile",
    "Dockerfile",
    "Containerfile",
    "Procfile",
    "Brewfile",
    "Gemfile",
    "Rakefile",
    "Justfile",
    "justfile",
    "Vagrantfile",
    "Jenkinsfile",
    "Podfile",
    "Fastfile",
    "BUILD",
    "WORKSPACE",
    "README",
    "LICENSE",
    "LICENCE",
    "COPYING",
    "NOTICE",
    "AUTHORS",
    "CHANGELOG",
    "CHANGES",
    "CONTRIBUTING",
    "CODEOWNERS",
    "OWNERS",
    "TODO",
    "VERSION",
];

const MEDIA_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "avif", "apng", "tif", "tiff",
    "heic", "heif", "mp4", "webm", "mov", "m4v", "avi", "mkv", "mp3", "wav", "ogg", "oga", "m4a",
    "aac", "flac", "opus", "pdf",
];

pub(super) fn strip_ascii_case_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    debug_assert!(prefix.is_ascii());
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

pub(super) fn strip_ascii_case_suffix<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    debug_assert!(suffix.is_ascii());
    let start = s.len().checked_sub(suffix.len())?;
    s.get(start..)
        .filter(|tail| tail.eq_ignore_ascii_case(suffix))
        .map(|_| &s[..start])
}

pub(super) fn is_context_uri(url: &str) -> bool {
    let u = url.trim();
    strip_ascii_case_prefix(u, "kar-context:").is_some()
        || strip_ascii_case_prefix(u, "t3-context:").is_some()
}

pub(super) fn parse_citation_comment(url: &str) -> Option<Option<String>> {
    let u = url.trim();
    if strip_ascii_case_prefix(u, "kar-citation://v1/").is_none()
        && strip_ascii_case_prefix(u, "t3-citation://v1/").is_none()
    {
        return None;
    }
    let raw_comment = if let Ok(parsed) = url::Url::parse(u) {
        parsed
            .query_pairs()
            .find_map(|(k, v)| (k == "comment").then(|| v.into_owned()))
    } else {
        u.split_once('?')
            .map(|(_, q)| q.split('#').next().unwrap_or(q))
            .and_then(|q| {
                q.split('&').find_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    (k == "comment").then(|| {
                        percent_encoding::percent_decode_str(v)
                            .decode_utf8_lossy()
                            .into_owned()
                    })
                })
            })
    };
    let normalized = raw_comment
        .map(|c| c.replace("\r\n", " ").replace('\n', " "))
        .filter(|c| !c.trim().is_empty());
    Some(normalized)
}

pub(super) fn filter_theme_url(url: &str, is_light: bool) -> Option<&str> {
    let u = url.trim();
    if let Some(stripped) = strip_ascii_case_suffix(u, "#gh-dark-mode-only") {
        return (!is_light).then_some(stripped);
    }
    if let Some(stripped) = strip_ascii_case_suffix(u, "#gh-light-mode-only") {
        return is_light.then_some(stripped);
    }
    Some(u)
}

pub(super) fn is_media_basename(basename: &str) -> bool {
    let Some((_, ext)) = basename.rsplit_once('.') else {
        return false;
    };
    MEDIA_EXTENSIONS.iter().any(|m| ext.eq_ignore_ascii_case(m))
}

fn parse_positive_u64(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok().filter(|&n| n >= 1)
}

fn take_leading_digits(s: &str) -> Option<(&str, &str)> {
    let len = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    (len > 0).then_some((&s[..len], &s[len..]))
}

fn parse_hash_position(frag: &str) -> Option<FilePosition> {
    let rest = frag.strip_prefix('L').or_else(|| frag.strip_prefix('l'))?;
    let (line_s, after_line) = take_leading_digits(rest)?;
    let line = parse_positive_u64(line_s)?;
    if after_line.is_empty() {
        return Some(FilePosition {
            line,
            col: None,
            end_line: None,
        });
    }
    if let Some(col_rest) = after_line
        .strip_prefix('C')
        .or_else(|| after_line.strip_prefix('c'))
        .or_else(|| after_line.strip_prefix(":C"))
        .or_else(|| after_line.strip_prefix(":c"))
        .or_else(|| after_line.strip_prefix(':'))
    {
        let (col_s, after_col) = take_leading_digits(col_rest)?;
        let col = parse_positive_u64(col_s)?;
        if after_col.is_empty() {
            return Some(FilePosition {
                line,
                col: Some(col),
                end_line: None,
            });
        }
        if let Some(range_rest) = after_col.strip_prefix('-') {
            let range_rest = range_rest
                .strip_prefix('L')
                .or_else(|| range_rest.strip_prefix('l'))
                .unwrap_or(range_rest);
            let (end_s, tail) = take_leading_digits(range_rest)?;
            let end_line = parse_positive_u64(end_s)?;
            if !tail.is_empty() {
                let tail = tail.strip_prefix('C').or_else(|| tail.strip_prefix('c'))?;
                let (end_col_s, rem) = take_leading_digits(tail)?;
                parse_positive_u64(end_col_s)?;
                if !rem.is_empty() {
                    return None;
                }
            }
            return Some(FilePosition {
                line,
                col: Some(col),
                end_line: Some(end_line),
            });
        }
        return None;
    }
    if let Some(range_rest) = after_line.strip_prefix('-') {
        let range_rest = range_rest
            .strip_prefix('L')
            .or_else(|| range_rest.strip_prefix('l'))
            .unwrap_or(range_rest);
        let (end_s, tail) = take_leading_digits(range_rest)?;
        let end_line = parse_positive_u64(end_s)?;
        if !tail.is_empty() {
            let tail = tail.strip_prefix('C').or_else(|| tail.strip_prefix('c'))?;
            let (end_col_s, rem) = take_leading_digits(tail)?;
            parse_positive_u64(end_col_s)?;
            if !rem.is_empty() {
                return None;
            }
        }
        return Some(FilePosition {
            line,
            col: None,
            end_line: Some(end_line),
        });
    }
    None
}

fn split_colon_position(raw: &str) -> (&str, Option<FilePosition>) {
    let Some((prefix, last)) = raw.rsplit_once(':') else {
        return (raw, None);
    };
    if prefix.is_empty() {
        return (raw, None);
    }
    if let Some((start_s, end_raw)) = last.split_once('-') {
        let end_s = end_raw
            .strip_prefix('L')
            .or_else(|| end_raw.strip_prefix('l'))
            .unwrap_or(end_raw);
        if let (Some(line), Some(end_line)) =
            (parse_positive_u64(start_s), parse_positive_u64(end_s))
        {
            return (
                prefix,
                Some(FilePosition {
                    line,
                    col: None,
                    end_line: Some(end_line),
                }),
            );
        }
        return (raw, None);
    }
    let Some(last_num) = parse_positive_u64(last) else {
        return (raw, None);
    };
    if let Some((before_line, line_s)) = prefix.rsplit_once(':')
        && !before_line.is_empty()
        && let Some(line) = parse_positive_u64(line_s)
    {
        return (
            before_line,
            Some(FilePosition {
                line,
                col: Some(last_num),
                end_line: None,
            }),
        );
    }
    (
        prefix,
        Some(FilePosition {
            line: last_num,
            col: None,
            end_line: None,
        }),
    )
}

pub(super) fn split_position_suffix(
    raw: &str,
) -> std::result::Result<(&str, Option<FilePosition>), ()> {
    if let Some((before_hash, frag)) = raw.split_once('#') {
        let pos = parse_hash_position(frag).ok_or(())?;
        return Ok((before_hash, Some(pos)));
    }
    Ok(split_colon_position(raw))
}

fn is_windows_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

fn normalize_lexical(path: &str) -> Option<(String, Vec<String>, String)> {
    let slash = path.replace('\\', "/");
    let (prefix, rest, is_drive) = if is_windows_drive(&slash) {
        (&slash[..2], &slash[2..], true)
    } else if slash.starts_with('/') {
        ("", slash.as_str(), false)
    } else {
        return None;
    };
    let mut stack: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    let basename = (*stack.last()?).to_owned();
    let mut parent_segments = Vec::with_capacity(stack.len());
    if is_drive {
        parent_segments.push(prefix.to_owned());
    }
    for &seg in &stack[..stack.len() - 1] {
        parent_segments.push(seg.to_owned());
    }
    let normalized = if is_drive {
        format!("{prefix}/{}", stack.join("/"))
    } else {
        format!("/{}", stack.join("/"))
    };
    Some((normalized, parent_segments, basename))
}

pub(super) fn has_file_extension(basename: &str) -> bool {
    if let Some(dotfile) = basename.strip_prefix('.') {
        return dotfile.len() >= 2
            && dotfile
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            && dotfile.chars().any(|c| c.is_ascii_alphabetic());
    }
    let Some((stem, ext)) = basename.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=16).contains(&ext.len())
        && ext
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-')
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

pub(super) fn is_known_extensionless_file(basename: &str) -> bool {
    KNOWN_EXTENSIONLESS_FILES.contains(&basename)
}

fn is_allowed_posix_path(normalized: &str, cwd: Option<&Path>) -> bool {
    if ALLOWED_POSIX_ROOTS
        .iter()
        .any(|root| normalized.starts_with(root))
    {
        return true;
    }
    if let Some(cwd_str) = cwd.and_then(Path::to_str)
        && let Some((cwd_norm, _, _)) = normalize_lexical(cwd_str)
    {
        return normalized == cwd_norm
            || normalized
                .strip_prefix(&cwd_norm)
                .is_some_and(|rest| rest.starts_with('/'));
    }
    false
}

pub(super) fn parse_file_link(
    raw: &str,
    cwd: Option<&Path>,
    decode_percent: bool,
) -> Option<FileTarget> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.starts_with('#')
        || raw.starts_with('?')
        || raw.starts_with("//")
        || raw.contains('?')
    {
        return None;
    }
    let (after_scheme, is_file_uri) = match strip_ascii_case_prefix(raw, "file://") {
        Some(rest) => (rest, true),
        None => (raw, false),
    };
    if !is_file_uri && after_scheme.contains("://") {
        return None;
    }
    let (mut path_part, position) = split_position_suffix(after_scheme).ok()?;
    if is_file_uri {
        if let Some(rest) = strip_ascii_case_prefix(path_part, "localhost/") {
            path_part = &path_part[path_part.len() - rest.len() - 1..];
        }
        if path_part.starts_with('/') && is_windows_drive(&path_part[1..]) {
            path_part = &path_part[1..];
        }
    } else if path_part.contains(':') && !is_windows_drive(path_part) {
        return None;
    }
    let decoded = if decode_percent || is_file_uri {
        std::borrow::Cow::Owned(
            percent_encoding::percent_decode_str(path_part)
                .decode_utf8()
                .ok()?
                .into_owned(),
        )
    } else {
        std::borrow::Cow::Borrowed(path_part)
    };
    if decoded.is_empty()
        || decoded.ends_with('/')
        || decoded.ends_with('\\')
        || decoded.contains(['<', '>', '"', '|', '*', '?', '\0', '\r', '\n'])
    {
        return None;
    }
    let slash_path = decoded.replace('\\', "/");
    let (normalized_path, parent_segments, basename) =
        if slash_path.starts_with('/') || is_windows_drive(&slash_path) {
            let (norm, parents, base) = normalize_lexical(&slash_path)?;
            if !is_file_uri && !is_windows_drive(&norm) && !is_allowed_posix_path(&norm, cwd) {
                return None;
            }
            (norm, parents, base)
        } else {
            if slash_path.starts_with('~') || slash_path.starts_with('@') {
                return None;
            }
            let cwd_str = cwd?.to_str()?;
            let joined = format!("{}/{slash_path}", cwd_str.trim_end_matches(['/', '\\']));
            normalize_lexical(&joined)?
        };
    if basename.is_empty() || basename == "." || basename == ".." {
        return None;
    }
    if !(has_file_extension(&basename)
        || is_known_extensionless_file(&basename)
        || position.is_some()
        || is_file_uri)
    {
        return None;
    }
    Some(FileTarget {
        normalized_path,
        basename,
        parent_segments,
        position,
    })
}

pub(super) fn parse_inline_code_file_link(code: &str, cwd: Option<&Path>) -> Option<FileTarget> {
    if code.is_empty()
        || code.chars().any(char::is_whitespace)
        || code.contains("::")
        || code.contains(['(', ')', ';', '=', '&', '$', '`', '{', '}'])
    {
        return None;
    }
    let has_sep = code.contains('/') || code.contains('\\');
    let (_, pos) = split_position_suffix(code).ok()?;
    if !has_sep && pos.is_none() {
        return None;
    }
    parse_file_link(code, cwd, false)
}

pub(super) fn extract_redundant_label_position(
    label: &str,
    target: &FileTarget,
) -> Option<FilePosition> {
    let trimmed = label.trim();
    let (_, pos) = split_position_suffix(trimmed).ok()?;
    let pos = pos?;
    is_redundant_file_label(trimmed, target).then_some(pos)
}

pub(super) fn is_redundant_file_label(label: &str, target: &FileTarget) -> bool {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return true;
    }
    let without_scheme = strip_ascii_case_prefix(trimmed, "file://").unwrap_or(trimmed);
    let without_pos = match split_position_suffix(without_scheme) {
        Ok((base, _)) => base,
        Err(()) => without_scheme,
    };
    let mut cleaned = without_pos.replace('\\', "/");
    while let Some(rest) = cleaned.strip_prefix("./") {
        cleaned = rest.to_owned();
    }
    let cleaned = cleaned.trim_end_matches('/');
    if cleaned.is_empty() {
        return true;
    }
    if cleaned == target.normalized_path || cleaned == target.basename {
        return true;
    }
    if cleaned.starts_with('/') || is_windows_drive(cleaned) {
        if let Some((norm, _, _)) = normalize_lexical(cleaned)
            && norm == target.normalized_path
        {
            return true;
        }
        return false;
    }
    target
        .normalized_path
        .strip_suffix(cleaned)
        .is_some_and(|prefix| prefix.ends_with('/'))
}

pub(super) fn compute_duplicate_suffixes(chips: &[&FileTarget]) -> HashMap<String, String> {
    let mut seen_pairs: HashSet<(&str, &str)> = HashSet::new();
    let mut by_basename: HashMap<&str, Vec<&FileTarget>> = HashMap::new();
    for &chip in chips {
        if seen_pairs.insert((&chip.basename, &chip.normalized_path)) {
            by_basename.entry(&chip.basename).or_default().push(chip);
        }
    }
    let mut suffixes = HashMap::new();
    for group in by_basename.into_values() {
        if group.len() < 2 {
            continue;
        }
        let max_depth = group
            .iter()
            .map(|t| t.parent_segments.len())
            .max()
            .unwrap_or(0);
        let limit = max_depth.max(1);
        let mut resolved_depth = limit;
        for depth in 1..=limit {
            let mut unique = HashSet::with_capacity(group.len());
            let all_unique = group.iter().all(|t| {
                let start = t.parent_segments.len().saturating_sub(depth);
                unique.insert(t.parent_segments[start..].join("/"))
            });
            if all_unique {
                resolved_depth = depth;
                break;
            }
        }
        for t in group {
            let suffix_depth = t.parent_segments.len().min(resolved_depth.max(2));
            if suffix_depth > 0 {
                let start = t.parent_segments.len() - suffix_depth;
                let suffix = t.parent_segments[start..].join("/");
                if !suffix.is_empty() {
                    suffixes.insert(t.normalized_path.clone(), suffix);
                }
            }
        }
    }
    suffixes
}

fn is_bare_path_left_boundary(prev: char) -> bool {
    prev.is_whitespace()
        || matches!(
            prev,
            '(' | '['
                | '{'
                | '<'
                | '"'
                | '\''
                | '“'
                | '‘'
                | '='
                | ','
                | ';'
                | '（'
                | '【'
                | '「'
                | '『'
                | '《'
                | '，'
                | '；'
                | '：'
                | '、'
        )
}

fn is_bare_path_terminator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '<' | '>'
                | '"'
                | '\''
                | '`'
                | '|'
                | '*'
                | '?'
                | '{'
                | '}'
                | ';'
                | ','
                | '“'
                | '”'
                | '‘'
                | '’'
                | '，'
                | '。'
                | '；'
                | '！'
                | '？'
                | '（'
                | '）'
                | '【'
                | '】'
                | '「'
                | '」'
                | '『'
                | '』'
                | '《'
                | '》'
                | '、'
        )
}

fn trim_bare_path_trailing_punct(mut token: &str) -> &str {
    loop {
        let prev_len = token.len();
        token = token.trim_end_matches(['.', '!', '?', ':']);
        if token.ends_with(')') {
            let opens = token.bytes().filter(|&b| b == b'(').count();
            let closes = token.bytes().filter(|&b| b == b')').count();
            if closes > opens {
                token = &token[..token.len() - 1];
            }
        }
        if token.ends_with(']') {
            let opens = token.bytes().filter(|&b| b == b'[').count();
            let closes = token.bytes().filter(|&b| b == b']').count();
            if closes > opens {
                token = &token[..token.len() - 1];
            }
        }
        if token.len() == prev_len {
            return token;
        }
    }
}

fn starts_bare_candidate(rest: &str, cwd: Option<&Path>) -> bool {
    if strip_ascii_case_prefix(rest, "file:///").is_some() {
        return true;
    }
    if !rest.starts_with('/') || rest.starts_with("//") {
        return false;
    }
    if ALLOWED_POSIX_ROOTS
        .iter()
        .any(|root| rest.starts_with(root))
    {
        return true;
    }
    if let Some(cwd_str) = cwd.and_then(Path::to_str)
        && cwd_str.starts_with('/')
    {
        let root = cwd_str.trim_end_matches('/');
        if !root.is_empty()
            && rest
                .strip_prefix(root)
                .is_some_and(|tail| tail.starts_with('/'))
        {
            return true;
        }
    }
    false
}

pub(super) fn push_text_with_bare_paths(
    text: &str,
    cwd: Option<&Path>,
    builder: &mut SegmentBuilder<'_>,
) {
    if !text.contains('/') {
        builder.push_raw_text(text);
        return;
    }
    let mut emitted = 0;
    let mut prev_char: Option<char> = None;
    let mut iter = text.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        let boundary_ok = prev_char.is_none_or(is_bare_path_left_boundary);
        if boundary_ok && starts_bare_candidate(&text[idx..], cwd) {
            let tail = &text[idx..];
            let raw_end = tail
                .char_indices()
                .find_map(|(off, c)| is_bare_path_terminator(c).then_some(off))
                .unwrap_or(tail.len());
            let candidate = trim_bare_path_trailing_punct(&tail[..raw_end]);
            if !candidate.is_empty()
                && let Some(target) = parse_file_link(candidate, cwd, false)
                && !target.parent_segments.is_empty()
                && (has_file_extension(&target.basename)
                    || is_known_extensionless_file(&target.basename)
                    || target.position.is_some())
            {
                builder.push_raw_text(&text[emitted..idx]);
                builder.push_chip(target);
                emitted = idx + candidate.len();
                prev_char = candidate.chars().last();
                while iter.peek().is_some_and(|&(next_idx, _)| next_idx < emitted) {
                    iter.next();
                }
                continue;
            }
        }
        prev_char = Some(ch);
    }
    builder.push_raw_text(&text[emitted..]);
}
