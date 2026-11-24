use super::{Node, Result, Text, Truncated, attr, nodes};
use anyhow::ensure;

fn hidden(n: Node<'_, '_>) -> bool {
    if !n.is_element() {
        return false;
    }
    match n.tag_name().name() {
        "head"
        | "script"
        | "style"
        | "binary-data"
        | "rPh"
        | "del"
        | "delText"
        | "moveFrom"
        | "instrText"
        | "delInstrText"
        | "tracked-changes"
        | "pPr"
        | "positionH"
        | "positionV"
        | "sizeRelH"
        | "sizeRelV"
        | "fldData"
        | "table-of-content-source"
        | "illustration-index-source"
        | "table-index-source"
        | "object-index-source"
        | "user-index-source"
        | "alphabetical-index-source"
        | "bibliography-source" => true,
        "sp" => nodes(n, "ph").any(|p| {
            matches!(
                attr(p, "type"),
                Some("sldImg" | "sldNum" | "dt" | "hdr" | "ftr")
            )
        }),
        _ => false,
    }
}
fn count(n: Node<'_, '_>, attr_name: &str) -> Result<usize> {
    let n = attr(n, attr_name).unwrap_or("1").parse()?;
    ensure!(n > 0, "invalid repeat count");
    Ok(n)
}
fn is_odf(n: Node<'_, '_>) -> bool {
    n.tag_name()
        .namespace()
        .is_some_and(|ns| ns.starts_with("urn:oasis:names:tc:opendocument:"))
}
pub(super) fn body(n: Node<'_, '_>, out: &mut Text) -> Result<()> {
    walk(n, out, 0, false)
}
fn walk(n: Node<'_, '_>, out: &mut Text, depth: usize, inline: bool) -> Result<()> {
    if depth > 128 {
        return Err(Truncated.into());
    }
    if n.is_text() {
        let s = n.text().unwrap_or_default();
        if inline || !s.trim().is_empty() {
            out.push(s)?;
        }
        return Ok(());
    }
    if hidden(n) {
        return Ok(());
    }
    let tag = n.tag_name().name();
    match tag {
        "AlternateContent" => {
            let chosen = n
                .children()
                .filter(|n| n.is_element())
                .find(|n| {
                    n.tag_name().name() == "Choice"
                        && n.descendants()
                            .any(|c| c.is_text() && c.text().is_some_and(|s| !s.trim().is_empty()))
                })
                .or_else(|| {
                    n.children()
                        .find(|c| c.is_element() && c.tag_name().name() == "Fallback")
                });
            if let Some(child) = chosen {
                walk(child, out, depth + 1, inline)?;
            }
            return Ok(());
        }
        "table" | "tbl" => return table_at(n, out, depth + 1),
        "br" | "cr" | "line-break" => return out.push("\n"),
        "tab" | "ptab" => return out.push("\t"),
        "noBreakHyphen" => return out.push("‑"),
        "s" if is_odf(n) => {
            let repeat = count(n, "c")?;
            out.push(&" ".repeat(repeat.min(out.remaining)))?;
            if repeat > super::super::MAX_UNITS || out.remaining == 0 {
                return Err(Truncated.into());
            }
            return Ok(());
        }
        "notes" => out.push("\nNotes:\n")?,
        _ => {}
    }
    let para = matches!(tag, "p" | "h" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
    if para {
        let level = match tag {
            "h" => attr(n, "outline-level")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
            "p" => nodes(n, "pStyle")
                .find_map(|c| attr(c, "val"))
                .and_then(|s| s.to_ascii_lowercase().strip_prefix("heading")?.parse().ok())
                .unwrap_or(0),
            _ => tag[1..].parse().unwrap_or(0usize),
        };
        if level > 0 {
            out.push(&"#".repeat(level.min(6)))?;
            out.push(" ")?;
        }
    }
    // Ignore package indentation outside actual text. XHTML, ODF and Word runs retain text.
    for child in n.children() {
        walk(
            child,
            out,
            depth + 1,
            inline
                || (para
                    && (is_odf(n)
                        || matches!(
                            n.tag_name().namespace(),
                            None | Some("http://www.w3.org/1999/xhtml")
                        )))
                || matches!(tag, "t" | "span" | "li"),
        )?;
    }
    if para || matches!(tag, "div" | "li" | "section") {
        out.push("\n")?;
    }
    Ok(())
}
pub(super) fn table(n: Node<'_, '_>, out: &mut Text) -> Result<()> {
    table_at(n, out, 0)
}
fn table_at(n: Node<'_, '_>, out: &mut Text, depth: usize) -> Result<()> {
    if depth > 128 {
        return Err(Truncated.into());
    }
    let row_tag = if n.tag_name().name() == "tbl" {
        "tr"
    } else if is_odf(n) {
        "table-row"
    } else {
        "tr"
    };
    let mut emitted = 0usize;
    for row in nodes(n, row_tag).filter(|r| {
        r.ancestors()
            .skip(1)
            .find(|e| e.is_element() && matches!(e.tag_name().name(), "table" | "tbl"))
            == Some(n)
    }) {
        out.check()?;
        let mut values = Vec::new();
        let mut width = 0usize;
        let mut clipped = false;
        for cell in row.children().filter(|n| {
            n.is_element()
                && matches!(
                    n.tag_name().name(),
                    "td" | "th" | "tc" | "table-cell" | "covered-table-cell"
                )
        }) {
            let times = count(cell, "number-columns-repeated")?;
            let mut local = Text {
                value: String::new(),
                remaining: out.remaining,
                deadline: out.deadline,
            };
            for child in cell.children() {
                walk(child, &mut local, depth + 1, false)?;
            }
            let mut text = local.value.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                text = match attr(cell, "value-type") {
                    Some("string") => attr(cell, "string-value").unwrap_or_default().to_owned(),
                    Some("boolean") => attr(cell, "boolean-value").unwrap_or_default().to_owned(),
                    Some("date") => attr(cell, "date-value").unwrap_or_default().to_owned(),
                    Some("time") => attr(cell, "time-value").unwrap_or_default().to_owned(),
                    Some("percentage") => attr(cell, "value")
                        .map(|v| format!("{v} [ODF value-type=percentage]"))
                        .unwrap_or_default(),
                    Some("currency") => format!(
                        "{} {}",
                        attr(cell, "value").unwrap_or_default(),
                        attr(cell, "currency").unwrap_or_default()
                    )
                    .trim()
                    .into(),
                    _ => attr(cell, "value").unwrap_or_default().to_owned(),
                };
            }
            if !text.is_empty() {
                let end = width.saturating_add(times).min(512);
                if end > width {
                    values.push((width..end, text));
                }
                clipped |= width.saturating_add(times) > 512;
            }
            width = width.saturating_add(times);
        }
        if values.is_empty() && !clipped {
            continue;
        }
        let count = count(row, "number-rows-repeated")?;
        for _ in 0..count.min(10_001) {
            emitted += 1;
            if emitted > 10_000 {
                return Err(Truncated.into());
            }
            let mut prior = 0;
            for (range, text) in &values {
                for col in range.clone() {
                    for _ in prior..col {
                        out.push("\t")?;
                    }
                    out.push(text)?;
                    prior = col;
                }
            }
            out.push("\n")?;
            if clipped {
                return Err(Truncated.into());
            }
        }
    }
    Ok(())
}
