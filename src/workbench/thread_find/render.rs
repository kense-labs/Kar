//! Search text displayed in a Markdown conversation, keeping block boundaries distinct.
use super::Skill;
use markdown::mdast::Node;

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

fn visit(
    node: &Node,
    user: bool,
    skills: &[Skill],
    current: &mut String,
    segments: &mut Vec<String>,
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
    let flush = |s: &mut String, all: &mut Vec<String>| {
        if !s.trim().is_empty() {
            all.push(std::mem::take(s));
        } else {
            s.clear();
        }
    };
    if block {
        flush(current, segments);
    }
    match node {
        Node::Text(t) => current.push_str(&replace_skills(
            &t.value.replace("\r\n", " ").replace('\n', " "),
            skills,
        )),
        Node::Html(html) if user => current.push_str(&html.value),
        Node::InlineCode(code) => current.push_str(&code.value),
        Node::Code(code) => current.push_str(&code.value),
        Node::Link(link) if user && link.url.starts_with("kar-context:") => {
            flush(current, segments)
        }
        Node::Link(link) => {
            for child in &link.children {
                visit(child, user, &[], current, segments);
            }
        }
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    visit(child, user, skills, current, segments);
                }
            }
        }
    }
    if block {
        flush(current, segments);
    }
}

pub(super) fn occurrences(
    text: &str,
    user: bool,
    complete: bool,
    query: &str,
    skills: &[Skill],
) -> usize {
    let bootstrap = text
        .get(..11)
        .filter(|p| p.eq_ignore_ascii_case("Ultrathink:"))
        .map_or(text, |_| text[11..].trim_start());
    if user && bootstrap == ATTACHMENT_ONLY {
        return 0;
    }
    let text = if !user && complete && text.is_empty() {
        "(empty response)"
    } else {
        text
    };
    // Plain messages without Markdown or skill references need no syntax tree.
    if !text.contains(|c: char| !c.is_alphanumeric() && c != ' ' && c != '’' && c != '\'') {
        return fold(text).matches(query).count();
    }
    let Ok(root) = markdown::to_mdast(text, &markdown::ParseOptions::gfm()) else {
        return fold(text).matches(query).count();
    };
    let mut current = String::new();
    let mut segments = Vec::new();
    visit(&root, user, skills, &mut current, &mut segments);
    segments.push(current);
    segments
        .iter()
        .map(|s| fold(s).matches(query).count())
        .sum()
}
