//! UTF-8 selections use byte offsets into the immutable base revision.
use crate::{Change, Outcome, merge::Comparison};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffTag, capture_diff_slices};
use std::ops::Range;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
}

fn boundaries<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<usize> {
    let mut offsets = vec![0];
    let mut bytes = 0;
    for part in parts {
        bytes += part.len();
        offsets.push(bytes);
    }
    offsets
}

fn changes(base: &str, current: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let a: Vec<_> = base.split_inclusive('\n').collect();
    let b: Vec<_> = current.split_inclusive('\n').collect();
    let ao = boundaries(a.iter().copied());
    let bo = boundaries(b.iter().copied());
    let mut spans = Vec::new();
    for line in capture_diff_slices(Algorithm::Patience, &a, &b) {
        let (tag, x, y) = line.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let old = ao[x.start]..ao[x.end];
        let new = bo[y.start]..bo[y.end];
        if old.is_empty() || new.is_empty() {
            spans.push((old, new));
            continue;
        }
        let a = &base[old.clone()];
        let b = &current[new.clone()];
        let chars_a: Vec<_> = a.chars().collect();
        let chars_b: Vec<_> = b.chars().collect();
        let aa: Vec<_> = a
            .char_indices()
            .map(|(i, _)| i + old.start)
            .chain(std::iter::once(old.end))
            .collect();
        let bb: Vec<_> = b
            .char_indices()
            .map(|(i, _)| i + new.start)
            .chain(std::iter::once(new.end))
            .collect();
        for ch in capture_diff_slices(Algorithm::Myers, &chars_a, &chars_b) {
            let (tag, x, y) = ch.as_tag_tuple();
            if tag != DiffTag::Equal {
                spans.push((aa[x.start]..aa[x.end], bb[y.start]..bb[y.end]));
            }
        }
    }
    spans
}

fn translated(base: &str, current: &str, range: Range<usize>) -> Option<Range<usize>> {
    if base == current {
        return Some(range);
    }
    let spans = changes(base, current);
    if range.is_empty()
        && let Some((_, new)) = spans.iter().find(|(old, _)| old == &range)
    {
        return Some(new.clone());
    }
    let offset = |position: usize, start: bool| {
        let (mut previous_old, mut previous_new) = (0, 0);
        for (a, b) in &spans {
            if position < a.start {
                break;
            }
            if position == a.start {
                return Some(if start && a.is_empty() {
                    b.end
                } else {
                    b.start
                });
            }
            if position < a.end {
                return None;
            }
            previous_old = a.end;
            previous_new = b.end;
        }
        Some(previous_new + (position - previous_old))
    };
    Some(offset(range.start, true)?..offset(range.end, false)?)
}

impl TextEdit {
    pub fn between(base: &str, proposal: &str) -> Vec<Self> {
        changes(base, proposal)
            .into_iter()
            .map(|(range, new)| Self {
                range,
                replacement: proposal[new].to_owned(),
            })
            .collect()
    }

    pub(crate) fn apply(
        &self,
        base: &str,
        current: &mut String,
        mode: Comparison,
    ) -> Result<Outcome> {
        let original = base
            .get(self.range.clone())
            .context("invalid UTF-8 range in base text")?;
        let mut expected = base.to_owned();
        expected.replace_range(self.range.clone(), &self.replacement);
        if current == &expected {
            return Ok(Outcome::Unchanged);
        }
        let change = Change {
            base: original.to_owned(),
            proposed: self.replacement.clone(),
        };
        let label = format!(
            "line {} / bytes {}..{}",
            base[..self.range.start]
                .bytes()
                .filter(|c| *c == b'\n')
                .count()
                + 1,
            self.range.start,
            self.range.end
        );
        let span = translated(base, current, self.range.clone()).ok_or_else(|| {
            change.conflict(Some(current), &label, "text", "overlapping text changes")
        })?;
        let selected = current
            .get(span.clone())
            .context("ambiguous UTF-8 range after concurrent edit")?
            .to_owned();
        let outcome = change.compare_at(Some(&selected), &label, "text", mode)?;
        if outcome == Outcome::Applied {
            current.replace_range(span, &self.replacement)
        }
        Ok(outcome)
    }
}
