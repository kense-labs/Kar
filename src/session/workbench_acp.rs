//! Native ACP controls that differ from set_mode and set_model on Fx and Kiro V3.
use super::*;
use crate::opening::OpenStep;

impl Session {
    pub(super) async fn configure_workbench_acp(
        &mut self,
        runtime: &str,
        id: &str,
        options: &SessionOptions,
    ) -> Result<()> {
        let cfg = &options.settings;
        if runtime == "kiro" {
            // Both Kar approval modes require the supervised native tool/review path.
            self.workbench_acp_write(id, "autopilot", "off").await?;
            if let Some(agent) = &cfg.acp_mode {
                self.workbench_acp_write(id, "mode", agent).await?;
            }
        } else {
            // Fx 0.0.13 reports ask even while its saved mode is still effective.
            let requested = cfg.acp_mode.as_deref().unwrap_or("ask");
            let value = if matches!(requested, "auto" | "code") {
                if self.acp_offers("mode", "auto") {
                    "auto"
                } else {
                    "code"
                }
            } else {
                requested
            };
            self.workbench_acp_write(id, "mode", value).await?;
        }
        if let Some(model) = options.model.as_deref().filter(|s| *s != "default") {
            // Fx's provider selector changes the available model list.
            if runtime == "fx"
                && let Some((provider, native)) = model.split_once(':')
                && self.acp_offers("provider", provider)
            {
                self.workbench_acp_write(id, "provider", provider).await?;
                self.workbench_acp_write(id, "model", native).await?;
            } else {
                self.workbench_acp_write(id, "model", model).await?;
            }
        }
        let effort = cfg.effort.as_deref().or_else(|| {
            (runtime == "fx" && self.acp_offers("effort", "medium")).then_some("medium")
        });
        if let Some(effort) = effort {
            let key = if runtime == "fx" {
                "effort"
            } else {
                "effortLevel"
            };
            self.workbench_acp_write(id, key, effort).await?;
        }
        Ok(())
    }

    fn acp_offers(&self, key: &str, value: &str) -> bool {
        fn contains(entries: &Value, value: &str) -> bool {
            entries.as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["value"] == value || contains(&row["options"], value))
            })
        }
        self.info["configOptions"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["id"] == key && contains(&row["options"], value))
        })
    }

    async fn workbench_acp_write(&mut self, id: &str, key: &str, value: &str) -> Result<()> {
        let reply = self
            .open_rpc(
                "session/set_config_option",
                json!({"sessionId":id,"configId":key,"value":value}),
                OpenStep::Open,
            )
            .await?;
        if let Some(rows) = reply["configOptions"].as_array() {
            if let Some(row) = rows.iter().find(|r| r["id"] == key)
                && let Some(current) = row["currentValue"].as_str()
                && current != value
            {
                return Err(Error::Refused(format!(
                    "native ACP did not apply {key}={value:?}"
                )));
            }
            self.info["configOptions"] = Value::Array(rows.clone());
        }
        Ok(())
    }
}
