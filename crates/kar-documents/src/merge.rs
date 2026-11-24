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

#[derive(Debug, Error, PartialEq, Serialize, Deserialize)]
#[error("{target} ({property}): {reason}")]
pub struct Conflict {
    pub target: String,
    pub property: String,
    pub reason: String,
    pub base: Value,
    /// None means the target or property could not be located.
    pub current: Option<Value>,
    pub proposed: Value,
}

impl<T: PartialEq + Serialize> Change<T> {
    pub(crate) fn compare(
        &self,
        current: Option<&T>,
        target: &str,
        property: &str,
    ) -> anyhow::Result<Outcome> {
        if let Some(current) = current {
            if current == &self.proposed || self.base == self.proposed {
                return Ok(Outcome::Unchanged);
            }
            if current == &self.base {
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
