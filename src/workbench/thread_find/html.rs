//! HTML block/inline extraction, legacy XML context stripping, and Insight divider removal.
use super::{
    files::{self, strip_ascii_case_prefix},
    render::{Piece, SegmentBuilder},
};
use std::{borrow::Cow, path::Path};

const IGNORED_TAGS: &[&str] = &["script", "style", "textarea", "title"];

const BLOCK_TAGS: &[&str] = &[
    "details",
    "summary",
    "div",
    "p",
    "br",
    "blockquote",
    "pre",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "th",
    "td",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "section",
    "article",
    "aside",
    "header",
    "footer",
    "main",
    "nav",
    "figure",
    "figcaption",
    "address",
];

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

const USER_CONTEXT_TAGS: &[&str] = &[
    "kar_context",
    "t3_context",
    "review_comment",
    "terminal_context",
    "element_context",
    "preview_annotation",
];

struct AnchorState {
    href: String,
    saved_current: Vec<Piece>,
}

struct PictureState {
    chosen_source: Option<String>,
    matched: bool,
}

#[derive(Default)]
pub(super) struct HtmlState {
    ignored_tag: Option<String>,
    pre_depth: usize,
    active_anchor: Option<AnchorState>,
    picture_state: Option<PictureState>,
}

impl HtmlState {
    pub(super) fn is_ignoring(&self) -> bool {
        self.ignored_tag.is_some()
    }

    pub(super) fn in_anchor(&self) -> bool {
        self.active_anchor.is_some()
    }

    fn close_anchor(&mut self, builder: &mut SegmentBuilder<'_>) {
        if let Some(anchor) = self.active_anchor.take() {
            let inner = std::mem::replace(&mut builder.current, anchor.saved_current);
            builder.emit_resolved_link(&anchor.href, inner);
        }
    }

    fn close_picture(&mut self, builder: &mut SegmentBuilder<'_>) {
        if let Some(pic) = self.picture_state.take()
            && let Some(src) = pic.chosen_source
        {
            builder.emit_image_url(&src);
        }
    }

    pub(super) fn finish_inline(&mut self, builder: &mut SegmentBuilder<'_>) {
        self.close_picture(builder);
        self.close_anchor(builder);
    }

    pub(super) fn finish(&mut self, builder: &mut SegmentBuilder<'_>) {
        self.ignored_tag = None;
        self.pre_depth = 0;
        self.finish_inline(builder);
    }

    pub(super) fn feed(&mut self, raw: &str, builder: &mut SegmentBuilder<'_>) {
        let mut cursor = 0;
        while cursor < raw.len() {
            if let Some(ign) = self.ignored_tag.as_deref() {
                if let Some((_, after_close)) = find_closing_tag(&raw[cursor..], ign) {
                    self.ignored_tag = None;
                    cursor += after_close;
                    continue;
                }
                break;
            }
            let rest = &raw[cursor..];
            let Some(rel_lt) = rest.find('<') else {
                self.emit_html_text(rest, builder);
                break;
            };
            if rel_lt > 0 {
                self.emit_html_text(&rest[..rel_lt], builder);
                cursor += rel_lt;
            }
            let at_lt = &raw[cursor..];
            match parse_html_token(at_lt) {
                Some(HtmlToken::Skip(len)) => {
                    cursor += len;
                }
                Some(HtmlToken::Cdata { inner, len }) => {
                    self.emit_html_text(inner, builder);
                    cursor += len;
                }
                Some(HtmlToken::Start {
                    name,
                    attrs,
                    self_closing,
                    len,
                }) => {
                    self.handle_start_tag(&name, attrs, self_closing, builder);
                    cursor += len;
                }
                Some(HtmlToken::End { name, len }) => {
                    self.handle_end_tag(&name, builder);
                    cursor += len;
                }
                None => {
                    self.emit_html_text("<", builder);
                    cursor += 1;
                }
            }
        }
    }

