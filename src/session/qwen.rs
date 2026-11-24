use super::Session;
use crate::{Error, Result};
use serde_json::{Value, json};

pub(super) fn initialize_error(error: Error, effort: Option<&str>) -> Error {
    if let (Error::Refused(wire), Some(level)) = (&error, effort)
        && let Ok(response) = serde_json::from_str::<Value>(wire)
        && let Some(message) = response["error"].as_str()
        && message.starts_with("Invalid effort value")
    {
        return Error::Refused(format!("Qwen refused effort {level:?}: {message}"));
    }
    error
}

pub(super) fn project_models(models: &mut [Value]) {
    for model in models {
        if let Some(label) = model["label"].as_str() {
            model["displayName"] = label.to_owned().into();
        }
        let reasoning = &model["capabilities"]["reasoning"];
        let default = reasoning["defaultEffort"].as_str().map(str::to_owned);
        if let Some(values) = reasoning["efforts"].as_array() {
            model["effortLevels"] = json!(
                values
                    .iter()
                    .filter_map(|v| {
                        v.as_str()
                            .or_else(|| v["effort"].as_str())
                            .or_else(|| v["id"].as_str())
                    })
                    .collect::<Vec<_>>()
            );
        }
        if let Some(default) = default {
            model["defaultEffort"] = default.into();
        }
    }
}

impl Session {
    pub(super) async fn confirm_qwen_effort(
        &self,
        level: &str,
        requested_model: Option<&str>,
    ) -> Result<()> {
        let applied = &self.info["effort_status"];
        if applied["effort"] != level
            || applied["applied"] != true
            || applied.get("override") != Some(&Value::Null)
        {
            return Err(Error::Refused(format!(
                "Qwen did not apply requested effort {level:?}"
            )));
        }
        // initialize confirms the SDK setting even for models with no effort
        // support. Check the native model menu before admitting a prompt.
        let models = self.models().await?;
        let rows = models.as_array().expect("Qwen models checked the array");
        let current;
        let model = if let Some(requested) = requested_model {
            Some(requested)
        } else if self.info.pointer("/capabilities/can_get_context_usage")
            == Some(&Value::Bool(true))
        {
            current = self.context_usage().await?;
            current["modelName"].as_str()
        } else {
            None
        };
        let offers = |m: &Value| {
            m["effortLevels"]
                .as_array()
                .is_some_and(|levels| levels.iter().any(|l| l == level))
        };
        let supported = match model {
            Some(id) => rows.iter().any(|m| m["id"] == id && offers(m)),
            None => !rows.is_empty() && rows.iter().all(offers),
        };
        if !supported {
            return Err(Error::Refused(format!(
                "Qwen model {} does not advertise requested effort {level:?}",
                model.unwrap_or("(default)")
            )));
        }
        Ok(())
    }
}
