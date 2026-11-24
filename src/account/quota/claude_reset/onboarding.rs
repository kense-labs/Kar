// Recorded Claude Code 2.1.322 first-run screens from OAR 0.129.1.
use std::sync::LazyLock;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Page {
    Theme,
    Security,
    Other,
}
fn normalized(s: &str) -> Option<String> {
    let text = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (version, body) = text
        .strip_prefix("Welcome to Claude Code v")?
        .split_once(' ')?;
    let version = semver::Version::parse(version).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then(|| body.to_owned())
}
pub(super) fn page(screen: &str) -> Option<Page> {
    if ![
        "Welcome to Claude Code",
        "Choose the text style",
        "Security notes:",
    ]
    .iter()
    .any(|s| screen.contains(s))
    {
        return None;
    }
    static THEME: LazyLock<String> =
        LazyLock::new(|| normalized(include_str!("screens/theme.txt")).unwrap());
    static SECURITY: LazyLock<String> =
        LazyLock::new(|| normalized(include_str!("screens/security.txt")).unwrap());
    Some(match normalized(screen) {
        Some(s) if s == *THEME => Page::Theme,
        Some(s) if s == *SECURITY => Page::Security,
        _ => Page::Other,
    })
}
