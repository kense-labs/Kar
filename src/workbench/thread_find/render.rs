//! Search text displayed in a Markdown conversation, keeping block boundaries distinct.
use super::{
    Skill,
    files::{self, FileTarget},
    html,
};
use markdown::mdast::Node;
use std::path::Path;

pub(super) fn fold(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '‘' | '’' => '\'',
            'ς' => 'σ',
            'İ' => 'i',
            _ => {
                let mut lowered = c.to_lowercase();
                let first = lowered.next().unwrap_or(c);
                if lowered.next().is_none() && first.len_utf16() == c.len_utf16() {
                    first
                } else {
                    c
                }
            }
        })
        .collect()
}

const ATTACHMENT_ONLY: &str = "[User attached one or more files without additional text. Respond using the conversation context and the attached files.]";

fn replace_skills(value: &str, skills: &[Skill]) -> String {
    if skills.is_empty() {
        return value.to_owned();
    }
    let mut text = String::with_capacity(value.len());
    let mut rest = value;
    while !rest.is_empty() {
        let space = rest.starts_with(char::is_whitespace);
        let end = rest
            .find(|c: char| c.is_whitespace() != space)
            .unwrap_or(rest.len());
        let token = &rest[..end];
        let skill = token
            .strip_prefix('$')
            .and_then(|key| skills.iter().find(|s| s.name == key));
        text.push_str(skill.map_or(token, |s| {
            s.display_name
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(&s.name)
        }));
        rest = &rest[end..];
    }
    text
}

#[derive(Clone, Debug)]
pub(super) enum Piece {
    Text(String),
    Chip(FileTarget),
}

pub(super) struct SegmentBuilder<'a> {
    user: bool,
    skills: &'a [Skill],
    cwd: Option<&'a Path>,
    pub(super) is_light: bool,
    pub(super) current: Vec<Piece>,
    segments: Vec<Vec<Piece>>,
}

impl<'a> SegmentBuilder<'a> {
    fn new(user: bool, skills: &'a [Skill], cwd: Option<&'a Path>, is_light: bool) -> Self {
        Self {
            user,
            skills,
            cwd,
            is_light,
            current: Vec::new(),
            segments: Vec::new(),
        }
    }

    pub(super) fn push_raw_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(Piece::Text(prev)) = self.current.last_mut() {
            prev.push_str(s);
        } else {
            self.current.push(Piece::Text(s.to_owned()));
        }
    }

    pub(super) fn push_chip(&mut self, chip: FileTarget) {
        self.current.push(Piece::Chip(chip));
    }

    pub(super) fn push_prose_text(
        &mut self,
        raw: &str,
        apply_skills: bool,
        detect_bare_paths: bool,
    ) {
        let normalized = raw.replace("\r\n", " ").replace('\n', " ");
        let skills = self.skills;
        let cwd = self.cwd;
        let with_skills = if apply_skills && !skills.is_empty() {
            std::borrow::Cow::Owned(replace_skills(&normalized, skills))
        } else {
            std::borrow::Cow::Borrowed(normalized.as_str())
        };
        if detect_bare_paths {
            files::push_text_with_bare_paths(&with_skills, cwd, self);
        } else {
            self.push_raw_text(&with_skills);
        }
    }

    pub(super) fn flush(&mut self) {
        let non_empty = self.current.iter().any(|p| match p {
            Piece::Text(s) => !s.trim().is_empty(),
            Piece::Chip(_) => true,
        });
        if non_empty {
            self.segments.push(std::mem::take(&mut self.current));
        } else {
            self.current.clear();
        }
    }

    pub(super) fn emit_image_url(&mut self, url: &str) {
        if files::is_context_uri(url) {
            self.flush();
            return;
        }
        let Some(visible_url) = files::filter_theme_url(url, self.is_light) else {
            return;
        };
        if let Some(target) = files::parse_file_link(visible_url, self.cwd, true)
            && !files::is_media_basename(&target.basename)
        {
            self.push_chip(target);
        }
    }

    pub(super) fn emit_resolved_link(&mut self, href: &str, inner_pieces: Vec<Piece>) {
        if files::is_context_uri(href) {
            self.flush();
            return;
        }
        if let Some(comment) = files::parse_citation_comment(href) {
            self.flush();
            if let Some(note) = comment {
                self.push_raw_text(&note);
            }
            return;
        }
        let trimmed = href.trim();
        let clean_href = files::strip_ascii_case_suffix(trimmed, "#gh-dark-mode-only")
            .or_else(|| files::strip_ascii_case_suffix(trimmed, "#gh-light-mode-only"))
            .unwrap_or(trimmed);
        if let Some(mut target) = files::parse_file_link(clean_href, self.cwd, true) {
            let mut label = String::new();
            for piece in &inner_pieces {
                match piece {
                    Piece::Text(t) => label.push_str(t),
                    Piece::Chip(c) => label.push_str(&c.basename),
                }
            }
            if target.position.is_none() {
                target.position = files::extract_redundant_label_position(&label, &target);
            }
            if files::is_redundant_file_label(&label, &target) {
                self.push_chip(target);
            } else {
                self.push_raw_text(&label);
                if !label.ends_with(char::is_whitespace) {
                    self.push_raw_text(" ");
                }
                self.push_chip(target);
            }
            return;
        }
        for piece in inner_pieces {
            match piece {
                Piece::Text(t) => self.push_raw_text(&t),
                Piece::Chip(c) => self.push_chip(c),
            }
        }
    }

    fn count_matches(&self, query: &str) -> usize {
        let all_chips: Vec<&FileTarget> = self
            .segments
            .iter()
            .flat_map(|seg| seg.iter())
            .filter_map(|p| match p {
                Piece::Chip(c) => Some(c),
                Piece::Text(_) => None,
            })
            .collect();
        let suffixes = files::compute_duplicate_suffixes(&all_chips);
        self.segments
            .iter()
            .map(|seg| {
                let mut rendered = String::new();
                for piece in seg {
                    match piece {
                        Piece::Text(t) => rendered.push_str(t),
                        Piece::Chip(c) => rendered.push_str(&c.chip_label(&suffixes)),
                    }
                }
                fold(&rendered).matches(query).count()
            })
            .sum()
    }
}