    fn emit_html_text(&self, raw: &str, builder: &mut SegmentBuilder<'_>) {
        if raw.is_empty() {
            return;
        }
        let decoded = decode_html_entities(raw);
        if self.pre_depth > 0 {
            builder.push_raw_text(&decoded);
        } else if self.active_anchor.is_some() {
            builder.push_prose_text(&decoded, false, false);
        } else {
            builder.push_prose_text(&decoded, true, true);
        }
    }

    fn handle_start_tag(
        &mut self,
        name: &str,
        attrs_raw: &str,
        self_closing: bool,
        builder: &mut SegmentBuilder<'_>,
    ) {
        let is_void = self_closing || VOID_TAGS.contains(&name);
        if IGNORED_TAGS.contains(&name) {
            if !is_void {
                self.ignored_tag = Some(name.to_owned());
            }
            return;
        }
        if BLOCK_TAGS.contains(&name) {
            if name == "br" && self.active_anchor.is_some() {
                builder.push_raw_text(" ");
                return;
            }
            self.close_anchor(builder);
            builder.flush();
            if name == "pre" && !is_void {
                self.pre_depth += 1;
            }
            return;
        }
        match name {
            "a" => {
                self.close_anchor(builder);
                let attrs = parse_html_attrs(attrs_raw);
                if let Some(href) = find_attr(&attrs, "href")
                    && !is_void
                {
                    let saved_current = std::mem::take(&mut builder.current);
                    self.active_anchor = Some(AnchorState {
                        href: href.to_owned(),
                        saved_current,
                    });
                }
            }
            "picture" => {
                self.close_picture(builder);
                if !is_void {
                    self.picture_state = Some(PictureState {
                        chosen_source: None,
                        matched: false,
                    });
                }
            }
            "source" => {
                if let Some(pic) = self.picture_state.as_mut()
                    && !pic.matched
                {
                    let attrs = parse_html_attrs(attrs_raw);
                    let media = find_attr(&attrs, "media").unwrap_or("");
                    if media_matches_theme(media, builder.is_light)
                        && let Some(raw_src) = find_attr(&attrs, "srcset")
                            .and_then(first_srcset_url)
                            .or_else(|| find_attr(&attrs, "src"))
                    {
                        pic.matched = true;
                        pic.chosen_source = Some(raw_src.to_owned());
                    }
                }
            }
            "img" => {
                let attrs = parse_html_attrs(attrs_raw);
                let fallback_src = find_attr(&attrs, "src");
                if let Some(pic) = self.picture_state.as_mut() {
                    let chosen = if pic.matched {
                        pic.chosen_source.take()
                    } else {
                        pic.matched = true;
                        fallback_src.map(str::to_owned)
                    };
                    if let Some(url) = chosen {
                        builder.emit_image_url(&url);
                    }
                } else if let Some(src) = fallback_src {
                    builder.emit_image_url(src);
                }
            }
            _ => {}
        }
    }

    fn handle_end_tag(&mut self, name: &str, builder: &mut SegmentBuilder<'_>) {
        if BLOCK_TAGS.contains(&name) {
            if name == "pre" {
                self.pre_depth = self.pre_depth.saturating_sub(1);
            }
            self.close_anchor(builder);
            builder.flush();
            return;
        }
        match name {
            "a" => self.close_anchor(builder),
            "picture" => self.close_picture(builder),
            _ => {}
        }
    }
}

enum HtmlToken<'a> {
    Skip(usize),
    Cdata {
        inner: &'a str,
        len: usize,
    },
    Start {
        name: String,
        attrs: &'a str,
        self_closing: bool,
        len: usize,
    },
    End {
        name: String,
        len: usize,
    },
}

fn find_unquoted_gt(s: &str) -> Option<(usize, bool)> {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut last_non_ws: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => {
                quote = Some(b);
                last_non_ws = Some(b);
            }
            b'>' => return Some((i, last_non_ws == Some(b'/'))),
            b if b.is_ascii_whitespace() => {}
            other => last_non_ws = Some(other),
        }
    }
    None
}

