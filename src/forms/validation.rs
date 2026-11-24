use super::*;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    Required,
    InvalidShape,
    ConflictingFields,
    ModeNotAllowed,
    ModelNotListed,
    ModelDisabled,
    EffortNotOffered,
    OptionNotOffered,
    SecretMissing,
    UnknownField,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIssue {
    pub field: String,
    pub code: IssueCode,
}
pub enum ValidationContext<'a> {
    Create {
        models: &'a ModelListing,
    },
    Edit {
        models: &'a ModelListing,
        stored: &'a FormValues,
    },
}

/// Secrets are returned only to the caller that resolves and launches the configuration.
/// This type deliberately has no serializing or debug representation.
#[derive(Default)]
pub struct LaunchMapping {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub unmapped: FormValues,
    pub secrets: BTreeMap<String, String>,
}
impl LaunchMapping {
    /// Apply the shared launch fields once the runtime-specific fields have been handled.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn apply_to(&self, options: &mut crate::SessionOptions) -> crate::Result<()> {
        if !self.unmapped.is_empty() || !self.secrets.is_empty() {
            return Err(crate::Error::Unsupported(
                "form contains runtime-specific launch fields".into(),
            ));
        }
        options.model.clone_from(&self.model);
        options.settings.effort.clone_from(&self.effort);
        Ok(())
    }
}

fn model(v: &Value) -> Option<ModelValue> {
    let model: ModelValue = serde_json::from_value(v.clone()).ok()?;
    model
        .selector()
        .is_none_or(|id| !id.trim().is_empty())
        .then_some(model)
}
fn empty(v: &Value) -> bool {
    v.is_null() || v.as_str() == Some("")
}
fn same_model(a: &Value, b: &Value) -> bool {
    model(a).is_some_and(|m| Some(m) == model(b)) || (empty(a) && empty(b))
}
fn scalar(field: &ConfigFieldKind, value: &Value) -> Result<(), IssueCode> {
    match field {
        ConfigFieldKind::Boolean if value.is_boolean() => Ok(()),
        ConfigFieldKind::Text { format } => {
            let text = value.as_str().ok_or(IssueCode::InvalidShape)?;
            if format.is_some() && url::Url::parse(text).is_err() {
                Err(IssueCode::InvalidShape)
            } else {
                Ok(())
            }
        }
        ConfigFieldKind::Select { options } => {
            let text = value.as_str().ok_or(IssueCode::InvalidShape)?;
            if options.iter().any(|o| o.value == text) {
                Ok(())
            } else {
                Err(IssueCode::OptionNotOffered)
            }
        }
        _ => Err(IssueCode::InvalidShape),
    }
}

impl RuntimeConfigForm {
    pub fn active_fields<'a>(
        &'a self,
        values: &FormValues,
    ) -> impl Iterator<Item = &'a ConfigField> {
        self.fields.iter().filter(move |field| {
            let mut seen = HashSet::new();
            let mut parent = field.visible_when.as_ref();
            while let Some(rule) = parent {
                let Some(controller) = self.fields.iter().find(|c| c.id == rule.field) else {
                    return false;
                };
                if !seen.insert(&controller.id)
                    || !value(values, &rule.field)
                        .as_str()
                        .is_some_and(|v| rule.values.iter().any(|wanted| wanted == v))
                {
                    return false;
                }
                parent = controller.visible_when.as_ref();
            }
            true
        })
    }
    fn inputs<'a>(&'a self, values: &FormValues) -> (Vec<&'a ConfigField>, Vec<ConfigIssue>) {
        let mut issues: Vec<_> = values
            .keys()
            .filter(|id| !self.fields.iter().any(|f| &f.id == *id))
            .map(|id| ConfigIssue {
                field: id.clone(),
                code: IssueCode::UnknownField,
            })
            .collect();
        let fields: Vec<_> = self.active_fields(values).collect();
        let mut kinds = HashSet::new();
        for field in &fields {
            let key = match field.kind {
                ConfigFieldKind::Model { .. } => "model",
                ConfigFieldKind::Effort { .. } => "effort",
                _ => continue,
            };
            if !empty(value(values, &field.id)) && !kinds.insert(key) {
                issues.push(ConfigIssue {
                    field: field.id.clone(),
                    code: IssueCode::ConflictingFields,
                });
            }
        }
        (fields, issues)
    }
    /// Validate all active fields, including the preservation of an unchanged model/effort on edit.
    pub fn validate(
        &self,
        values: &FormValues,
        context: ValidationContext<'_>,
    ) -> Vec<ConfigIssue> {
        let (models, stored) = match context {
            ValidationContext::Create { models } => (models, None),
            ValidationContext::Edit { models, stored } => (models, Some(stored)),
        };
        let (fields, mut issues) = self.inputs(values);
        for f in fields {
            if let Err(code) = check(f, values, models.models(), stored) {
                issues.push(ConfigIssue {
                    field: f.id.clone(),
                    code,
                });
            }
        }
        issues
    }
    /// Map resolved launch values. A write-only field takes `{"present":true,"value":"..."}`.
    pub fn map_launch(&self, values: &FormValues) -> Result<LaunchMapping, Vec<ConfigIssue>> {
        let (fields, mut issues) = self.inputs(values);
        let mut out = LaunchMapping::default();
        for f in fields {
            if let Err(code) = map(f, value(values, &f.id), &mut out) {
                issues.push(ConfigIssue {
                    field: f.id.clone(),
                    code,
                });
            }
        }
        if issues.is_empty() {
            Ok(out)
        } else {
            Err(issues)
        }
    }
}