fn visit(
    node: &Node,
    in_link: bool,
    builder: &mut SegmentBuilder<'_>,
    html_state: &mut html::HtmlState,
) {
    let block = matches!(
        node,
        Node::Paragraph(_)
            | Node::Heading(_)
            | Node::Blockquote(_)
            | Node::List(_)
            | Node::ListItem(_)
            | Node::Code(_)
            | Node::TableCell(_)
            | Node::ThematicBreak(_)
            | Node::Break(_)
    );
    if block {
        if matches!(node, Node::Break(_)) && (in_link || html_state.in_anchor()) {
            builder.push_raw_text(" ");
            return;
        }
        html_state.finish_inline(builder);
        builder.flush();
    }
    match node {
        Node::Html(raw) => {
            if builder.user {
                if !html_state.is_ignoring() {
                    builder.push_raw_text(&raw.value);
                }
            } else {
                html_state.feed(&raw.value, builder);
            }
        }
        _ if html_state.is_ignoring() => {}
        Node::Text(t) => {
            let inside_link = in_link || html_state.in_anchor();
            builder.push_prose_text(&t.value, !inside_link, !inside_link);
        }
        Node::InlineCode(code) => {
            let inside_link = in_link || html_state.in_anchor();
            if !inside_link
                && let Some(target) = files::parse_inline_code_file_link(&code.value, builder.cwd)
            {
                builder.push_chip(target);
            } else {
                builder.push_raw_text(&code.value);
            }
        }
        Node::Code(code) => builder.push_raw_text(&code.value),
        Node::Image(image) => builder.emit_image_url(&image.url),
        Node::Link(link) => {
            if files::is_context_uri(&link.url) {
                builder.flush();
            } else if let Some(comment) = files::parse_citation_comment(&link.url) {
                builder.flush();
                if let Some(note) = comment {
                    builder.push_raw_text(&note);
                }
            } else {
                let mut child_builder =
                    SegmentBuilder::new(builder.user, &[], builder.cwd, builder.is_light);
                let mut child_html = html::HtmlState::default();
                for child in &link.children {
                    visit(child, true, &mut child_builder, &mut child_html);
                }
                child_html.finish(&mut child_builder);
                let mut inner_pieces = Vec::new();
                for seg in child_builder.segments {
                    if !inner_pieces.is_empty() {
                        inner_pieces.push(Piece::Text(" ".to_owned()));
                    }
                    inner_pieces.extend(seg);
                }
                if !child_builder.current.is_empty() {
                    if !inner_pieces.is_empty() {
                        inner_pieces.push(Piece::Text(" ".to_owned()));
                    }
                    inner_pieces.extend(child_builder.current);
                }
                builder.emit_resolved_link(&link.url, inner_pieces);
            }
        }
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    visit(child, in_link, builder, html_state);
                }
            }
        }
    }
    if block {
        html_state.finish_inline(builder);
        builder.flush();
    }
}