fn parse_html_token(s: &str) -> Option<HtmlToken<'_>> {
    if !s.starts_with('<') {
        return None;
    }
    if let Some(after_comment) = s.strip_prefix("<!--") {
        let len = match after_comment.find("-->") {
            Some(end) => 4 + end + 3,
            None => s.len(),
        };
        return Some(HtmlToken::Skip(len));
    }
    if let Some(after_cdata) = strip_ascii_case_prefix(s, "<![CDATA[") {
        return match after_cdata.find("]]>") {
            Some(end) => Some(HtmlToken::Cdata {
                inner: &after_cdata[..end],
                len: 9 + end + 3,
            }),
            None => Some(HtmlToken::Cdata {
                inner: after_cdata,
                len: s.len(),
            }),
        };
    }
    if s.get(1..2).is_some_and(|c| c == "!" || c == "?") {
        let gt = s[2..].find('>')?;
        return Some(HtmlToken::Skip(2 + gt + 1));
    }
    if let Some(rest) = s.strip_prefix("</") {
        let first = *rest.as_bytes().first()?;
        if !first.is_ascii_alphabetic() {
            return None;
        }
        let name_len = rest
            .bytes()
            .take_while(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            .count();
        let after_name = &rest[name_len..];
        if let Some(&next_b) = after_name.as_bytes().first()
            && !next_b.is_ascii_whitespace()
            && next_b != b'>'
        {
            return None;
        }
        let (gt_off, _) = find_unquoted_gt(after_name)?;
        return Some(HtmlToken::End {
            name: rest[..name_len].to_ascii_lowercase(),
            len: 2 + name_len + gt_off + 1,
        });
    }
    let rest = &s[1..];
    let first = *rest.as_bytes().first()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let name_len = rest
        .bytes()
        .take_while(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        .count();
    let after_name = &rest[name_len..];
    let next_b = *after_name.as_bytes().first()?;
    if !next_b.is_ascii_whitespace() && next_b != b'/' && next_b != b'>' {
        return None;
    }
    let (gt_off, self_closing) = find_unquoted_gt(after_name)?;
    Some(HtmlToken::Start {
        name: rest[..name_len].to_ascii_lowercase(),
        attrs: &after_name[..gt_off],
        self_closing,
        len: 1 + name_len + gt_off + 1,
    })
}

fn find_closing_tag(s: &str, tag_lower: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    while let Some(rel) = s[offset..].find("</") {
        let start = offset + rel;
        let after = &s[start + 2..];
        if let Some(tail) = strip_ascii_case_prefix(after, tag_lower)
            && tail
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
            && let Some(gt) = tail.find('>')
        {
            return Some((start, start + 2 + tag_lower.len() + gt + 1));
        }
        offset = start + 2;
    }
    None
}

fn parse_html_attrs(mut s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        if s.is_empty() {
            break;
        }
        let name_len = s
            .bytes()
            .take_while(|b| !b.is_ascii_whitespace() && *b != b'=' && *b != b'/' && *b != b'>')
            .count();
        if name_len == 0 {
            break;
        }
        let name = s[..name_len].to_ascii_lowercase();
        s = s[name_len..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if let Some(after_eq) = s.strip_prefix('=') {
            s = after_eq.trim_start_matches(|c: char| c.is_ascii_whitespace());
            let value = if let Some(quoted) = s.strip_prefix('"') {
                match quoted.find('"') {
                    Some(end) => {
                        let v = &quoted[..end];
                        s = &quoted[end + 1..];
                        v
                    }
                    None => {
                        let v = quoted;
                        s = "";
                        v
                    }
                }
            } else if let Some(quoted) = s.strip_prefix('\'') {
                match quoted.find('\'') {
                    Some(end) => {
                        let v = &quoted[..end];
                        s = &quoted[end + 1..];
                        v
                    }
                    None => {
                        let v = quoted;
                        s = "";
                        v
                    }
                }
            } else {
                let v_len = s
                    .bytes()
                    .take_while(|b| !b.is_ascii_whitespace() && *b != b'>')
                    .count();
                let v = &s[..v_len];
                s = &s[v_len..];
                v
            };
            out.push((name, decode_html_entities(value)));
        } else {
            out.push((name, String::new()));
        }
    }
    out
}

fn find_attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find_map(|(k, v)| (k == key).then_some(v.as_str()))
}

