//! Read the Git patch header only. Hunk contents may themselves resemble file and index lines.
use std::collections::BTreeMap;

#[derive(Default)]
struct Entry {
    old: Option<String>,
    new: Option<String>,
    deleted: bool,
    head: Option<String>,
    in_hunk: bool,
}
fn unquote(token: &str) -> Option<String> {
    let Some(token) = token.strip_prefix('"') else {
        return Some(token.to_owned());
    };
    let mut src = token.strip_suffix('"')?.bytes().peekable();
    let mut out = Vec::new();
    while let Some(ch) = src.next() {
        if ch != b'\\' {
            out.push(ch);
            continue;
        }
        let escaped = src.next()?;
        let b = match escaped {
            b'a' => 7,
            b'b' => 8,
            b't' => 9,
            b'n' => 10,
            b'v' => 11,
            b'f' => 12,
            b'r' => 13,
            b'\\' | b'"' => escaped,
            b'0'..=b'7' => {
                let mut value = u16::from(escaped - b'0');
                for _ in 0..2 {
                    if !src.peek().is_some_and(|b| matches!(b, b'0'..=b'7')) {
                        break;
                    }
                    value = value * 8 + u16::from(src.next()? - b'0');
                }
                u8::try_from(value).ok()?
            }
            _ => return None,
        };
        out.push(b);
    }
    String::from_utf8(out).ok()
}
fn header(raw: &str) -> Entry {
    let mut splits = raw
        .match_indices(" b/")
        .map(|(i, _)| i)
        .chain(raw.match_indices(" \"b/").map(|(i, _)| i));
    let mut best = None;
    let mut count = 0;
    for i in splits.by_ref() {
        let Some(left) = unquote(&raw[..i]).and_then(|s| s.strip_prefix("a/").map(str::to_owned))
        else {
            continue;
        };
        let Some(right) =
            unquote(&raw[i + 1..]).and_then(|s| s.strip_prefix("b/").map(str::to_owned))
        else {
            continue;
        };
        count += 1;
        if left == right {
            return Entry {
                old: Some(left),
                new: Some(right),
                ..Entry::default()
            };
        }
        best = Some((left, right));
    }
    if count == 1
        && let Some((old, new)) = best
    {
        return Entry {
            old: Some(old),
            new: Some(new),
            ..Entry::default()
        };
    }
    Entry::default()
}
fn side(raw: &str, prefix: &str) -> Option<String> {
    let s = raw.split('\t').next()?;
    if s == "/dev/null" {
        return None;
    }
    let name = unquote(s)?;
    Some(name.strip_prefix(prefix).unwrap_or(&name).to_owned())
}
fn store(map: &mut BTreeMap<String, String>, e: Entry) {
    let path = if e.deleted { e.old } else { e.new.or(e.old) };
    if let Some((path, oid)) = path.zip(e.head) {
        map.insert(path, oid);
    }
}
pub(super) fn revisions(raw: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut entry = None;
    for line in raw.lines() {
        if let Some(h) = line.strip_prefix("diff --git ") {
            if let Some(prev) = entry.replace(header(h)) {
                store(&mut map, prev);
            }
            continue;
        }
        let Some(e) = entry.as_mut().filter(|e| !e.in_hunk) else {
            continue;
        };
        if line.starts_with("@@") {
            e.in_hunk = true;
        } else if let Some(idx) = line.strip_prefix("index ") {
            e.head = idx
                .split_once("..")
                .and_then(|(_, h)| h.split_ascii_whitespace().next())
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(str::to_owned);
        } else if line.starts_with("deleted file mode") {
            e.deleted = true;
        } else if let Some(s) = line.strip_prefix("rename from ") {
            e.old = unquote(s);
        } else if let Some(s) = line.strip_prefix("rename to ") {
            e.new = unquote(s);
        } else if let Some(s) = line.strip_prefix("--- ") {
            e.old = side(s, "a/");
        } else if let Some(s) = line.strip_prefix("+++ ") {
            e.new = side(s, "b/");
            e.deleted |= e.new.is_none();
        }
    }
    if let Some(e) = entry {
        store(&mut map, e);
    }
    map
}