pub(super) fn plain_occurrences(text: &str, query: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let normalized = text.replace("\r\n", " ").replace('\n', " ");
    fold(&normalized).matches(query).count()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn occurrences(
    text: &str,
    user: bool,
    complete: bool,
    suppress_empty: bool,
    query: &str,
    skills: &[Skill],
    cwd: Option<&Path>,
    theme: Option<&str>,
) -> usize {
    let bootstrap = text
        .get(..11)
        .filter(|p| p.eq_ignore_ascii_case("Ultrathink:"))
        .map_or(text, |_| text[11..].trim_start());
    if user && bootstrap == ATTACHMENT_ONLY {
        return 0;
    }
    if !user && complete && text.is_empty() {
        if suppress_empty {
            return 0;
        }
        return fold("(empty response)").matches(query).count();
    }
    if text.is_empty() {
        return 0;
    }
    // Plain messages without Markdown or skill references need no syntax tree.
    if !text.contains(|c: char| !c.is_alphanumeric() && c != ' ' && c != '’' && c != '\'') {
        return fold(text).matches(query).count();
    }
    let prepared = if user {
        html::strip_user_context_envelopes(text, cwd)
    } else {
        html::strip_assistant_insight_dividers(text)
    };
    let mut options = markdown::ParseOptions::gfm();
    options.gfm_strikethrough_single_tilde = !user;
    options.constructs.code_indented = !user;
    let Ok(root) = markdown::to_mdast(&prepared, &options) else {
        return fold(&prepared).matches(query).count();
    };
    let is_light = theme.is_some_and(|t| t.trim().eq_ignore_ascii_case("light"));
    let mut builder = SegmentBuilder::new(user, skills, cwd, is_light);
    let mut html_state = html::HtmlState::default();
    visit(&root, false, &mut builder, &mut html_state);
    html_state.finish(&mut builder);
    builder.flush();
    builder.count_matches(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(
        text: &str,
        user: bool,
        query: &str,
        cwd: Option<&Path>,
        theme: Option<&str>,
    ) -> usize {
        occurrences(text, user, true, false, &fold(query), &[], cwd, theme)
    }

    #[test]
    fn duplicate_basename_and_line_range_chips() {
        let cwd = Path::new("/workspace/repo");
        let msg = "Review [src/notes.ts](src/notes.ts#L42-L64) and [test/notes.ts](test/notes.ts#L10) plus [important description](/tmp/actual.ts) and [external](https://hidden.example/secret).";
        assert_eq!(count(msg, false, "notes.ts", Some(cwd), None), 2);
        assert_eq!(count(msg, false, "repo/src", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "repo/test", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "L42–64", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "L42-64", Some(cwd), None), 0);
        assert_eq!(count(msg, false, "L10", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "src/notes.ts", Some(cwd), None), 0);
        assert_eq!(count(msg, false, "important description", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "actual.ts", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "/tmp", Some(cwd), None), 0);
        assert_eq!(count(msg, false, "hidden.example", Some(cwd), None), 0);
        assert_eq!(count(msg, false, "secret", Some(cwd), None), 0);
    }

    #[test]
    fn inline_code_bare_prose_and_non_media_image_chips() {
        let cwd = Path::new("/workspace/repo");
        let msg = "Inline `/tmp/file.ts:42:5` and bare /Users/alice/app/Makefile:7 and ![spec](docs/design.md#L3-L3)\n```ts\nconst raw = \"/tmp/literal.ts:99\";\n```";
        assert_eq!(count(msg, false, "file.ts · L42:C5", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "/tmp/file.ts", Some(cwd), None), 0);
        assert_eq!(count(msg, false, "Makefile · L7", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "/Users/alice", Some(cwd), None), 0);
        assert_eq!(count(msg, false, "design.md · L3", Some(cwd), None), 1);
        assert_eq!(count(msg, false, "/tmp/literal.ts:99", Some(cwd), None), 1);
    }

    #[test]
    fn citations_and_context_image_boundaries() {
        let with_note =
            "fix [Assistant quote](kar-citation://v1/e/t/m?text=secret&comment=my%0Anote) thanks";
        assert_eq!(count(with_note, true, "my note thanks", None, None), 1);
        assert_eq!(count(with_note, true, "fix my note", None, None), 0);
        assert_eq!(count(with_note, true, "secret", None, None), 0);
        assert_eq!(count(with_note, true, "Assistant quote", None, None), 0);

        let without_note = "Please fix [Assistant quote](t3-citation://v1/e/t/m?text=secret) thanks";
        assert_eq!(count(without_note, true, "Please fix", None, None), 1);
        assert_eq!(count(without_note, true, "thanks", None, None), 1);
        assert_eq!(count(without_note, true, "Please fix  thanks", None, None), 0);

        let img_ctx = "before![img](kar-context://v1/image/1)after";
        assert_eq!(count(img_ctx, true, "before", None, None), 1);
        assert_eq!(count(img_ctx, true, "after", None, None), 1);
        assert_eq!(count(img_ctx, true, "beforeafter", None, None), 0);
    }

    #[test]
    fn html_details_picture_theme_and_entities() {
        let html_msg = "<details><summary>折叠标题 &amp; 概览</summary><p>折叠正文</p><details><summary>Inner</summary><p>second</p></details></details><script>window.secret_token = 1;</script><picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"/tmp/dark_spec.ts\"><source media=\"(prefers-color-scheme: light)\" srcset=\"/tmp/light_spec.ts\"><img src=\"/tmp/fallback_spec.ts\"></picture>";
        assert_eq!(count(html_msg, false, "折叠标题 & 概览", None, Some("dark")), 1);
        assert_eq!(count(html_msg, false, "折叠正文", None, Some("dark")), 1);
        assert_eq!(count(html_msg, false, "折叠标题 & 概览折叠正文", None, Some("dark")), 0);
        assert_eq!(count(html_msg, false, "Inner", None, Some("dark")), 1);
        assert_eq!(count(html_msg, false, "second", None, Some("dark")), 1);
        assert_eq!(count(html_msg, false, "secret_token", None, Some("dark")), 0);
        assert_eq!(count(html_msg, false, "dark_spec.ts", None, Some("dark")), 1);
        assert_eq!(count(html_msg, false, "light_spec.ts", None, Some("dark")), 0);
        assert_eq!(count(html_msg, false, "light_spec.ts", None, Some("light")), 1);
        assert_eq!(count(html_msg, false, "dark_spec.ts", None, Some("light")), 0);
    }

    #[test]
    fn insight_dividers_user_tildes_and_xml_context() {
        let assistant = "`★ Insight ─────────────────────────────────────`\n Searchable insight body\n`─────────────────────────────────────────────────`";
        assert_eq!(count(assistant, false, "Searchable insight body", None, None), 1);
        assert_eq!(count(assistant, false, "★ Insight", None, None), 0);

        let user_tilde = "Install as ~/a/x, then read (~/a/guide.md)";
        assert_eq!(count(user_tilde, true, "~/a/x", None, None), 1);
        assert_eq!(count(user_tilde, true, "~/a/guide.md", None, None), 1);

        let user_xml = "before<kar_context><review_comment file=\"src/notes.ts\" line=\"12\">hidden_diff_line</review_comment></kar_context>after";
        let cwd = Path::new("/workspace/repo");
        assert_eq!(count(user_xml, true, "notes.ts · L12", Some(cwd), None), 1);
        assert_eq!(count(user_xml, true, "hidden_diff_line", Some(cwd), None), 0);
        assert_eq!(count(user_xml, true, "beforeafter", Some(cwd), None), 0);

        assert_eq!(
            occurrences("", false, true, false, &fold("empty response"), &[], None, None),
            1
        );
        assert_eq!(
            occurrences("", false, true, true, &fold("empty response"), &[], None, None),
            0
        );
    }
}
