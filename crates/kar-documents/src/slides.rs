use crate::merge::{Change, Comparison, Outcome, unique_index};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use slides_engine::Presentation;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Object {
    pub slide_id: u32,
    pub shape_id: u32,
    pub shape_name: String,
}

impl Object {
    pub fn at(deck: &Presentation, slide: usize, shape: usize) -> Result<Self> {
        let slide = deck
            .slides()
            .get(slide)
            .ok_or_else(|| anyhow::anyhow!("unknown slide"))?;
        let shape = slide
            .shapes
            .get(shape)
            .ok_or_else(|| anyhow::anyhow!("unknown shape"))?;
        Ok(Self {
            slide_id: slide.sld_id,
            shape_id: shape.id,
            shape_name: shape.name.clone(),
        })
    }

    pub fn apply(&self, deck: &mut Presentation, edit: &ObjectEdit) -> Result<Outcome> {
        self.apply_at(deck, edit, Comparison::Current)
    }

    pub(crate) fn apply_at(
        &self,
        deck: &mut Presentation,
        edit: &ObjectEdit,
        comparison: Comparison,
    ) -> Result<Outcome> {
        let index = unique_index(
            deck.slides()
                .iter()
                .enumerate()
                .filter_map(|(i, slide)| (slide.sld_id == self.slide_id).then_some(i)),
        );
        let indexes = index.and_then(|i| {
            let j = unique_index(
                deck.slide(i)
                    .shapes
                    .iter()
                    .enumerate()
                    .filter_map(|(j, shape)| (shape.id == self.shape_id).then_some(j)),
            )?;
            Some((i, j))
        });
        let shape = indexes.map(|(i, j)| &deck.slide(i).shapes[j]);
        let name = shape.map(|s| &s.name).unwrap_or(&self.shape_name);
        let target = format!(
            "slide {} / {} (shape {})",
            self.slide_id, name, self.shape_id
        );
        match edit {
            ObjectEdit::Text(change) => {
                ensure!(
                    change
                        .proposed
                        .chars()
                        .all(|c| !c.is_control() || matches!(c, '\t' | '\n')),
                    "invalid shape text"
                );
                let value = shape.and_then(|s| s.text.as_ref()).map(|v| v.plain_text());
                let result = change.compare_at(value.as_ref(), &target, "text", comparison)?;
                if let (Outcome::Applied, Some(r)) = (result, indexes) {
                    deck.set_shape_text(r, &change.proposed)?;
                }
                Ok(result)
            }
            ObjectEdit::Fill(change) => {
                ensure!(
                    change.proposed.is_none_or(|c| c <= 0xff_ffff),
                    "invalid RGB fill"
                );
                let value = shape.and_then(solid_fill);
                let result = change.compare_at(value.as_ref(), &target, "fill", comparison)?;
                if let (Outcome::Applied, Some(r)) = (result, indexes) {
                    let paint = change
                        .proposed
                        .map(|c| {
                            slides_engine::Paint::Rgb(slides_engine::Rgb([
                                (c >> 16) as u8,
                                (c >> 8) as u8,
                                c as u8,
                            ]))
                        })
                        .unwrap_or(slides_engine::Paint::None);
                    deck.set_fill(r, paint)?;
                }
                Ok(result)
            }
            ObjectEdit::X(change)
            | ObjectEdit::Y(change)
            | ObjectEdit::Width(change)
            | ObjectEdit::Height(change) => {
                let transform =
                    indexes.and_then(|(i, j)| deck.effective_xfrm(i, &deck.slide(i).shapes[j]));
                let (property, current) = match edit {
                    ObjectEdit::X(_) => ("x", transform.map(|v| v.x)),
                    ObjectEdit::Y(_) => ("y", transform.map(|v| v.y)),
                    ObjectEdit::Width(_) => ("width", transform.map(|v| v.cx)),
                    ObjectEdit::Height(_) => ("height", transform.map(|v| v.cy)),
                    ObjectEdit::Text(_) | ObjectEdit::Fill(_) => unreachable!(),
                };
                let result = change.compare_at(current.as_ref(), &target, property, comparison)?;
                if let (Outcome::Applied, Some(r), Some(mut v)) = (result, indexes, transform) {
                    match edit {
                        ObjectEdit::X(_) => v.x = change.proposed,
                        ObjectEdit::Y(_) => v.y = change.proposed,
                        ObjectEdit::Width(_) => v.cx = change.proposed,
                        ObjectEdit::Height(_) => v.cy = change.proposed,
                        ObjectEdit::Text(_) | ObjectEdit::Fill(_) => unreachable!(),
                    }
                    ensure!(v.cx > 0 && v.cy > 0, "invalid shape dimensions");
                    deck.set_xfrm(r, v, false)?;
                }
                Ok(result)
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ObjectEdit {
    Text(Change<String>),
    Fill(Change<Option<u32>>),
    X(Change<i64>),
    Y(Change<i64>),
    Width(Change<i64>),
    Height(Change<i64>),
}

pub(crate) fn solid_fill(shape: &slides_engine::Shape) -> Option<Option<u32>> {
    use slides_engine::{Fill, Paint, Rgb};
    let rgb = |Rgb([r, g, b])| u32::from_be_bytes([0, r, g, b]);
    match &shape.paint.fill {
        Some(Paint::Rgb(c)) => Some(Some(rgb(*c))),
        Some(Paint::None) => Some(None),
        Some(_) => None,
        None => match &shape.look.fill_as_read {
            Some(Fill::None) => Some(None),
            Some(Fill::Solid(c, 255)) => Some(Some(rgb(*c))),
            Some(_) => None,
            None => Some(shape.look.fill.map(rgb)),
        },
    }
}
