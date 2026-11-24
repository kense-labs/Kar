use super::super::{
    Choice, LoginMethod, LoginProvider, Method, Prompt, ProviderInteraction, ProviderStatus,
};
use crate::providers::ProviderErrorKind as Kind;
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
pub(super) struct Integration {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub methods: Vec<NativeMethod>,
    #[serde(default)]
    pub connections: Vec<Connection>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum NativeMethod {
    Oauth {
        id: String,
        label: Option<String>,
        #[serde(default)]
        form: Vec<Field>,
    },
    Key {
        label: Option<String>,
        #[serde(default)]
        form: Vec<Field>,
    },
    Env {
        names: Vec<String>,
    },
    #[serde(other)]
    Other,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum Connection {
    Credential {
        id: String,
        label: Option<String>,
        method: Option<String>,
    },
    Env {
        name: String,
    },
    #[serde(other)]
    Other,
}
impl Integration {
    pub fn display(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
    pub fn menu(&self) -> Option<LoginProvider> {
        let mut methods = vec![];
        for method in &self.methods {
            if let NativeMethod::Oauth { id, label, .. } = method {
                methods.push(LoginMethod {
                    method: Method::Oauth,
                    name: label.as_ref().unwrap_or(id).clone(),
                    ambient: false,
                    subscription: None,
                    login_label: None,
                });
            }
        }
        for method in &self.methods {
            let (name, ambient) = match method {
                NativeMethod::Key { label, .. } => (
                    label
                        .clone()
                        .unwrap_or_else(|| format!("{} API key", self.display())),
                    false,
                ),
                NativeMethod::Env { names } if !names.is_empty() => (names.join(", "), true),
                _ => continue,
            };
            methods.push(LoginMethod {
                method: Method::ApiKey,
                name,
                ambient,
                subscription: None,
                login_label: None,
            });
        }
        (!methods.is_empty()).then(|| LoginProvider {
            provider_id: self.id.clone(),
            name: self.display().into(),
            methods,
        })
    }
    pub fn credentials(&self) -> impl Iterator<Item = &str> {
        self.connections.iter().filter_map(|c| match c {
            Connection::Credential { id, .. } => Some(id.as_str()),
            _ => None,
        })
    }
    pub fn status(&self) -> ProviderStatus {
        let stored = self
            .connections
            .iter()
            .find(|c| matches!(c, Connection::Credential { .. }));
        let chosen = stored.or_else(|| {
            self.connections
                .iter()
                .find(|c| matches!(c, Connection::Env { .. }))
        });
        let (method, label) = match chosen {
            Some(Connection::Credential { label, method, .. }) => (
                match method.as_deref() {
                    Some("oauth") => Some(Method::Oauth),
                    Some("key") => Some(Method::ApiKey),
                    _ => None,
                },
                label.clone(),
            ),
            Some(Connection::Env { name }) => (Some(Method::ApiKey), Some(name.clone())),
            _ => (None, None),
        };
        ProviderStatus {
            provider_id: self.id.clone(),
            configured: chosen.is_some(),
            method,
            label,
            subscription: None,
        }
    }
    pub fn key(&self) -> Result<(String, &[Field])> {
        self.methods
            .iter()
            .find_map(|m| match m {
                NativeMethod::Key { label, form } => Some((
                    label
                        .clone()
                        .unwrap_or_else(|| format!("{} API key", self.display())),
                    form.as_slice(),
                )),
                _ => None,
            })
            .ok_or_else(|| {
                Kind::MethodUnavailable.error("This provider does not offer API-key login")
            })
    }
}
#[derive(Deserialize)]
pub(super) struct Field {
    key: String,
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
    placeholder: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    custom: bool,
    #[serde(default)]
    options: Vec<FieldOption>,
    default: Option<Value>,
    #[serde(default)]
    when: Vec<Condition>,
}
#[derive(Deserialize)]
struct FieldOption {
    value: String,
    label: Option<String>,
    description: Option<String>,
}
#[derive(Deserialize)]
struct Condition {
    key: String,
    op: Option<String>,
    value: Value,
}
fn defaults(fields: &[Field]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|f| f.default.clone().map(|v| (f.key.clone(), v)))
        .collect()
}
impl Field {
    fn asked(&self, known: &Map<String, Value>) -> bool {
        self.required
            && !self.hidden
            && self.kind != "external"
            && self.when.iter().all(|c| {
                let eq = known.get(&c.key) == Some(&c.value);
                if c.op.as_deref() == Some("neq") {
                    !eq
                } else {
                    eq
                }
            })
    }
    fn prompt(&self) -> Prompt {
        let message = self.title.as_deref().unwrap_or(&self.key).to_owned();
        let options = if self.kind == "boolean" {
            [("true", "Yes"), ("false", "No")]
                .map(|(id, label)| Choice {
                    id: id.into(),
                    label: label.into(),
                    description: None,
                })
                .to_vec()
        } else if !self.custom && self.kind != "multiselect" {
            self.options
                .iter()
                .map(|o| Choice {
                    id: o.value.clone(),
                    label: o.label.as_ref().unwrap_or(&o.value).clone(),
                    description: o.description.clone(),
                })
                .collect()
        } else {
            vec![]
        };
        if options.is_empty() {
            Prompt::Text {
                message,
                placeholder: self
                    .placeholder
                    .clone()
                    .or_else(|| self.default.as_ref()?.as_str().map(str::to_owned)),
            }
        } else {
            Prompt::Select { message, options }
        }
    }
    fn value(&self, answer: &str) -> Result<Option<Value>> {
        let s = answer.trim();
        if s.is_empty() {
            return Ok(self.default.clone());
        }
        let v = match self.kind.as_str() {
            "boolean" => match s {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(Error::Refused("Expected a boolean answer".into())),
            },
            "integer" | "number" => serde_json::from_str::<Value>(s)
                .ok()
                .filter(|v| v.is_number())
                .ok_or_else(|| Error::Refused("Expected a numeric answer".into()))?,
            "multiselect" => Value::Array(
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.into()))
                    .collect(),
            ),
            _ => Value::String(s.into()),
        };
        Ok(Some(v))
    }
}
pub(super) fn requires_input(fields: &[Field]) -> bool {
    let known = defaults(fields);
    fields
        .iter()
        .any(|f| f.asked(&known) && f.default.is_none())
}
pub(super) async fn ask(host: &mut impl ProviderInteraction, prompt: Prompt) -> Result<String> {
    host.prompt(prompt).await.map_err(|_| {
        Kind::InteractionFailed.error("Host could not answer the provider sign-in prompt")
    })
}
pub(super) async fn answers(
    fields: &[Field],
    host: &mut impl ProviderInteraction,
) -> Result<Map<String, Value>> {
    let mut known = defaults(fields);
    let mut answers = Map::new();
    for f in fields {
        if f.asked(&known)
            && let Some(value) = f.value(&ask(host, f.prompt()).await?)?
        {
            known.insert(f.key.clone(), value.clone());
            answers.insert(f.key.clone(), value);
        }
    }
    Ok(answers)
}
