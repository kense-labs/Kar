//! Fold a command's control sequences into a bounded, unwrapped terminal tail.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub(super) const TAIL: usize = 64 * 1024;
const LIVE: usize = 64;

fn tail(value: &str, units: usize) -> (&str, bool) {
    if value.len() <= units {
        return (value, false);
    }
    let mut total = 0;
    for (i, ch) in value.char_indices().rev() {
        total += ch.len_utf16();
        if total > units {
            let cut = &value[i + ch.len_utf8()..];
            return match cut.find('\n').filter(|&n| n < units / 4) {
                Some(n) => (&cut[n + 1..], true),
                None => (cut, true),
            };
        }
    }
    (value, false)
}

// Rendered lines contain only canonical SGR sequences. Restore the pen if a cut falls
// inside a styled run, and never expose a partial escape or a detached combining mark.
fn display_tail(value: &str) -> (String, bool) {
    let (suffix, dropped) = tail(value, TAIL);
    if !dropped {
        return (value.to_owned(), false);
    }
    let cut = value.len() - suffix.len();
    let before = value[..cut].rsplit('\n').next().unwrap_or_default();
    let mut visible = suffix;
    let mut prefix = "";
    if let Some(escape) = before.rfind('\x1b') {
        let start = cut - (before.len() - escape);
        if let Some(end) = value[start..].find('m') {
            let sequence = &value[start..=start + end];
            if sequence != "\x1b[0m" {
                prefix = sequence;
            }
            if start + end >= cut {
                visible = &value[start + end + 1..];
            }
        }
    }
    let visible = visible.trim_start_matches(|c: char| c.width() == Some(0));
    ([prefix, visible].concat(), true)
}
#[derive(Default)]
struct Style {
    flags: u16,
    fg: String,
    bg: String,
}
impl Style {
    fn apply(&mut self, params: &Params) -> Arc<str> {
        let mut codes = vec![];
        for p in params.iter() {
            match p {
                [4, 0, ..] => codes.push(24),
                [code @ (38 | 48 | 58), 2, _, r, g, b, ..] => codes.extend([*code, 2, *r, *g, *b]),
                [code @ (38 | 48 | 58), tail @ ..] => {
                    codes.push(*code);
                    codes.extend_from_slice(tail);
                }
                [code, ..] => codes.push(*code),
                [] => {}
            }
        }
        let mut it = codes.into_iter();
        while let Some(code) = it.next() {
            match code {
                0 => *self = Self::default(),
                1 | 2 | 3 | 4 | 7 | 9 => self.flags |= 1 << code,
                22 => self.flags &= !6,
                23 | 24 | 27 | 29 => self.flags &= !(1 << (code - 20)),
                30..=37 | 90..=97 => self.fg = code.to_string(),
                40..=47 | 100..=107 => self.bg = code.to_string(),
                39 => self.fg.clear(),
                49 => self.bg.clear(),
                38 | 48 | 58 => {
                    let color = match it.next() {
                        Some(5) => it.next().map(|n| format!("{code};5;{}", n & 255)),
                        Some(2) => match (it.next(), it.next(), it.next()) {
                            (Some(r), Some(g), Some(b)) => {
                                Some(format!("{code};2;{};{};{}", r & 255, g & 255, b & 255))
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(color) = color {
                        match code {
                            38 => self.fg = color,
                            48 => self.bg = color,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        let mut set: Vec<_> = [1u16, 2, 3, 4, 7, 9]
            .into_iter()
            .filter(|n| self.flags & (1 << n) != 0)
            .map(|n| n.to_string())
            .collect();
        set.extend(
            [&self.fg, &self.bg]
                .into_iter()
                .filter(|s| !s.is_empty())
                .cloned(),
        );
        set.join(";").into()
    }
}
struct Glyph {
    text: String,
    width: usize,
    style: Arc<str>,
}
type Line = BTreeMap<usize, Glyph>;
fn render(line: &Line) -> String {
    let mut text = String::new();
    let mut col = 0;
    let mut pen = "";
    for (at, g) in line {
        if *at > col {
            if !pen.is_empty() {
                text.push_str("\x1b[0m");
                pen = "";
            }
            text.extend(std::iter::repeat_n(' ', at - col));
        }
        if pen != &*g.style {
            pen = &g.style;
            if pen.is_empty() {
                text.push_str("\x1b[0m");
            } else {
                text.push_str("\x1b[0;");
                text.push_str(pen);
                text.push('m');
            }
        }
        text.push_str(&g.text);
        col = at + g.width;
    }
    if !pen.is_empty() {
        text.push_str("\x1b[0m");
    }
    text
}
struct Screen {
    rows: VecDeque<Line>,
    row: usize,
    col: usize,
    saved: Option<(usize, usize)>,
    style: Style,
    pen: Arc<str>,
    history: String,
    truncated: bool,
}
impl Screen {
    fn position(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows.len() - 1);
        self.col = col.min(TAIL * 2);
    }
    fn linefeed(&mut self) {
        if self.row + 1 == LIVE {
            let line = self.rows.pop_front().unwrap();
            self.history.push_str(&render(&line));
            self.history.push('\n');
            if self.history.len() > TAIL * 4 {
                let (trimmed, dropped) = display_tail(&self.history);
                self.truncated |= dropped;
                self.history = trimmed;
            }
            if let Some((r, _)) = &mut self.saved {
                *r = r.saturating_sub(1);
            }
            self.row -= 1;
        }
        self.row += 1;
        if self.row == self.rows.len() {
            self.rows.push_back(Line::new());
        }
        self.col = 0;
    }
    fn erase(&mut self, mode: u16) {
        let row = &mut self.rows[self.row];
        match mode {
            0 => row.retain(|start, g| start + g.width <= self.col),
            1 => row.retain(|start, _| *start > self.col),
            2 => row.clear(),
            _ => {}
        }
    }
}
impl Perform for Screen {
    fn print(&mut self, c: char) {
        let width = if c == '\t' { 1 } else { c.width().unwrap_or(0) };
        let row = &mut self.rows[self.row];
        if width == 0 {
            if let Some((p, g)) = row.range_mut(..self.col).next_back()
                && p + g.width == self.col
            {
                g.text.push(c);
            }
            return;
        }
        if let Some((&at, g)) = row.range(..self.col).next_back()
            && at + g.width > self.col
        {
            row.remove(&at);
        }
        for at in self.col..self.col + width {
            row.remove(&at);
        }
        row.insert(
            self.col,
            Glyph {
                text: c.to_string(),
                width,
                style: self.pen.clone(),
            },
        );
        self.col += width;
    }
    fn execute(&mut self, b: u8) {
        match b {
            b'\n' => self.linefeed(),
            b'\r' => self.col = 0,
            8 => self.col = self.col.saturating_sub(1),
            b'\t' => self.print('\t'),
            _ => {}
        }
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        if action == 'm' {
            self.pen = self.style.apply(params);
            return;
        }
        let mut numbers = params.iter().map(|p| p.first().copied().unwrap_or(0));
        let mode = numbers.next().unwrap_or(0);
        let n = usize::from(mode.max(1));
        match action {
            'A' => self.position(self.row.saturating_sub(n), self.col),
            'B' => self.position(self.row + n, self.col),
            'C' => self.position(self.row, self.col + n),
            'D' => self.position(self.row, self.col.saturating_sub(n)),
            'E' => self.position(self.row + n, 0),
            'F' => self.position(self.row.saturating_sub(n), 0),
            'G' => self.position(self.row, n - 1),
            'H' | 'f' => self.position(n - 1, usize::from(numbers.next().unwrap_or(1).max(1)) - 1),
            'K' => self.erase(mode),
            'J' => match mode {
                0 => {
                    self.erase(0);
                    self.rows.truncate(self.row + 1);
                }
                1 => {
                    self.erase(1);
                    for row in self.rows.iter_mut().take(self.row) {
                        row.clear();
                    }
                }
                2 => {
                    self.rows.clear();
                    self.rows.push_back(Line::new());
                    self.position(0, 0);
                }
                3 => {
                    self.truncated |= !self.history.is_empty();
                    self.history.clear();
                }
                _ => {}
            },
            's' => self.saved = Some((self.row, self.col)),
            'u' => {
                if let Some((r, c)) = self.saved {
                    self.position(r, c)
                }
            }
            _ => {}
        }
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.saved = Some((self.row, self.col)),
            b'8' => {
                if let Some((r, c)) = self.saved {
                    self.position(r, c)
                }
            }
            b'M' => self.position(self.row.saturating_sub(1), self.col),
            _ => {}
        }
    }
}
pub(super) fn normalize(raw: &str) -> (String, bool) {
    let (raw, cut) = tail(raw, TAIL * 4);
    if !raw
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        let (text, dropped) = display_tail(raw);
        return (text, cut || dropped);
    }
    let mut screen = Screen {
        rows: VecDeque::from([Line::new()]),
        row: 0,
        col: 0,
        saved: None,
        style: Style::default(),
        pen: "".into(),
        history: String::new(),
        truncated: cut,
    };
    vte::Parser::new().advance(&mut screen, raw.as_bytes());
    let mut text = screen.history;
    for (i, row) in screen.rows.iter().enumerate() {
        if i != 0 {
            text.push('\n');
        }
        text.push_str(&render(row));
    }
    let (out, dropped) = display_tail(&text);
    (out, dropped || screen.truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_the_visible_terminal_after_cursor_updates() {
        assert_eq!(normalize("waiting\rgo\x1b[K"), ("go".into(), false));
        assert_eq!(normalize("界a\rZ"), ("Z a".into(), false));
        assert_eq!(
            normalize("a\x1b]0;private title\x07b"),
            ("ab".into(), false)
        );
        assert_eq!(normalize("a\x1b[20Bb"), ("ab".into(), false));
    }

    #[test]
    fn utf16_tail_keeps_style_and_complete_characters() {
        let styled = format!("\x1b[31m{}\x1b[0m", "🦀".repeat(TAIL));
        let (visible, dropped) = normalize(&styled);
        assert!(dropped);
        assert!(visible.starts_with("\x1b[0;31m🦀"));
        assert!(visible.ends_with("🦀\x1b[0m"));
        assert_eq!(
            display_tail(&format!("x\u{301}{}", "a".repeat(TAIL - 1))).0,
            "a".repeat(TAIL - 1)
        );
    }
}
