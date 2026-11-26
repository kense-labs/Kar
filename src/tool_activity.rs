use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Unknown,
}

/// A path whose change the provider confirmed. A turn-wide patch is a separate event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(default, alias = "moveTo", skip_serializing_if = "Option::is_none")]
    pub move_to: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputPart {
    Text {
        text: String,
    },
    Image {
        #[serde(alias = "mediaType")]
        media_type: String,
        data: String,
    },
    Other {
        value: Value,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: Option<ToolStatus>,
    pub exit_code: Option<i64>,
    /// None means no output was reported. An empty list means a reported empty result.
    pub content: Option<Vec<OutputPart>>,
    #[serde(default)]
    pub file_changes: Vec<FileChange>,
}

impl ToolResult {
    /// Join native text parts in their original order. Non-text parts remain in `content`.
    pub fn text(&self) -> Option<String> {
        let chunks: Vec<_> = self
            .content
            .as_ref()?
            .iter()
            .filter_map(|part| match part {
                OutputPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        (!chunks.is_empty()).then(|| chunks.join("\n"))
    }
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod durable;

pub(crate) fn parts(value: &Value) -> Option<Vec<OutputPart>> {
    if value.is_null() {
        return None;
    }
    let one = |v: &Value| {
        if let Some(text) = v
            .as_str()
            .or_else(|| (v["type"] == "text").then(|| v["text"].as_str()).flatten())
        {
            return OutputPart::Text { text: text.into() };
        }
        if v["type"] == "image" {
            let src = if v["source"].is_object() {
                &v["source"]
            } else {
                v
            };
            if let (Some(mime), Some(data)) = (
                src["mimeType"].as_str().or(src["media_type"].as_str()),
                src["data"].as_str(),
            ) {
                return OutputPart::Image {
                    media_type: mime.into(),
                    data: data.into(),
                };
            }
        }
        OutputPart::Other { value: v.clone() }
    };
    Some(match value.as_array() {
        Some(list) => list.iter().map(one).collect(),
        None => vec![one(value)],
    })
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn mcp_raw_parts(raw: &Value, status: Option<&ToolStatus>) -> Option<Vec<OutputPart>> {
    if raw["type"] != "MCP" {
        return None;
    }
    let output = raw["output"].as_object()?;
    let has_ok = output.contains_key("OkayOutput");
    let has_err = output.contains_key("Error");
    if has_ok && has_err {
        return None;
    }
    let text = match status {
        Some(ToolStatus::Failed) if has_err => output.get("Error").and_then(Value::as_str),
        Some(ToolStatus::Succeeded) | None if has_ok => {
            output.get("OkayOutput").and_then(Value::as_str)
        }
        _ => None,
    }
    .filter(|s| !s.is_empty())?;
    Some(vec![OutputPart::Text {
        text: text.to_owned(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_raw_parts_projects_okay_and_error_text_by_status() {
        let ok = json!({"type": "MCP", "output": {"OkayOutput": "mcp ok"}});
        let err = json!({"type": "MCP", "output": {"Error": "mcp failed"}});
        let both = json!({"type": "MCP", "output": {"OkayOutput": "a", "Error": "b"}});

        assert_eq!(
            mcp_raw_parts(&ok, Some(&ToolStatus::Succeeded)),
            Some(vec![OutputPart::Text {
                text: "mcp ok".into(),
            }])
        );
        assert_eq!(
            mcp_raw_parts(&ok, None),
            Some(vec![OutputPart::Text {
                text: "mcp ok".into(),
            }])
        );
        assert!(mcp_raw_parts(&ok, Some(&ToolStatus::Failed)).is_none());
        assert!(mcp_raw_parts(&ok, Some(&ToolStatus::Cancelled)).is_none());

        assert_eq!(
            mcp_raw_parts(&err, Some(&ToolStatus::Failed)),
            Some(vec![OutputPart::Text {
                text: "mcp failed".into(),
            }])
        );
        assert!(mcp_raw_parts(&err, Some(&ToolStatus::Succeeded)).is_none());
        assert!(mcp_raw_parts(&both, Some(&ToolStatus::Succeeded)).is_none());
        assert!(mcp_raw_parts(&both, Some(&ToolStatus::Failed)).is_none());
        assert!(
            mcp_raw_parts(
                &json!({"type": "MCP", "output": {"OkayOutput": ""}}),
                Some(&ToolStatus::Succeeded),
            )
            .is_none()
        );
    }
}
