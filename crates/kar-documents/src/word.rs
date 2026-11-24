use crate::{Change, Outcome, merge::Comparison, replace_word_text};
use anyhow::{Result, ensure};
use docs_engine::Document;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Range;

/// Identifies a UTF-8 byte range in the immutable base document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WordEdit {
    pub paragraph: usize,
    pub range: Range<usize>,
    pub replacement: String,
}

impl WordEdit {
    pub fn apply(&self, base: &Document, current: &mut Document) -> Result<Outcome> {
        self.apply_at(base, current, Comparison::Current, false)
    }

    pub(crate) fn apply_at(
        &self,
        base: &Document,
        current: &mut Document,
        comparison: Comparison,
        paragraphs_stable: bool,
    ) -> Result<Outcome> {
        let old = base
            .paragraphs()
            .get(self.paragraph)
            .ok_or_else(|| anyhow::anyhow!("unknown base paragraph"))?;
        let selected = old
            .text
            .get(self.range.clone())
            .ok_or_else(|| anyhow::anyhow!("invalid UTF-8 range in base paragraph"))?;
        ensure!(
            !self.replacement.contains(['\r', '\n']),
            "a paragraph text edit cannot split the paragraph"
        );
        let change = Change {
            base: selected.to_owned(),
            proposed: self.replacement.clone(),
        };
        let label = format!(
            "paragraph {} / bytes {}..{}",
            self.paragraph + 1,
            self.range.start,
            self.range.end
        );
        let index = if paragraphs_stable {
            current
                .paragraphs()
                .get(self.paragraph)
                .map(|_| self.paragraph)
        } else {
            map_paragraph(base, current, self.paragraph)
        }
        .ok_or_else(|| {
            change.conflict(
                None,
                &label,
                "text",
                "paragraph deleted, split or ambiguous",
            )
        })?;
        let text = &current.paragraphs()[index].text;
        if &old.text != text {
            let mut proposed = old.text.clone();
            proposed.replace_range(self.range.clone(), &self.replacement);
            if &proposed == text {
                return Ok(Outcome::Unchanged);
            }
        }
        let mapped = map_range(&old.text, text, self.range.clone()).ok_or_else(|| {
            change.conflict(Some(text), &label, "text", "overlapping text changes")
        })?;
        let value = text[mapped.clone()].to_owned();
        let outcome = change.compare_at(Some(&value), &label, "text", comparison)?;
        if outcome == Outcome::Applied {
            replace_word_text(current, index, mapped, &value, &self.replacement)?;
        }
        Ok(outcome)
    }
}

// Bound the paragraph with unchanged neighbors which occur exactly once
// in both revisions. Equal paragraphs outside those bounds are unrelated.
fn map_paragraph(base: &Document, current: &Document, index: usize) -> Option<usize> {
    let old: Vec<_> = base.paragraphs().iter().map(|p| p.text.as_str()).collect();
    let new: Vec<_> = current
        .paragraphs()
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    let positions = |texts: &[&str]| {
        let mut map = HashMap::new();
        for (i, &text) in texts.iter().enumerate() {
            map.entry(text.to_owned())
                .and_modify(|p| *p = None)
                .or_insert(Some(i));
        }
        map
    };
    let base_positions = positions(&old);
    let current_positions = positions(&new);
    let anchor = |i: usize| {
        base_positions[old[i]]?;
        current_positions
            .get(old[i])
            .copied()
            .flatten()
            .map(|j| (i, j))
    };
    let (old_lo, new_lo) = (0..index)
        .rev()
        .find_map(anchor)
        .map_or((0, 0), |(a, b)| (a + 1, b + 1));
    let (old_hi, new_hi) = (index + 1..old.len())
        .find_map(anchor)
        .unwrap_or((old.len(), new.len()));
    let source = old.get(old_lo..old_hi)?;
    let latest = new.get(new_lo..new_hi)?;
    let slot = index - old_lo;
    if source.len() == latest.len()
        && source[..slot] == latest[..slot]
        && source[slot + 1..] == latest[slot + 1..]
    {
        return Some(new_lo + slot);
    }
    if source.iter().filter(|p| **p == old[index]).count() == 1 {
        return crate::merge::unique_index(
            latest
                .iter()
                .enumerate()
                .filter_map(|(i, p)| (*p == old[index]).then_some(new_lo + i)),
        );
    }
    None
}

