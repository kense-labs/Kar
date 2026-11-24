//! Native picker observations for Claude and Factory.
use crate::{Error, Protocol, Result, forms::ModelEntry};
use serde_json::{Map, Value, json};

pub(crate) fn project(rows: &Value, protocol: Protocol) -> Result<Vec<ModelEntry>> {
    let rows = rows
        .as_array()
        .ok_or_else(|| Error::Protocol("model control omitted its catalog".into()))?;
    let mut entries = Vec::new();
    for row in rows {
        let mut metadata = Map::new();
        let (selector, effort_field, disabled_reason) = match protocol {
            Protocol::Claude => {
                if let Some(resolved) = row["resolvedModel"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                {
                    metadata.insert("resolvedId".into(), resolved.trim().into());
                }
                if row["supportsFastMode"] == true {
                    metadata.insert("serviceTiers".into(), json!(["fast"]));
                }
                (
                    "value",
                    "supportedEffortLevels",
                    row["description"].as_str().unwrap_or("disabled by Claude"),
                )
            }
            Protocol::Factory => {
                if let Some(default) = level(&row["defaultReasoningEffort"]) {
                    metadata.insert("defaultEffort".into(), default.into());
                }
                (
                    "id",
                    "supportedReasoningEfforts",
                    row["disabledReason"]
                        .as_str()
                        .unwrap_or("droid reports this model disabled"),
                )
            }
            _ => {
                return Err(Error::Unsupported(
                    "unrecognized native model catalog".into(),
                ));
            }
        };
        let Some(id) = row[selector]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if let Some(name) = row["displayName"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            metadata.insert("displayName".into(), name.into());
        }
        let effort_levels = row[effort_field]
            .as_array()
            .map(|levels| levels.iter().filter_map(level).map(str::to_owned).collect());
        entries.push(ModelEntry {
            id: id.into(),
            default: false,
            effort_levels,
            metadata,
            disabled: (row["disabled"] == true).then(|| json!({"reason":disabled_reason})),
        });
    }
    Ok(entries)
}
fn level(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value["effort"].as_str())
        .or_else(|| value["id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_picker_retains_selector_disabled_reason_and_effort() {
        let rows = json!([{"value":"sonnet","resolvedModel":"resolved-a","disabled":true,"description":"upgrade required",
            "supportedEffortLevels":["low",{"effort":"high"}],"supportsFastMode":true}, {"value":"  "}]);
        let claude = project(&rows, Protocol::Claude).unwrap();
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].metadata["resolvedId"], "resolved-a");
        assert_eq!(claude[0].metadata["serviceTiers"], json!(["fast"]));
        assert_eq!(
            claude[0].disabled.as_ref().unwrap()["reason"],
            "upgrade required"
        );
        let rows = json!([{"id":"custom:example","supportedReasoningEfforts":["low",{"id":"high"}],
            "defaultReasoningEffort":{"effort":"low"},"disabled":false}]);
        let factory = project(&rows, Protocol::Factory).unwrap();
        assert!(factory[0].disabled.is_none());
        assert_eq!(factory[0].effort_levels, claude[0].effort_levels);
        assert_eq!(factory[0].metadata["defaultEffort"], "low");
    }
}
