use crate::PendingRequest;
#[cfg(not(target_arch = "wasm32"))]
use crate::PermissionDecision;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(not(target_arch = "wasm32"))]
use serde_json::json;

/// Identifies an option by its source and position in this particular native request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", content = "index", rename_all = "snake_case")]
pub enum PolicyChoice {
    Acp(usize),
    Factory(usize),
    Advertised(usize),
    CommandPrefix,
    NetworkRule(usize),
    ClaudeSuggestion(usize),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyOption {
    pub choice: PolicyChoice,
    pub details: Value,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "index", rename_all = "snake_case")]
pub enum ResourceId {
    Read(usize),
    Write(usize),
    Entry(usize),
    Network,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionResource {
    pub id: ResourceId,
    pub value: Value,
}

impl PendingRequest {
    pub fn policy_options(&self) -> Vec<PolicyOption> {
        policies(&self.method, &self.body)
    }
    pub fn permission_resources(&self) -> Vec<PermissionResource> {
        resources(&self.method, &self.body)
    }
}
fn exec_rule(v: &Value) -> bool {
    v.as_array()
        .is_some_and(|words| !words.is_empty() && words.iter().all(Value::is_string))
}
fn net_rule(v: &Value) -> bool {
    v["host"].as_str().is_some_and(|s| !s.is_empty())
        && matches!(v["action"].as_str(), Some("allow" | "deny"))
}
fn policies(method: &str, b: &Value) -> Vec<PolicyOption> {
    let mut out = vec![];
    let mut add = |choice, details: &Value| {
        out.push(PolicyOption {
            choice,
            details: details.clone(),
        })
    };
    match method {
        "droid.request_permission" => {
            let mut values = std::collections::HashSet::new();
            for (index, option) in b["options"].as_array().into_iter().flatten().enumerate() {
                let Some(value) = option["value"].as_str() else {
                    continue;
                };
                if !values.insert(value) {
                    return vec![];
                }
                // Editing a proposed spec also needs its replacement content.
                if value != "proceed_edit" && !value.is_empty() {
                    add(PolicyChoice::Factory(index), option);
                }
            }
        }
        "session/request_permission" => {
            if let Some(options) = b["options"].as_array() {
                let mut ids = std::collections::HashSet::new();
                for (i, v) in options.iter().enumerate() {
                    if let Some(id) = v["optionId"].as_str() {
                        if !ids.insert(id) {
                            return vec![];
                        }
                        add(PolicyChoice::Acp(i), v);
                    }
                }
            }
        }
        "item/commandExecution/requestApproval" => {
            if !b["availableDecisions"].is_null() {
                for (i, v) in b["availableDecisions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if v.pointer("/acceptWithExecpolicyAmendment/execpolicy_amendment")
                        .is_some_and(exec_rule)
                        || v.pointer("/applyNetworkPolicyAmendment/network_policy_amendment")
                            .is_some_and(net_rule)
                    {
                        add(PolicyChoice::Advertised(i), v);
                    }
                }
            } else {
                if exec_rule(&b["proposedExecpolicyAmendment"]) {
                    add(
                        PolicyChoice::CommandPrefix,
                        &b["proposedExecpolicyAmendment"],
                    );
                }
                for (i, v) in b["proposedNetworkPolicyAmendments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if net_rule(v) {
                        add(PolicyChoice::NetworkRule(i), v);
                    }
                }
            }
        }
        "can_use_tool" if b["tool_name"] != "AskUserQuestion" && b["input"].is_object() => {
            for (i, v) in b["permission_suggestions"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if v["type"].is_string() {
                    add(PolicyChoice::ClaudeSuggestion(i), v);
                }
            }
        }
        _ => {}
    }
    out
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn select(method: &str, body: &Value, choice: PolicyChoice) -> Option<Value> {
    let option = policies(method, body)
        .into_iter()
        .find(|v| v.choice == choice)?
        .details;
    Some(match choice {
        PolicyChoice::Factory(_) => json!({"selectedOption":option["value"]}),
        PolicyChoice::Acp(_) => {
            json!({"outcome":{"outcome":"selected","optionId":option["optionId"]}})
        }
        PolicyChoice::Advertised(_) => json!({"decision":option}),
        PolicyChoice::CommandPrefix => {
            json!({"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":option}}})
        }
        PolicyChoice::NetworkRule(_) => {
            json!({"decision":{"applyNetworkPolicyAmendment":{"network_policy_amendment":option}}})
        }
        PolicyChoice::ClaudeSuggestion(_) => {
            json!({"behavior":"allow","updatedInput":body["input"],"updatedPermissions":[option]})
        }
    })
}
fn resources(method: &str, b: &Value) -> Vec<PermissionResource> {
    if method != "item/permissions/requestApproval" {
        return vec![];
    }
    let Some(profile) = b["permissions"].as_object() else {
        return vec![];
    };
    let fs = &b["permissions"]["fileSystem"];
    if profile
        .keys()
        .any(|k| !matches!(k.as_str(), "fileSystem" | "network"))
        || fs.as_object().is_some_and(|m| {
            m.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "entries" | "read" | "write" | "globScanMaxDepth"
                )
            })
        })
    {
        return vec![];
    }
    let mut rows = vec![];
    for key in ["entries", "read", "write"] {
        if fs[key].is_null() {
            continue;
        }
        let Some(values) = fs[key].as_array() else {
            return vec![];
        };
        for (i, v) in values.iter().enumerate() {
            let id = match key {
                "read" if v.is_string() => ResourceId::Read(i),
                "write" if v.is_string() => ResourceId::Write(i),
                "entries" if v["access"] == "deny" => continue,
                "entries"
                    if matches!(v["access"].as_str(), Some("read" | "write"))
                        && v["path"].is_object() =>
                {
                    ResourceId::Entry(i)
                }
                _ => return vec![],
            };
            rows.push(PermissionResource {
                id,
                value: v.clone(),
            });
        }
    }
    let network = &b["permissions"]["network"];
    if network["enabled"] == true && network.as_object().is_some_and(|n| n.len() == 1) {
        rows.push(PermissionResource {
            id: ResourceId::Network,
            value: network.clone(),
        });
    }
    rows
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn grant(
    method: &str,
    b: &Value,
    scope: PermissionDecision,
    ids: &[ResourceId],
) -> Option<Value> {
    let scope = match scope {
        PermissionDecision::AllowForTurn => "turn",
        PermissionDecision::AllowForSession => "session",
        _ => return None,
    };
    let wanted: std::collections::HashSet<_> = ids.iter().copied().collect();
    let choices = resources(method, b);
    if wanted.is_empty()
        || wanted.len() != ids.len()
        || !wanted.iter().all(|id| choices.iter().any(|c| &c.id == id))
    {
        return None;
    }
    let mut profile = json!({});
    let source = &b["permissions"]["fileSystem"];
    if wanted.iter().any(|id| *id != ResourceId::Network) {
        let mut fs = json!({});
        for key in ["entries", "read", "write"] {
            let entries: Vec<_> = source[key]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(i, v)| match key {
                    "entries" => v["access"] == "deny" || wanted.contains(&ResourceId::Entry(*i)),
                    "read" => wanted.contains(&ResourceId::Read(*i)),
                    _ => wanted.contains(&ResourceId::Write(*i)),
                })
                .map(|(_, v)| v.clone())
                .collect();
            if !entries.is_empty() {
                fs[key] = entries.into();
            }
        }
        if let Some(limit) = source.get("globScanMaxDepth") {
            fs["globScanMaxDepth"] = limit.clone();
        }
        profile["fileSystem"] = fs;
    }
    if wanted.contains(&ResourceId::Network) {
        profile["network"] = json!({"enabled":true});
    }
    Some(json!({"permissions":profile,"scope":scope}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subset_keeps_native_denials_and_only_the_selected_access() {
        let entry = json!({"path":{"type":"path","path":"/project"},"access":"write"});
        let deny = json!({"path":{"type":"path","path":"/project/private"},"access":"deny"});
        let method = "item/permissions/requestApproval";
        let body = json!({"permissions":{"network":{"enabled":true},"fileSystem":{"entries":[entry,deny],"read":["one","two"],"globScanMaxDepth":4}}});
        let ids = [ResourceId::Entry(0), ResourceId::Read(1)];
        assert_eq!(
            grant(method, &body, PermissionDecision::AllowForTurn, &ids),
            Some(
                json!({"scope":"turn","permissions":{"fileSystem":{"entries":[entry,deny],"read":["two"],"globScanMaxDepth":4}}})
            )
        );
        for invalid in [
            vec![ResourceId::Entry(1)],
            vec![ResourceId::Read(4)],
            vec![ResourceId::Network, ResourceId::Network],
        ] {
            assert!(grant(method, &body, PermissionDecision::AllowForSession, &invalid).is_none());
        }
        let body = json!({"input":{"file_path":"native"},"tool_name":"Write","permission_suggestions":[{"type":"addRules","destination":"session","rules":[{"toolName":"Write"}]}]});
        assert_eq!(
            select("can_use_tool", &body, PolicyChoice::ClaudeSuggestion(0)).unwrap()["updatedPermissions"]
                [0],
            body["permission_suggestions"][0]
        );
        let acp = json!({"options":[{"kind":"allow_once","optionId":"one"},{"kind":"allow_once","optionId":"two"}]});
        assert_eq!(
            select("session/request_permission", &acp, PolicyChoice::Acp(1)).unwrap()["outcome"]["optionId"],
            "two"
        );
    }
    #[test]
    fn an_explicit_decision_list_takes_precedence_over_proposed_rules() {
        let cmd = "item/commandExecution/requestApproval";
        let amendment = json!({"applyNetworkPolicyAmendment":{"network_policy_amendment":{"host":"fixture.local","action":"allow"}}});
        let advertised = json!({"proposedExecpolicyAmendment":["cargo","test"],"availableDecisions":["cancel",amendment]});
        assert!(select(cmd, &advertised, PolicyChoice::CommandPrefix).is_none());
        assert_eq!(
            select(cmd, &advertised, PolicyChoice::Advertised(1)).unwrap()["decision"],
            amendment
        );
        assert!(select(cmd, &advertised, PolicyChoice::Advertised(0)).is_none());
    }
}
