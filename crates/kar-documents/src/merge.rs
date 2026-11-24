use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Unchanged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change<T> {
    pub base: T,
    pub proposed: T,
}

#[derive(Clone, Debug, Error, PartialEq, Serialize, Deserialize)]
#[error("{target} ({property}): {reason}")]
pub struct Conflict {
    pub target: String,
    pub property: String,
    pub reason: String,
    pub base: Value,
    /// None means the target or property could not be located.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub current: Option<Value>,
    pub proposed: Value,
}

fn present_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Clone, Copy)]
pub(crate) enum Comparison {
    Base,
    Current,
    Adopt,
}

impl<T: PartialEq + Serialize> Change<T> {
    pub(crate) fn compare_at(
        &self,
        current: Option<&T>,
        target: &str,
        property: &str,
        comparison: Comparison,
    ) -> anyhow::Result<Outcome> {
        if matches!(comparison, Comparison::Base) {
            anyhow::ensure!(
                current == Some(&self.base),
                "edit does not match base snapshot at {target} ({property})"
            );
            return Ok(Outcome::Unchanged);
        }
        if let Some(current) = current {
            if current == &self.proposed || self.base == self.proposed {
                return Ok(Outcome::Unchanged);
            }
            if current == &self.base || matches!(comparison, Comparison::Adopt) {
                return Ok(Outcome::Applied);
            }
        }
        Err(self
            .conflict(
                current,
                target,
                property,
                if current.is_some() {
                    "concurrent property change"
                } else {
                    "target missing or ambiguous"
                },
            )
            .into())
    }

    pub(crate) fn conflict(
        &self,
        current: Option<&T>,
        target: &str,
        property: &str,
        reason: &str,
    ) -> Conflict {
        Conflict {
            target: target.into(),
            property: property.into(),
            reason: reason.into(),
            base: serde_json::json!(self.base),
            current: current.map(|value| serde_json::json!(value)),
            proposed: serde_json::json!(self.proposed),
        }
    }
}

pub(crate) fn unique_index(mut indexes: impl Iterator<Item = usize>) -> Option<usize> {
    let first = indexes.next()?;
    indexes.next().is_none().then_some(first)
}
