use super::{Parts, Result, Text, Truncated, attr, nodes, office_main, relation_id, rels, xml};
use anyhow::anyhow;
use roxmltree::Node;
use std::collections::BTreeMap;

fn decode_string(s: &str) -> String {
    let mut tail = s;
    let mut units = Vec::new();
    while let Some((before, after)) = tail.split_once("_x") {
        units.extend(before.encode_utf16());
        if let Some(hex) = after.get(..4)
            && after.as_bytes().get(4) == Some(&b'_')
            && let Ok(unit) = u16::from_str_radix(hex, 16)
        {
            units.push(unit);
            tail = &after[5..];
        } else {
            units.extend([b'_' as u16, b'x' as u16]);
            tail = after;
        }
    }
    units.extend(tail.encode_utf16());
    String::from_utf16_lossy(&units)
}
fn string(n: Node<'_, '_>) -> String {
    let mut value = String::new();
    for t in nodes(n, "t") {
        if !t
            .ancestors()
            .any(|a| a.is_element() && a.tag_name().name() == "rPh")
        {
            value.push_str(&decode_string(t.text().unwrap_or_default()));
        }
    }
    value
}
pub(super) fn excel(files: &Parts, out: &mut Text) -> Result<()> {
    let main = office_main(files, "xl/workbook.xml");
    let book = xml(files, &main)?;
    let links = rels(files, &main)?;
    let target = |kind: &str, fallback: &str| {
        links
            .values()
            .find(|r| r.kind.rsplit('/').next() == Some(kind))
            .map(|r| r.target.clone())
            .unwrap_or_else(|| fallback.into())
    };
    let strings = files
        .contains_key(&target("sharedStrings", "xl/sharedStrings.xml"))
        .then(|| xml(files, &target("sharedStrings", "xl/sharedStrings.xml")))
        .transpose()?;
    let strings: Vec<_> = strings
        .iter()
        .flat_map(|s| nodes(s.root(), "si"))
        .map(string)
        .collect();
    let styles = files
        .contains_key(&target("styles", "xl/styles.xml"))
        .then(|| xml(files, &target("styles", "xl/styles.xml")))
        .transpose()?;
    let formats: Vec<_> = styles
        .iter()
        .filter_map(|s| nodes(s.root(), "cellXfs").next())
        .flat_map(|p| p.children())
        .filter(|n| n.is_element() && n.tag_name().name() == "xf")
        .map(|n| attr(n, "numFmtId").unwrap_or("0"))
        .collect();
    let custom: BTreeMap<_, _> = styles
        .iter()
        .flat_map(|s| nodes(s.root(), "numFmt"))
        .filter_map(|n| Some((attr(n, "numFmtId")?, attr(n, "formatCode")?)))
        .collect();
    let epoch = if nodes(book.root(), "workbookPr")
        .any(|n| matches!(attr(n, "date1904"), Some("true" | "1")))
    {
        "1904"
    } else {
        "1900"
    };
    for sheet in nodes(book.root(), "sheet") {
        out.push(&format!(
            "\n## Sheet: {}\n",
            attr(sheet, "name").unwrap_or_default()
        ))?;
        let part = relation_id(sheet)
            .and_then(|id| links.get(id))
            .ok_or_else(|| anyhow!("missing sheet relationship"))?;
        let doc = xml(files, &part.target)?;
        let mut lines = 0;
        for row in nodes(doc.root(), "row") {
            out.check()?;
            let mut values = BTreeMap::new();
            let mut next = 0;
            let mut clipped = false;
            for cell in row
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "c")
            {
                let col = attr(cell, "r")
                    .and_then(|r| {
                        let mut letters = r.bytes().take_while(u8::is_ascii_uppercase).peekable();
                        letters.peek()?;
                        Some(
                            letters
                                .fold(0usize, |a, b| {
                                    a.saturating_mul(26)
                                        .saturating_add(usize::from(b - b'A' + 1))
                                })
                                .saturating_sub(1),
                        )
                    })
                    .unwrap_or(next);
                next = col.saturating_add(1);
                let raw = nodes(cell, "v")
                    .next()
                    .and_then(|n| n.text())
                    .unwrap_or_default();
                let populated = match attr(cell, "t") {
                    Some("s") => raw
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| strings.get(i))
                        .is_some_and(|s| !s.trim().is_empty()),
                    Some("inlineStr") => {
                        nodes(cell, "t").any(|n| n.text().is_some_and(|s| !s.trim().is_empty()))
                    }
                    _ => {
                        !raw.trim().is_empty()
                            || nodes(cell, "f")
                                .any(|n| n.text().is_some_and(|s| !s.trim().is_empty()))
                    }
                };
                if populated && col < 512 {
                    values.insert(col, cell);
                } else if populated {
                    clipped = true;
                }
            }
            if values.is_empty() && !clipped {
                continue;
            }
            lines += 1;
            if lines > 10_000 {
                return Err(Truncated.into());
            }
            let mut cursor = 0;
            for (col, cell) in values {
                let raw = nodes(cell, "v")
                    .next()
                    .and_then(|n| n.text())
                    .unwrap_or_default();
                let mut value = match attr(cell, "t") {
                    Some("s") => raw
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| strings.get(i))
                        .cloned()
                        .unwrap_or_default(),
                    Some("inlineStr") => string(cell),
                    Some("b") if raw.trim() == "0" => "false".into(),
                    Some("b") if raw.trim() == "1" => "true".into(),
                    _ if raw.is_empty() => nodes(cell, "f")
                        .next()
                        .and_then(|n| n.text())
                        .unwrap_or_default()
                        .to_owned(),
                    _ => decode_string(raw),
                };
                let format = attr(cell, "s")
                    .unwrap_or("0")
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| formats.get(i))
                    .copied()
                    .unwrap_or("0");
                if !raw.is_empty() && format != "0" && matches!(attr(cell, "t"), None | Some("n")) {
                    let code = custom
                        .get(format)
                        .map(|c| format!("; format={c}"))
                        .unwrap_or_default();
                    value.push_str(&format!(
                        " [Excel numFmtId={format}{code}; date system {epoch}]"
                    ));
                }
                for _ in cursor..col {
                    out.push("\t")?;
                }
                let mut words = value.split_whitespace();
                if let Some(first) = words.next() {
                    out.push(first)?;
                }
                for word in words {
                    out.push(" ")?;
                    out.push(word)?;
                }
                cursor = col;
            }
            out.push("\n")?;
            if clipped {
                return Err(Truncated.into());
            }
        }
    }
    Ok(())
}
