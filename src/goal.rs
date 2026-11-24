use serde::{Deserialize, Deserializer, Serialize};

/// Distinguishes an absent field (unobserved) from a native JSON null (observed absence).
pub(crate) fn observed<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Provider-reported goal. Status retains new native values verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalSnapshot {
    pub objective: String,
    pub status: String,
    #[serde(
        default,
        deserialize_with = "observed",
        skip_serializing_if = "Option::is_none"
    )]
    #[serde(alias = "tokenBudget")]
    pub token_budget: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "tokensUsed")]
    pub tokens_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "timeUsedSeconds")]
    pub time_used_seconds: Option<u64>,
    /// Unix epoch seconds as reported by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "createdAt")]
    pub created_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "updatedAt")]
    pub updated_at: Option<f64>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoalSetOptions {
    pub objective: String,
    pub status: Option<String>,
    pub token_budget: Option<u64>,
}

#[cfg(not(target_arch = "wasm32"))]
mod codex {
    use super::*;
    use crate::{Error, Result, Session};
    use serde_json::{Value, json};

    /// Missing or malformed observations never clear the last confirmed goal.
    pub(crate) fn observation(
        value: &Value,
        method: &str,
        thread: Option<&str>,
    ) -> Result<Option<GoalSnapshot>> {
        if method == "thread/goal/cleared" {
            return Ok(None);
        }
        if method == "thread/goal/clear" {
            return value["cleared"]
                .as_bool()
                .map(|_| None)
                .ok_or_else(|| Error::Refused("Codex did not confirm clearing the goal".into()));
        }
        let invalid = || Error::Refused(format!("Codex returned an invalid {method} observation"));
        let v = value.get("goal").ok_or_else(invalid)?;
        if v.is_null() && matches!(method, "thread/goal/get" | "thread/goal/updated") {
            return Ok(None);
        }
        if let Some(reported) = v.get("threadId")
            && thread.is_some_and(|expected| reported.as_str() != Some(expected))
        {
            return Err(invalid());
        }
        Ok(Some(GoalSnapshot {
            objective: v["objective"].as_str().ok_or_else(invalid)?.into(),
            status: v["status"].as_str().ok_or_else(invalid)?.into(),
            token_budget: v.get("tokenBudget").and_then(|b| {
                if b.is_null() {
                    Some(None)
                } else {
                    b.as_u64().map(Some)
                }
            }),
            tokens_used: v["tokensUsed"].as_u64(),
            time_used_seconds: v["timeUsedSeconds"].as_u64(),
            created_at: v["createdAt"].as_f64(),
            updated_at: v["updatedAt"].as_f64(),
        }))
    }
    impl Session {
        fn require_goals(&self) -> Result<()> {
            if self.capabilities().goals {
                Ok(())
            } else {
                Err(Error::Unsupported(
                    "open a goal-capable session with goals enabled".into(),
                ))
            }
        }
        pub async fn read_goal(&self) -> Result<Option<GoalSnapshot>> {
            self.require_goals()?;
            if self.protocol() == crate::Protocol::Dsh {
                return Ok(serde_json::from_value(
                    self.rpc("goals/get", json!({})).await?,
                )?);
            }
            let method = "thread/goal/get";
            observation(
                &self.rpc(method, json!({"threadId":self.id()})).await?,
                method,
                Some(self.id()),
            )
        }
        pub async fn set_goal(&self, options: GoalSetOptions) -> Result<GoalSnapshot> {
            self.require_goals()?;
            if self.protocol() == crate::Protocol::Dsh {
                return Ok(serde_json::from_value(
                    self.rpc("goals/set", serde_json::to_value(options)?)
                        .await?,
                )?);
            }
            let mut params =
                json!({"threadId":self.id(),"origin":"user","objective":options.objective});
            if let Some(status) = options.status {
                if !matches!(
                    status.as_str(),
                    "active" | "paused" | "blocked" | "usageLimited" | "budgetLimited" | "complete"
                ) {
                    return Err(Error::Unsupported(format!(
                        "unsupported native goal status {status}"
                    )));
                }
                params["status"] = status.into();
            }
            if let Some(budget) = options.token_budget {
                if !(1..=9_007_199_254_740_991).contains(&budget) {
                    return Err(Error::Refused(
                        "token budget must be a positive safe integer".into(),
                    ));
                }
                params["tokenBudget"] = budget.into();
            }
            let method = "thread/goal/set";
            observation(&self.rpc(method, params).await?, method, Some(self.id()))?
                .ok_or_else(|| Error::Refused("Codex did not return a goal".into()))
        }
        pub async fn clear_goal(&self) -> Result<()> {
            self.require_goals()?;
            if self.protocol() == crate::Protocol::Dsh {
                return self.rpc("goals/clear", json!({})).await.map(drop);
            }
            let reply = self
                .rpc(
                    "thread/goal/clear",
                    json!({"threadId":self.id(),"origin":"user"}),
                )
                .await?;
            observation(&reply, "thread/goal/clear", Some(self.id()))?;
            Ok(())
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use codex::observation;