// Translate through one changed span, narrowing it on scalar boundaries.
// For an overlap only compare a selection whose two boundaries still map.
fn map_range(base: &str, current: &str, range: Range<usize>) -> Option<Range<usize>> {
    if base == current {
        return Some(range);
    }
    let prefix: usize = base
        .chars()
        .zip(current.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let suffix: usize = base[prefix..]
        .chars()
        .rev()
        .zip(current[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let old_end = base.len() - suffix;
    let new_end = current.len() - suffix;
    if range.is_empty() && range.start == prefix && prefix == old_end {
        return Some(prefix..new_end);
    }
    if range.end <= prefix {
        return Some(range);
    }
    if range.start >= old_end {
        return Some(new_end + (range.start - old_end)..new_end + (range.end - old_end));
    }
    if range.start <= prefix && range.end >= old_end {
        return Some(range.start..new_end + (range.end - old_end));
    }
    None
}

/// Direct OOXML run formatting. A None value inherits from the style.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum WordStyle {
    Bold,
    Italic,
    Underline,
}

impl WordStyle {
    pub(crate) fn read(self, run: &docs_engine::RunProps) -> Option<bool> {
        match self {
            Self::Bold => run.bold,
            Self::Italic => run.italic,
            Self::Underline => run.underline,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::Underline => "underline",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WordStyleEdit {
    pub paragraph: usize,
    pub range: Range<usize>,
    pub property: WordStyle,
    pub change: Change<Option<bool>>,
}

impl WordStyleEdit {
    fn value(
        &self,
        paragraph: &docs_engine::Paragraph,
        range: &Range<usize>,
    ) -> Option<Option<bool>> {
        let mut runs = paragraph
            .run_ranges()
            .filter(|(s, _)| s.start < range.end && range.start < s.end)
            .map(|(_, run)| self.property.read(&run.props));
        let first = runs.next()?;
        runs.all(|next| next == first).then_some(first)
    }

    pub(crate) fn apply(
        &self,
        base: &Document,
        current: &mut Document,
        comparison: Comparison,
        stable: bool,
    ) -> Result<Outcome> {
        let p = base
            .paragraphs()
            .get(self.paragraph)
            .ok_or_else(|| anyhow::anyhow!("unknown base paragraph"))?;
        ensure!(
            !self.range.is_empty() && p.text.get(self.range.clone()).is_some(),
            "invalid character formatting range"
        );
        let label = format!(
            "paragraph {} / bytes {}..{}",
            self.paragraph + 1,
            self.range.start,
            self.range.end
        );
        let name = self.property.name();
        self.change.compare_at(
            self.value(p, &self.range).as_ref(),
            &label,
            name,
            Comparison::Base,
        )?;
        let i = if stable {
            Some(self.paragraph)
        } else {
            map_paragraph(base, current, self.paragraph)
        };
        let (i, q) = i
            .and_then(|i| current.paragraphs().get(i).map(|q| (i, q)))
            .ok_or_else(|| {
                self.change
                    .conflict(None, &label, name, "paragraph deleted, split or ambiguous")
            })?;
        let range = map_range(&p.text, &q.text, self.range.clone())
            .filter(|r| !r.is_empty())
            .ok_or_else(|| {
                self.change
                    .conflict(None, &label, name, "formatted range missing or ambiguous")
            })?;
        let value = self.value(q, &range);
        let result = self
            .change
            .compare_at(value.as_ref(), &label, name, comparison)?;
        if result == Outcome::Applied {
            let sel = docs_engine::Selection {
                anchor: docs_engine::Position::new(i, range.start),
                head: docs_engine::Position::new(i, range.end),
            };
            current.format(sel, |r| match self.property {
                WordStyle::Bold => r.bold = self.change.proposed,
                WordStyle::Italic => r.italic = self.change.proposed,
                WordStyle::Underline => r.underline = self.change.proposed,
            });
        }
        Ok(result)
    }
}
