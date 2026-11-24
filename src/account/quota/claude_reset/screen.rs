//! Recognise the visible Claude Code screen before sending a key.
use super::ResetKind;
use regex::Regex;
use std::sync::LazyLock;
const FOOTER: &str = "Enter to confirm · Esc to cancel";
const TRUST: &str = "Quick safety check: Is this a project you created or one you trust?";
pub(super) fn footer(s: &str) -> bool {
    s.contains(FOOTER)
}
pub(super) fn sign_in(s: &str) -> bool {
    s.contains("Not logged in · Run /login")
}
pub(super) fn version(s: &str) -> Option<String> {
    static BANNER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)(?:^|\s)Claude Code v([0-9]+\.[0-9]+\.[0-9]+)\s*$").unwrap()
    });
    Some(BANNER.captures(s)?[1].into())
}
pub(super) fn input(s: &str) -> Option<String> {
    if footer(s) {
        return None;
    }
    let lines: Vec<_> = s.lines().collect();
    let mut starts = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.strip_prefix("❯\u{a0}").map(|first| (i, first)));
    let (start, first) = starts.next()?;
    if starts.next().is_some() {
        return None;
    }
    let rest = &lines[start + 1..];
    let end = rest.iter().position(|l| {
        let l = l.trim_end();
        !l.is_empty() && l.chars().all(|c| c == '─')
    })?;
    let mut rows = vec![first.trim_end()];
    rows.extend(rest[..end].iter().map(|l| l.trim()));
    while rows.len() > 1 && rows.last() == Some(&"") {
        rows.pop();
    }
    Some(rows.join("\n"))
}
pub(super) fn prompt(s: &str) -> Option<String> {
    if !s.contains("shift+tab to cycle") && !s.contains("? for shortcuts") {
        return None;
    }
    let line = input(s)?;
    static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^Try ".+"$"#).unwrap());
    Some(if PLACEHOLDER.is_match(&line) {
        String::new()
    } else {
        line
    })
}
fn trimmed(s: &str) -> Vec<&str> {
    s.lines().map(str::trim).filter(|s| !s.is_empty()).collect()
}
pub(super) fn trust(s: &str) -> Option<bool> {
    let rows = trimmed(s);
    if !rows.iter().any(|r| r.starts_with(TRUST)) {
        return None;
    }
    match rows.as_slice() {
        [.., "❯ No, exit", "Yes, I trust this folder", FOOTER] => Some(false),
        [.., "No, exit", "❯ Yes, I trust this folder", FOOTER] => Some(true),
        _ => None,
    }
}
pub(super) fn diagnostic(s: &str) -> super::StartupScreen {
    use super::StartupScreen::*;
    if s.trim().is_empty() {
        Blank
    } else if super::onboarding::page(s).is_some() {
        Onboarding
    } else if s.contains(TRUST) {
        TrustUnrecognised
    } else if footer(s) {
        Dialog
    } else {
        match prompt(s) {
            None => Unknown,
            Some(_) if version(s).is_none() => NoBanner,
            Some(line) if line.is_empty() => Unsettled,
            Some(_) => PromptWithText,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Dialog {
    pub kind: ResetKind,
    pub left: u32,
    pub expiry: String,
}
impl ResetKind {
    fn title(self) -> &'static str {
        match self {
            Self::FiveHour => "5-hour",
            Self::Weekly => "weekly",
            Self::Fable => "Fable",
            Self::Full => "full",
        }
    }
    pub(super) fn command(self) -> String {
        format!("/limit-reset {}", self.title().to_ascii_lowercase())
    }
    fn body_pattern(self) -> Regex {
        let name = self.title();
        let count = if self == Self::FiveHour {
            "five-hour"
        } else {
            name
        };
        let limit = "(?:5-hour|weekly|Opus|Sonnet|Fable)";
        let date = r"[A-Z][a-z]{2} [0-9]{1,2}(?:, | at )[0-9]{1,2}(?::[0-9]{2})?(?:am|pm)";
        let fable = if self == Self::Fable {
            "(?:returns what Fable used to your weekly limit · )?"
        } else {
            ""
        };
        Regex::new(&format!(r"^(?:You still have [0-9]{{1,3}}% of your {limit} limit left — use your {name} reset anyway\? )?Refills your ({limit}(?:, {limit})*(?: and {limit})?) limits? now · {fable}your weekly reset day stays {date} ([0-9]{{1,3}}) {count} resets? left · use by ({date})$")).unwrap()
    }
}
pub(super) fn dialog(s: &str) -> Option<Dialog> {
    static BODIES: LazyLock<[Regex; 4]> =
        LazyLock::new(|| ResetKind::ALL.map(ResetKind::body_pattern));
    let lines = trimmed(s);
    let [before @ .., yes, "No, keep it", FOOTER] = lines.as_slice() else {
        return None;
    };
    let selected = yes.strip_prefix("❯ Yes, use my ")?.strip_suffix(" reset")?;
    let idx = before
        .iter()
        .rposition(|l| (l.starts_with("Use a ") || l.starts_with("Use an ")) && l.ends_with('?'))?;
    if before[idx]
        .strip_prefix("Use a ")?
        .strip_suffix(" reset?")?
        != selected
    {
        return None;
    }
    let (i, kind) = ResetKind::ALL
        .into_iter()
        .enumerate()
        .find(|(_, k)| k.title() == selected)?;
    let body = before[idx + 1..].join(" ");
    let fields = BODIES[i].captures(&body)?;
    let refills = fields[1].replace(" and ", ", ");
    let required: &[&str] = match kind {
        ResetKind::FiveHour => &["5-hour"],
        ResetKind::Weekly => &["weekly"],
        ResetKind::Fable => &["Fable"],
        ResetKind::Full => &["5-hour", "weekly"],
    };
    if !required
        .iter()
        .all(|r| refills.split(", ").any(|s| s == *r))
    {
        return None;
    }
    Some(Dialog {
        kind,
        left: fields[2].parse().ok()?,
        expiry: fields[3].into(),
    })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Line {
    Reset(ResetKind),
    Kept,
    None,
    NeedsLimit,
    LegacySpent,
    LegacyUnavailable,
    Unknown,
}
fn result(s: &str) -> Line {
    for (prefix, kind) in [
        (
            "5-hour limit reset · your weekly reset day stays ",
            ResetKind::FiveHour,
        ),
        (
            "Weekly limits reset · your weekly reset day stays ",
            ResetKind::Weekly,
        ),
        ("Fable reset used · ", ResetKind::Fable),
        (
            "Limits reset · your weekly reset day stays ",
            ResetKind::Full,
        ),
    ] {
        if s.starts_with(prefix) {
            return Line::Reset(kind);
        }
    }
    if ["5-hour", "Weekly", "Fable", "Full"]
        .iter()
        .any(|k| s.starts_with(&format!("{k} reset kept · ")))
    {
        Line::Kept
    } else if [
        "reset",
        "5-hour reset",
        "weekly reset",
        "Fable reset",
        "full reset",
    ]
    .iter()
    .any(|k| s.starts_with(&format!("No {k} to use · ")))
    {
        Line::None
    } else if s.starts_with("You have a reset saved for when you reach a usage limit · ") {
        Line::NeedsLimit
    } else if s.starts_with("5-hour limit reset · next reset available ") {
        Line::LegacySpent
    } else if s == "A 5-hour limit reset isn't available right now." {
        Line::LegacyUnavailable
    } else {
        Line::Unknown
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Shown {
    Confirm(Dialog),
    OtherDialog,
    Result(Line),
    NoCommand,
}
pub(super) fn command(s: &str, command: &str) -> Option<Shown> {
    if let Some(d) = dialog(s) {
        return Some(Shown::Confirm(d));
    }
    if footer(s) {
        return Some(Shown::OtherDialog);
    }
    if !prompt(s)?.is_empty() {
        return None;
    }
    if s.contains("Unknown command: /limit-reset") {
        return Some(Shown::NoCommand);
    }
    let lines: Vec<_> = s.lines().collect();
    let echo = format!("❯ {command}");
    let idx = lines
        .iter()
        .rposition(|l| l.trim_end_matches(' ') == echo)?;
    let row = lines.get(idx + 1)?;
    // Newer Claude renderers draw the second space after the marker as a nonbreaking space.
    let first = row
        .strip_prefix("  ⎿  ")
        .or_else(|| row.strip_prefix("  ⎿ \u{a0}"))?;
    if first.is_empty() || first.starts_with(char::is_whitespace) {
        return None;
    }
    let mut line = first.trim_end().to_owned();
    for row in &lines[idx + 2..] {
        let Some(part) = row
            .strip_prefix("     ")
            .filter(|v| !v.is_empty() && !v.starts_with(char::is_whitespace))
        else {
            break;
        };
        line.push(' ');
        line.push_str(part.trim_end());
    }
    Some(Shown::Result(result(&line)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_reads_the_result_of_the_visible_submitted_command() {
        for marker in ["  ⎿  ", "  ⎿ \u{a0}"] {
            let cmd = "/limit-reset weekly";
            let screen = format!(
                "❯ {cmd}   \n{marker}Weekly limits reset · your weekly reset day stays Nov 24 at 11pm\n────────\n❯\u{a0} \n────────\n(shift+tab to cycle)"
            );
            assert_eq!(
                command(&screen, cmd),
                Some(Shown::Result(Line::Reset(ResetKind::Weekly)))
            );
            assert_eq!(command(&screen, "/limit-reset full"), None);
            assert_eq!(
                command(&screen.replace("❯\u{a0} ", "❯\u{a0}do something else"), cmd),
                None
            );
        }
    }
}
