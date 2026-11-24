use serde::{Deserialize, Serialize};
#[cfg(not(target_arch = "wasm32"))]
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    #[serde(alias = "contextWindow")]
    pub context_window: Option<u64>,
    pub percent: Option<f64>,
}
impl ContextUsage {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn counts(tokens: Option<u64>, window: Option<u64>, round: bool) -> Self {
        let percent = tokens
            .zip(window.filter(|n| *n > 0))
            .map(|(n, w)| 100.0 * n as f64 / w as f64)
            .map(|n| if round { n.round() } else { n });
        Self {
            tokens,
            context_window: window,
            percent,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Used,
    Reserved,
    Deferred,
    Free,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub name: String,
    pub tokens: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCategory {
    pub name: String,
    pub tokens: u64,
    pub kind: ContextKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ContextItem>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub tokens: u64,
    pub context_window: u64,
    pub categories: Vec<ContextCategory>,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn breakdown(body: &Value) -> crate::Result<ContextBreakdown> {
    let count = |k: &str| {
        body[k].as_u64().ok_or_else(|| {
            crate::Error::Refused(format!("Claude context breakdown did not report {k}"))
        })
    };
    let categories = body["categories"]
        .as_array()
        .ok_or_else(|| crate::Error::Refused("Claude did not report context categories".into()))?;
    let both_mcp = ["MCP tools", "MCP tools (deferred)"]
        .iter()
        .all(|&name| categories.iter().any(|v| v["name"] == name));
    let categories = categories
        .iter()
        .filter_map(|c| {
            let name = c["name"].as_str()?;
            let tokens = c["tokens"].as_u64()?;
            let kind = match c["kind"].as_str() {
                Some("used") => ContextKind::Used,
                Some("buffer") => ContextKind::Reserved,
                Some("deferred") => ContextKind::Deferred,
                Some("free") => ContextKind::Free,
                _ => ContextKind::Unknown,
            };
            let (array, field) = match name {
                "Memory files" => (&body["memoryFiles"], "path"),
                "Skills" => (&body["skills"]["skillFrontmatter"], "name"),
                "Custom agents" => (&body["agents"], "agentType"),
                "MCP tools" | "MCP tools (deferred)" => (&body["mcpTools"], "name"),
                _ => (&Value::Null, ""),
            };
            let items = array
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| {
                    !both_mcp
                        || !name.starts_with("MCP tools")
                        || ((v["isLoaded"] == true) == (name == "MCP tools"))
                })
                .filter_map(|v| {
                    Some(ContextItem {
                        name: v[field].as_str()?.into(),
                        tokens: v["tokens"].as_u64()?,
                    })
                })
                .collect();
            Some(ContextCategory {
                name: name.into(),
                tokens,
                kind,
                items,
            })
        })
        .collect();
    Ok(ContextBreakdown {
        tokens: count("totalTokens")?,
        context_window: count("maxTokens")?,
        categories,
    })
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(crate) struct ClaudeContext {
    tokens: Option<Option<u64>>,
    model: Option<String>,
}
#[cfg(not(target_arch = "wasm32"))]
impl ClaudeContext {
    pub fn observe(&mut self, v: &Value) -> Option<ContextUsage> {
        match v["type"].as_str() {
            Some("assistant")
                if v["message"]["model"] != "<synthetic>" && v["message"]["usage"].is_object() =>
            {
                let msg = &v["message"];
                let u = &msg["usage"];
                if let Some(model) = msg["model"].as_str() {
                    self.model = Some(model.into());
                }
                self.tokens = Some(
                    [
                        "input_tokens",
                        "cache_creation_input_tokens",
                        "cache_read_input_tokens",
                    ]
                    .iter()
                    .try_fold(0_u64, |n, k| n.checked_add(u[*k].as_u64().unwrap_or(0))),
                );
                None
            }
            Some("system") if v["subtype"] == "compact_boundary" => {
                self.tokens = Some(None);
                None
            }
            Some("result") => {
                let tokens = self.tokens.take()?;
                let models = v["modelUsage"].as_object();
                let window = models
                    .and_then(|ms| {
                        self.model
                            .as_ref()
                            .and_then(|m| ms.get(m))
                            .or_else(|| ms.values().next())
                    })
                    .and_then(|m| m["contextWindow"].as_u64());
                Some(ContextUsage::counts(tokens, window, true))
            }
            _ => None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn grok(response: &Value) -> Option<ContextUsage> {
    let meta = response.get("_meta")?;
    let first = |keys: &[&str]| keys.iter().find_map(|key| meta[key].as_u64());
    let tokens = first(&["totalTokens", "contextTokens"]);
    let window = first(&["contextWindow", "context_window", "maxContextTokens"]);
    (tokens.is_some() || window.is_some()).then(|| ContextUsage::counts(tokens, window, true))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn qwen(v: &Value) -> Option<ContextUsage> {
    if v["subtype"] != "get_context_usage" {
        return None;
    }
    let tokens = (v["isEstimated"] == false)
        .then(|| v["totalTokens"].as_u64())
        .flatten();
    let window = v["contextWindowSize"].as_u64().filter(|n| *n > 0);
    (tokens.is_some() || window.is_some()).then(|| ContextUsage::counts(tokens, window, false))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_uses_last_model_input_and_recognizes_unknown_occupancy() {
        let mut c = ClaudeContext::default();
        for tokens in [100, 7] {
            c.observe(&json!({"type":"assistant","message":{"model":"primary","usage":{"input_tokens":tokens,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":99}}}));
        }
        let result = json!({"type":"result","modelUsage":{"aux":{"contextWindow":13},"primary":{"contextWindow":100}}});
        assert_eq!(
            c.observe(&result).unwrap(),
            ContextUsage::counts(Some(12), Some(100), true)
        );
        assert!(c.observe(&result).is_none());
        c.observe(&json!({"type":"system","subtype":"compact_boundary"}));
        assert_eq!(c.observe(&result).unwrap().tokens, None);
        let estimated=qwen(&json!({"subtype":"get_context_usage","totalTokens":800,"contextWindowSize":1600,"isEstimated":true})).unwrap();
        assert_eq!(
            (
                estimated.tokens,
                estimated.context_window,
                estimated.percent
            ),
            (None, Some(1600), None)
        );
    }
    #[test]
    fn category_items_preserve_order_and_native_mcp_grouping() {
        let mut body = json!({"totalTokens":6,"maxTokens":12,"categories":[{"name":"MCP tools","tokens":3,"kind":"used"},{"name":"MCP tools (deferred)","tokens":2,"kind":"deferred"},{"name":"future category","tokens":5,"kind":"future"}],"mcpTools":[{"name":"a","tokens":3,"isLoaded":true},{"name":"b","tokens":2,"isLoaded":false}]});
        let report = breakdown(&body).unwrap();
        assert_eq!(report.categories[0].items[0].name, "a");
        assert_eq!(report.categories[1].items[0].name, "b");
        assert_eq!(report.categories[2].kind, ContextKind::Unknown);
        body["categories"].as_array_mut().unwrap().remove(1);
        assert_eq!(breakdown(&body).unwrap().categories[0].items.len(), 2);
    }
}