fn first_srcset_url(srcset: &str) -> Option<&str> {
    srcset
        .split(',')
        .next()?
        .split_whitespace()
        .next()
        .filter(|s| !s.is_empty())
}

fn media_matches_theme(media: &str, is_light: bool) -> bool {
    let m = media.trim().to_ascii_lowercase();
    if m.is_empty() || m == "all" || m == "screen" {
        return true;
    }
    if m.contains("prefers-color-scheme") {
        if m.contains("light") {
            return is_light;
        }
        if m.contains("dark") {
            return !is_light;
        }
    }
    false
}

pub(super) fn decode_html_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after_amp = &rest[amp + 1..];
        let semi = after_amp
            .bytes()
            .take(16)
            .position(|b| b == b';')
            .filter(|&pos| pos > 0 && after_amp[..pos].bytes().all(|b| b.is_ascii_graphic()));
        if let Some(pos) = semi
            && let Some(decoded_ch) = decode_single_entity(&after_amp[..pos])
        {
            out.push(decoded_ch);
            rest = &after_amp[pos + 1..];
            continue;
        }
        out.push('&');
        rest = after_amp;
    }
    out.push_str(rest);
    out
}

fn decode_single_entity(entity: &str) -> Option<char> {
    if let Some(num) = entity.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        let ch = char::from_u32(code)?;
        return Some(if ch == '\u{00a0}' { ' ' } else { ch });
    }
    match entity.to_ascii_lowercase().as_str() {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "ndash" => Some('\u{2013}'),
        "mdash" => Some('\u{2014}'),
        "hellip" => Some('\u{2026}'),
        "middot" => Some('\u{00b7}'),
        _ => None,
    }
}

fn parse_fence_line(line: &str) -> Option<(u8, usize, bool)> {
    let leading = line.bytes().take_while(|&b| b == b' ').count();
    if leading > 3 {
        return None;
    }
    let rest = &line[leading..];
    let marker = *rest.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let count = rest.bytes().take_while(|&b| b == marker).count();
    if count < 3 {
        return None;
    }
    let tail = &rest[count..];
    if marker == b'`' && tail.contains('`') {
        return None;
    }
    let only_ws = tail.bytes().all(|b| b == b' ' || b == b'\t');
    Some((marker, count, only_ws))
}

fn is_insight_open_line(line: &str) -> bool {
    let trimmed_end = line.trim_end_matches([' ', '\t']);
    let leading = trimmed_end
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .count();
    if leading > 3 {
        return false;
    }
    let s = trimmed_end.trim_start_matches([' ', '\t']);
    let s = match s.strip_prefix('`') {
        Some(inner) => match inner.strip_suffix('`') {
            Some(unquoted) => unquoted,
            None => return false,
        },
        None => s,
    };
    let Some(after_title) = s.strip_prefix("★ Insight") else {
        return false;
    };
    let after_ws = after_title.trim_start_matches([' ', '\t']);
    if after_ws.len() == after_title.len() {
        return false;
    }
    after_ws.chars().count() >= 3 && after_ws.chars().all(|c| c == '─')
}

fn is_insight_close_line(line: &str) -> bool {
    let trimmed_end = line.trim_end_matches([' ', '\t']);
    let leading = trimmed_end
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .count();
    if leading > 3 {
        return false;
    }
    let s = trimmed_end.trim_start_matches([' ', '\t']);
    let s = match s.strip_prefix('`') {
        Some(inner) => match inner.strip_suffix('`') {
            Some(unquoted) => unquoted,
            None => return false,
        },
        None => s,
    };
    s.chars().count() >= 3 && s.chars().all(|c| c == '─')
}

