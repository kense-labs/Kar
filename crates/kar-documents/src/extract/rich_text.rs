use super::{Text, Truncated};
use anyhow::{Result, ensure};
use encoding_rs::Encoding;
use serde_json::Value;

fn fragments(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}
// Notebook output can contain terminal colouring and control strings.
fn plain(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.next_if_eq(&'\\').is_some() {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' | '\n' => out.push(c),
            c if !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out
}
pub(super) fn notebook(bytes: &[u8], out: &mut Text) -> Result<()> {
    let book: Value = serde_json::from_slice(bytes)?;
    let cells = book["cells"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing notebook cells"))?;
    for (i, cell) in cells.iter().enumerate() {
        if i >= 10_000 {
            return Err(Truncated.into());
        }
        if !cell.is_object() {
            continue;
        }
        let kind = match cell["cell_type"].as_str() {
            Some("raw") => "raw",
            Some("code") => "code",
            _ => "markdown",
        };
        out.push(&format!("## Cell {} ({kind})\n", i + 1))?;
        out.push(&fragments(&cell["source"]))?;
        if let Some(outputs) = cell["outputs"].as_array() {
            out.push("\nOutput:\n")?;
            for value in outputs {
                if let Some(text) = value
                    .get("text")
                    .or_else(|| value["data"].get("text/plain"))
                {
                    out.push(&plain(&fragments(text)))?;
                } else if value["output_type"] == "error" {
                    out.push(&fragments(&value["ename"]))?;
                    out.push(": ")?;
                    out.push(&fragments(&value["evalue"]))?;
                    out.push("\n")?;
                    if let Some(lines) = value["traceback"].as_array() {
                        for line in lines {
                            out.push(&plain(&fragments(line)))?;
                            out.push("\n")?;
                        }
                    }
                }
                out.push("\n")?;
            }
        }
        out.push("\n\n")?;
    }
    Ok(())
}
#[derive(Clone, Copy)]
struct Group {
    skip: bool,
    hidden: bool,
    fallback: usize,
    font: i32,
    encoding: &'static Encoding,
    fonts: bool,
}
pub(super) fn rtf(bytes: &[u8], out: &mut Text) -> Result<()> {
    ensure!(bytes.starts_with(b"{\\rtf"), "invalid RTF");
    let mut state = Group {
        skip: false,
        hidden: false,
        fallback: 1,
        font: 0,
        encoding: encoding_rs::WINDOWS_1252,
        fonts: false,
    };
    let mut stack = Vec::new();
    let mut fonts: std::collections::HashMap<i32, &'static Encoding> =
        std::collections::HashMap::new();
    let mut fallback = 0;
    let mut i = 0;
    let mut pending_high = None;
    while i < bytes.len() {
        out.check()?;
        let b = bytes[i];
        i += 1;
        let mut visible = None;
        match b {
            b'{' => {
                ensure!(stack.len() < 1024, "RTF nesting limit");
                stack.push(state);
            }
            b'}' => {
                state = stack
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("invalid RTF grouping"))?;
            }
            b'\n' | b'\r' => {}
            b'\\' => {
                let start = i;
                let Some(next) = bytes.get(i).copied() else {
                    break;
                };
                if !next.is_ascii_alphabetic() {
                    i += 1;
                    match next {
                        b'*' => state.skip = true,
                        b'\'' => {
                            let code = std::str::from_utf8(
                                bytes
                                    .get(i..i + 2)
                                    .ok_or_else(|| anyhow::anyhow!("incomplete RTF byte"))?,
                            )?;
                            i += 2;
                            let byte = u8::from_str_radix(code, 16)?;
                            visible = Some(
                                state
                                    .encoding
                                    .decode_without_bom_handling(&[byte])
                                    .0
                                    .into_owned(),
                            );
                        }
                        b'{' | b'}' | b'\\' => visible = Some(char::from(next).to_string()),
                        b'~' => visible = Some("\u{a0}".into()),
                        b'_' => visible = Some("‑".into()),
                        b'-' => visible = Some("\u{ad}".into()),
                        b'\r' | b'\n' => visible = Some("\n".into()),
                        _ => {}
                    }
                } else {
                    while bytes.get(i).is_some_and(u8::is_ascii_alphabetic) {
                        i += 1;
                    }
                    let word = &bytes[start..i];
                    let sign = i;
                    if bytes.get(i) == Some(&b'-') {
                        i += 1;
                    }
                    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                        i += 1;
                    }
                    let n = std::str::from_utf8(&bytes[sign..i])
                        .ok()
                        .and_then(|s| s.parse::<i32>().ok());
                    if bytes.get(i) == Some(&b' ') {
                        i += 1;
                    }
                    match word {
                        b"fonttbl" => {
                            state.skip = true;
                            state.fonts = true;
                        }
                        b"colortbl" | b"stylesheet" | b"info" | b"pict" | b"object"
                        | b"objdata" | b"datastore" | b"xmlnstbl" | b"fldinst" | b"header"
                        | b"footer" | b"shppict" | b"nonshppict" | b"listtable"
                        | b"listoverridetable" | b"generator" | b"filetbl" | b"themedata" => {
                            state.skip = true
                        }
                        b"v" | b"deleted" => state.hidden = n != Some(0),
                        b"uc" => state.fallback = n.unwrap_or(1).clamp(0, 127) as usize,
                        b"ansicpg" | b"cpg" => {
                            if let Some(enc) = n.and_then(|n| {
                                Encoding::for_label(format!("windows-{n}").as_bytes())
                            }) {
                                state.encoding = enc;
                            }
                        }
                        b"f" => {
                            state.font = n.unwrap_or(0);
                            if let Some(enc) = fonts.get(&state.font) {
                                state.encoding = enc;
                            }
                        }
                        b"fcharset" if state.fonts => {
                            let label = match n {
                                Some(77) => "macintosh",
                                Some(161) => "windows-1253",
                                Some(162) => "windows-1254",
                                Some(163) => "windows-1258",
                                Some(177) => "windows-1255",
                                Some(178) => "windows-1256",
                                Some(186) => "windows-1257",
                                Some(204) => "windows-1251",
                                Some(222) => "windows-874",
                                Some(238) => "windows-1250",
                                _ => "windows-1252",
                            };
                            if let Some(enc) = Encoding::for_label(label.as_bytes()) {
                                fonts.insert(state.font, enc);
                            }
                        }
                        b"bin" => {
                            let length = usize::try_from(n.unwrap_or(-1))?;
                            ensure!(length <= bytes.len() - i, "short RTF binary");
                            i += length;
                        }
                        b"u" if !state.skip && !state.hidden => {
                            let unit = n.unwrap_or(0) as u16;
                            if let Some(high) = pending_high.take() {
                                if (0xdc00..=0xdfff).contains(&unit) {
                                    out.push(&String::from_utf16_lossy(&[high, unit]))?;
                                    fallback = state.fallback;
                                    continue;
                                }
                                out.push("�")?;
                            }
                            if (0xd800..=0xdbff).contains(&unit) {
                                pending_high = Some(unit);
                            } else {
                                out.push(&String::from_utf16_lossy(&[unit]))?;
                            }
                            fallback = state.fallback;
                        }
                        b"line" | b"row" | b"par" => visible = Some("\n".into()),
                        b"tab" | b"cell" => visible = Some("\t".into()),
                        b"emdash" => visible = Some("—".into()),
                        b"endash" => visible = Some("–".into()),
                        b"bullet" => visible = Some("•".into()),
                        b"lquote" => visible = Some("‘".into()),
                        b"rquote" => visible = Some("’".into()),
                        b"ldblquote" => visible = Some("“".into()),
                        b"rdblquote" => visible = Some("”".into()),
                        _ => {}
                    }
                }
            }
            c => {
                visible = Some(
                    state
                        .encoding
                        .decode_without_bom_handling(&[c])
                        .0
                        .into_owned(),
                )
            }
        }
        if let Some(text) = visible {
            if fallback > 0 {
                fallback -= 1;
            } else if !state.skip && !state.hidden {
                if pending_high.take().is_some() {
                    out.push("�")?;
                }
                out.push(&text)?;
            }
        }
    }
    ensure!(stack.is_empty(), "incomplete RTF");
    Ok(())
}