fn value<'a>(values: &'a FormValues, id: &str) -> &'a Value {
    values.get(id).unwrap_or(&Value::Null)
}
fn reject_if(invalid: bool, code: IssueCode) -> Result<(), IssueCode> {
    if invalid { Err(code) } else { Ok(()) }
}
fn check(
    f: &ConfigField,
    values: &FormValues,
    models: &[ModelEntry],
    stored: Option<&FormValues>,
) -> Result<(), IssueCode> {
    let v = value(values, &f.id);
    if matches!(f.kind, ConfigFieldKind::Secret) {
        if v.is_null() {
            return reject_if(f.required && stored.is_none(), IssueCode::Required);
        }
        return match serde_json::from_value::<SecretEdit>(v.clone()) {
            Ok(SecretEdit::Set { value }) if !value.is_empty() => Ok(()),
            Ok(SecretEdit::Keep) if stored.is_some() => Ok(()),
            Ok(SecretEdit::Keep | SecretEdit::Unset | SecretEdit::Clear) => {
                reject_if(f.required, IssueCode::Required)
            }
            _ => Err(IssueCode::InvalidShape),
        };
    }
    if empty(v) {
        return reject_if(f.required, IssueCode::Required);
    }
    match &f.kind {
        ConfigFieldKind::Model {
            modes,
            launch_modes,
        } => {
            let chosen = model(v).ok_or(IssueCode::InvalidShape)?;
            let kept = stored.is_some_and(|old| same_model(v, value(old, &f.id)));
            if !modes.contains(&chosen.mode()) {
                return reject_if(
                    !(kept
                        && launch_modes
                            .as_ref()
                            .unwrap_or(modes)
                            .contains(&chosen.mode())),
                    IssueCode::ModeNotAllowed,
                );
            }
            if chosen.mode() == ModelMode::Preset && !kept {
                let entry = models
                    .iter()
                    .find(|m| Some(m.id.as_str()) == chosen.selector())
                    .ok_or(IssueCode::ModelNotListed)?;
                if entry.disabled.is_some() {
                    return Err(IssueCode::ModelDisabled);
                }
            }
            Ok(())
        }
        ConfigFieldKind::Effort { model_field } => {
            let effort = v.as_str().ok_or(IssueCode::InvalidShape)?;
            let selected = value(values, model_field);
            if stored.is_some_and(|old| {
                value(old, &f.id) == v && same_model(selected, value(old, model_field))
            }) {
                return Ok(());
            }
            let entry = match model(selected) {
                Some(ModelValue::Preset { id } | ModelValue::Custom { name: id }) => {
                    models.iter().find(|m| m.id == id)
                }
                Some(ModelValue::Inherit) => models.iter().find(|m| m.default),
                _ if empty(selected) => models.iter().find(|m| m.default),
                _ => None,
            };
            let offered = entry
                .and_then(|m| m.effort_levels.as_ref())
                .is_some_and(|levels| levels.iter().any(|s| s == effort));
            reject_if(!offered, IssueCode::EffortNotOffered)
        }
        k => scalar(k, v),
    }
}
fn map(f: &ConfigField, v: &Value, out: &mut LaunchMapping) -> Result<(), IssueCode> {
    if matches!(f.kind, ConfigFieldKind::Secret) {
        if v.is_null() || v["present"] == false {
            return reject_if(f.required, IssueCode::SecretMissing);
        }
        if v["present"] != true {
            return Err(IssueCode::InvalidShape);
        }
        let secret = v["value"].as_str().ok_or(IssueCode::InvalidShape)?;
        if secret.is_empty() {
            return reject_if(f.required, IssueCode::SecretMissing);
        }
        out.secrets.insert(f.id.clone(), secret.into());
        return Ok(());
    }
    if empty(v) {
        return reject_if(f.required, IssueCode::Required);
    }
    match &f.kind {
        ConfigFieldKind::Model {
            modes,
            launch_modes,
        } => {
            let chosen = model(v).ok_or(IssueCode::InvalidShape)?;
            if !launch_modes
                .as_ref()
                .unwrap_or(modes)
                .contains(&chosen.mode())
            {
                return Err(IssueCode::ModeNotAllowed);
            }
            out.model = chosen.selector().map(str::to_owned);
        }
        ConfigFieldKind::Effort { .. } => {
            out.effort = Some(v.as_str().ok_or(IssueCode::InvalidShape)?.into())
        }
        k => {
            scalar(k, v)?;
            out.unmapped.insert(f.id.clone(), v.clone());
        }
    }
    Ok(())
}