pub(super) fn strip_assistant_insight_dividers(text: &str) -> Cow<'_, str> {
    if !text.contains("★ Insight") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(u8, usize)> = None;
    let mut in_insight = false;
    for (idx, raw_line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if let Some((f_char, f_len)) = fence {
            if let Some((m_char, m_len, true)) = parse_fence_line(line)
                && m_char == f_char
                && m_len >= f_len
            {
                fence = None;
            }
            out.push_str(raw_line);
            continue;
        }
        if let Some((m_char, m_len, _)) = parse_fence_line(line) {
            fence = Some((m_char, m_len));
            out.push_str(raw_line);
            continue;
        }
        if is_insight_open_line(line) {
            in_insight = true;
            continue;
        }
        if in_insight && is_insight_close_line(line) {
            in_insight = false;
            continue;
        }
        out.push_str(raw_line);
    }
    Cow::Owned(out)
}

fn contains_any_user_context_tag(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    USER_CONTEXT_TAGS
        .iter()
        .any(|tag| lower.contains(&format!("<{tag}")))
}

fn format_context_file_chip(attrs: &[(String, String)], cwd: Option<&Path>) -> Option<String> {
    let file = find_attr(attrs, "file")
        .or_else(|| find_attr(attrs, "path"))?
        .trim();
    if file.is_empty() {
        return None;
    }
    let line_suffix = if let Some(lines) = find_attr(attrs, "lines").map(str::trim)
        && !lines.is_empty()
    {
        if lines.starts_with('L') || lines.starts_with('l') {
            format!("#{lines}")
        } else {
            format!("#L{lines}")
        }
    } else if let Some(start) = find_attr(attrs, "line")
        .or_else(|| find_attr(attrs, "start_line"))
        .or_else(|| find_attr(attrs, "startline"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(end) = find_attr(attrs, "end_line")
            .or_else(|| find_attr(attrs, "endline"))
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != start)
        {
            format!("#L{start}-L{end}")
        } else {
            format!("#L{start}")
        }
    } else {
        String::new()
    };
    let candidate = format!("{file}{line_suffix}");
    if files::parse_file_link(&candidate, cwd, false).is_some() {
        let escaped_path = file
            .replace('%', "%25")
            .replace(' ', "%20")
            .replace('(', "%28")
            .replace(')', "%29");
        let slash = file.replace('\\', "/");
        let base = slash.rsplit('/').next().unwrap_or(file);
        return Some(format!("[{base}]({escaped_path}{line_suffix})"));
    }
    let (without_pos, pos) = files::split_position_suffix(&candidate).unwrap_or((file, None));
    let slash = without_pos.replace('\\', "/");
    let base = slash
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(without_pos);
    if base.is_empty() {
        return None;
    }
    Some(match pos {
        Some(p) => format!("{base} \u{00b7} {}", p.format()),
        None => base.to_owned(),
    })
}

fn collect_envelope_chips(
    tag: &str,
    attrs_raw: &str,
    inner: &str,
    cwd: Option<&Path>,
    chips: &mut Vec<String>,
) {
    let attrs = parse_html_attrs(attrs_raw);
    if matches!(tag, "review_comment" | "element_context")
        && let Some(chip) = format_context_file_chip(&attrs, cwd)
    {
        chips.push(chip);
    }
    if matches!(tag, "kar_context" | "t3_context") {
        let mut pos = 0;
        while pos < inner.len() {
            let Some(rel) = inner[pos..].find('<') else {
                break;
            };
            let start = pos + rel;
            if let Some(HtmlToken::Start {
                name,
                attrs: child_attrs,
                self_closing,
                len,
            }) = parse_html_token(&inner[start..])
                && USER_CONTEXT_TAGS.contains(&name.as_str())
            {
                if self_closing {
                    collect_envelope_chips(&name, child_attrs, "", cwd, chips);
                    pos = start + len;
                    continue;
                }
                if let Some((body_end, after_close)) =
                    find_matching_context_end(&inner[start + len..], &name)
                {
                    let child_inner = &inner[start + len..start + len + body_end];
                    collect_envelope_chips(&name, child_attrs, child_inner, cwd, chips);
                    pos = start + len + after_close;
                    continue;
                }
            }
            pos = start + 1;
        }
    }
}

