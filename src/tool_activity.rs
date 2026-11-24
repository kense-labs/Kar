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
