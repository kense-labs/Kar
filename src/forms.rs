//! Serializable runtime settings and their validation at the host and launch boundaries.
#[cfg(test)]
mod tests;
mod validation;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
pub use validation::{ConfigIssue, IssueCode, LaunchMapping, ValidationContext};

pub type FormValues = Map<String, Value>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyText {
    pub key: String,
    pub en: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visibility {
    pub field: String,
    #[serde(rename = "in")]
    pub values: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: CopyText,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelMode {
    Inherit,
    Preset,
    Custom,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ModelValue {
    Inherit,
    Preset { id: String },
    Custom { name: String },
}
impl ModelValue {
    pub fn mode(&self) -> ModelMode {
        match self {
            Self::Inherit => ModelMode::Inherit,
            Self::Preset { .. } => ModelMode::Preset,
            Self::Custom { .. } => ModelMode::Custom,
        }
    }
    pub fn selector(&self) -> Option<&str> {
        match self {
            Self::Inherit => None,
            Self::Preset { id } => Some(id),
            Self::Custom { name } => Some(name),
        }
    }
}
/// Submitted secret edits. The host resolves them against its credential store before launch.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SecretEdit {
    Unset,
    Keep,
    Clear,
    Set { value: String },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextFormat {
    Url,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum ConfigFieldKind {
    Model {
        modes: Vec<ModelMode>,
        #[serde(skip_serializing_if = "Option::is_none")]
        launch_modes: Option<Vec<ModelMode>>,
    },
    Effort {
        model_field: String,
    },
    Select {
        options: Vec<SelectOption>,
    },
    Text {
        #[serde(skip_serializing_if = "Option::is_none")]
        format: Option<TextFormat>,
    },
    Secret,
    Boolean,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigField {
    pub id: String,
    pub label: CopyText,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<CopyText>,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<Visibility>,
    #[serde(flatten)]
    pub kind: ConfigFieldKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum FormProtocol {
    V1,
}
impl TryFrom<u8> for FormProtocol {
    type Error = &'static str;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        if v == 1 {
            Ok(Self::V1)
        } else {
            Err("unsupported runtime form protocol")
        }
    }
}
impl From<FormProtocol> for u8 {
    fn from(_: FormProtocol) -> Self {
        1
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfigForm {
    pub protocol: FormProtocol,
    pub runtime: String,
    pub fields: Vec<ConfigField>,
}

/// Return the available configuration forms without starting a runtime or reading credentials.
pub fn config_form(runtime: &str) -> Option<RuntimeConfigForm> {
    config_forms().get(runtime).cloned()
}

pub fn config_forms() -> &'static std::collections::BTreeMap<&'static str, RuntimeConfigForm> {
    use std::{collections::BTreeMap, sync::LazyLock};
    static FORMS: LazyLock<BTreeMap<&str, RuntimeConfigForm>> =
        LazyLock::new(|| BTreeMap::from([("grok", grok_form())]));
    &FORMS
}

fn grok_form() -> RuntimeConfigForm {
    let field = |id: &str, key: &str, label: &str, hint: &str, kind| ConfigField {
        id: id.into(),
        required: false,
        visible_when: None,
        kind,
        label: CopyText {
            key: format!("runtime_form.grok.{key}.label"),
            en: label.into(),
        },
        hint: Some(CopyText {
            key: format!("runtime_form.grok.{key}.hint"),
            en: hint.into(),
        }),
    };
    RuntimeConfigForm {
        protocol: FormProtocol::V1,
        runtime: "grok".into(),
        fields: vec![
            field(
                "model",
                "model",
                "Model",
                "Models available from this computer's Grok CLI. A typed model id the CLI does not know fails when the agent starts.",
                ConfigFieldKind::Model {
                    modes: vec![ModelMode::Inherit, ModelMode::Preset, ModelMode::Custom],
                    launch_modes: None,
                },
            ),
            field(
                "reasoningEffort",
                "reasoning_effort",
                "Reasoning",
                "Available values depend on the selected model.",
                ConfigFieldKind::Effort {
                    model_field: "model".into(),
                },
            ),
        ],
    }
}

/// A model as offered to the form. Extra provider, display, and service-tier metadata is preserved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort_levels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<Value>,
    #[serde(flatten)]
    pub metadata: Map<String, Value>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingReason {
    Unauthenticated,
    Timeout,
    Failed,
    Empty,
    MachineOffline,
    Unsupported,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListingSource {
    Bundled,
    Configured,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ModelListing {
    Live {
        models: Vec<ModelEntry>,
    },
    Fallback {
        source: ListingSource,
        reason: ListingReason,
        models: Vec<ModelEntry>,
    },
    Unavailable {
        reason: ListingReason,
    },
}
impl ModelListing {
    pub fn from_result(
        live: Result<Vec<ModelEntry>, ListingReason>,
        bundled: Vec<ModelEntry>,
    ) -> Self {
        let reason = match live {
            Ok(models) if !models.is_empty() => return Self::Live { models },
            Ok(_) => ListingReason::Empty,
            Err(reason) => reason,
        };
        if bundled.is_empty() {
            Self::Unavailable { reason }
        } else {
            Self::Fallback {
                source: ListingSource::Bundled,
                reason,
                models: bundled,
            }
        }
    }
    pub fn models(&self) -> &[ModelEntry] {
        match self {
            Self::Live { models } | Self::Fallback { models, .. } => models,
            Self::Unavailable { .. } => &[],
        }
    }
}