fn find_matching_context_end(s: &str, tag_lower: &str) -> Option<(usize, usize)> {
    let mut depth = 1usize;
    let mut pos = 0;
    while pos < s.len() {
        let rel = s[pos..].find('<')?;
        let at = pos + rel;
        match parse_html_token(&s[at..]) {
            Some(HtmlToken::Start {
                name,
                self_closing: false,
                len,
                ..
            }) if name == tag_lower => {
                depth += 1;
                pos = at + len;
            }
            Some(HtmlToken::End { name, len }) if name == tag_lower => {
                depth -= 1;
                if depth == 0 {
                    return Some((at, at + len));
                }
                pos = at + len;
            }
            Some(HtmlToken::Skip(len) | HtmlToken::Cdata { len, .. } | HtmlToken::Start { len, .. } | HtmlToken::End { len, .. }) => {
                pos = at + len;
            }
            None => {
                pos = at + 1;
            }
        }
    }
    None
}

pub(super) fn strip_user_context_envelopes<'a>(
    text: &'a str,
    cwd: Option<&Path>,
) -> Cow<'a, str> {
    if !text.contains('<') || !contains_any_user_context_tag(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(u8, usize)> = None;
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut at_line_start = true;
    while i < bytes.len() {
        if at_line_start {
            let line_end = text[i..].find('\n').map_or(text.len(), |n| i + n);
            let raw_line = &text[i..line_end];
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if let Some((f_char, f_len)) = fence {
                out.push_str(raw_line);
                if line_end < text.len() {
                    out.push('\n');
                    i = line_end + 1;
                } else {
                    i = text.len();
                }
                if let Some((m_char, m_len, true)) = parse_fence_line(line)
                    && m_char == f_char
                    && m_len >= f_len
                {
                    fence = None;
                }
                continue;
            }
            if let Some((m_char, m_len, _)) = parse_fence_line(line) {
                fence = Some((m_char, m_len));
                out.push_str(raw_line);
                if line_end < text.len() {
                    out.push('\n');
                    i = line_end + 1;
                } else {
                    i = text.len();
                }
                continue;
            }
        }
        let b = bytes[i];
        if b == b'\n' {
            out.push('\n');
            i += 1;
            at_line_start = true;
            continue;
        }
        at_line_start = false;
        if b == b'`' {
            let tick_len = text[i..].bytes().take_while(|&c| c == b'`').count();
            out.push_str(&text[i..i + tick_len]);
            i += tick_len;
            let mut scan = i;
            let mut closed = None;
            while let Some(rel) = text[scan..].find('`') {
                let pos = scan + rel;
                let run = text[pos..].bytes().take_while(|&c| c == b'`').count();
                if run == tick_len {
                    closed = Some(pos + run);
                    break;
                }
                scan = pos + run;
            }
            if let Some(end_idx) = closed {
                out.push_str(&text[i..end_idx]);
                i = end_idx;
            }
            continue;
        }
        if b == b'<'
            && let Some(HtmlToken::Start {
                name,
                attrs,
                self_closing,
                len,
            }) = parse_html_token(&text[i..])
            && USER_CONTEXT_TAGS.contains(&name.as_str())
        {
            if self_closing {
                let mut chips = Vec::new();
                collect_envelope_chips(&name, attrs, "", cwd, &mut chips);
                append_context_replacement(&mut out, &chips);
                i += len;
                continue;
            }
            if let Some((body_end, after_close)) =
                find_matching_context_end(&text[i + len..], &name)
            {
                let inner = &text[i + len..i + len + body_end];
                let mut chips = Vec::new();
                collect_envelope_chips(&name, attrs, inner, cwd, &mut chips);
                append_context_replacement(&mut out, &chips);
                i += len + after_close;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Cow::Owned(out)
}

fn append_context_replacement(out: &mut String, chips: &[String]) {
    out.push_str(" [](kar-context://flush) ");
    for chip in chips {
        out.push_str(chip);
        out.push_str(" [](kar-context://flush) ");
    }
}
