use crate::{Error, Protocol, Result, Session};
use serde_json::{Value, json};

pub(super) fn clean(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn prefix(server: &str) -> String {
    let name = clean(server);
    if server.starts_with("claude.ai ") {
        format!(
            "mcp__{}__",
            name.split('_')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("_")
        )
    } else {
        format!("mcp__{name}__")
    }
}
fn rows(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}
pub(super) fn uri(tool: &Value) -> Option<&str> {
    tool.pointer("/_meta/ui/resourceUri")
        .or_else(|| tool.pointer("/_meta/ui~1resourceUri"))
        .and_then(Value::as_str)
        .filter(|s| s.starts_with("ui://") && s.len() <= 4096)
}
pub(super) fn visible(tool: &Value) -> bool {
    tool.pointer("/_meta/ui/visibility")
        .and_then(Value::as_array)
        .is_none_or(|v| v.iter().any(|s| s == "app"))
}
pub(super) async fn claude_servers(s: &Session) -> Result<Value> {
    let mut v = s.rpc("mcp_status", json!({})).await?;
    if !v["mcpServers"].is_array() {
        return Err(Error::Protocol(
            "Claude omitted its negotiated MCP servers".into(),
        ));
    }
    Ok(v["mcpServers"].take())
}
pub(super) fn capability<'a>(servers: &'a Value, name: &str) -> Result<(&'a str, &'a Value)> {
    let mut matches = Vec::new();
    for s in rows(servers).filter(|s| s["status"] == "connected") {
        let Some(server) = s["name"].as_str() else {
            continue;
        };
        let p = prefix(server);
        if !name.starts_with(&p) {
            continue;
        }
        for t in rows(&s["tools"]) {
            if let Some(tool) = t["name"].as_str()
                && format!("{p}{}", clean(tool)) == name
            {
                matches.push((server, t));
            }
        }
    }
    let [(server, tool)] = matches.as_slice() else {
        return Err(Error::Refused(
            "Claude MCP tool has an unknown or ambiguous native name".into(),
        ));
    };
    let p = prefix(server);
    if rows(servers).any(|s| {
        s["name"].as_str().is_some_and(|other| {
            other != *server && (prefix(other).starts_with(&p) || p.starts_with(&prefix(other)))
        })
    }) {
        return Err(Error::Refused(
            "Claude MCP server namespaces overlap".into(),
        ));
    }
    Ok((server, *tool))
}
fn rules_block(rules: &Value, behaviors: &[&str]) -> bool {
    let state = &rules["state"];
    if state
        .get("errors")
        .is_some_and(|e| e.as_array().is_none_or(|r| !r.is_empty()))
    {
        return true;
    }
    let Some(list) = state["rules"].as_array() else {
        return true;
    };
    list.iter().filter(|r| r["notInEffect"] != true).any(|r| {
        let Some(b) = r["behavior"].as_str() else {
            return true;
        };
        if !matches!(b, "allow" | "deny" | "ask") || !r["rule"].is_string() {
            return true;
        }
        if !behaviors.contains(&b) {
            return false;
        }
        r["rule"].as_str().is_none_or(|rule| {
            let name = rule.split('(').next().unwrap_or("");
            name.starts_with("mcp__") || name.contains('*')
        })
    })
}
pub(super) async fn tool(
    s: &Session,
    server: &str,
    name: &str,
    for_call: bool,
) -> Result<Option<Value>> {
    if s.protocol() == Protocol::Codex {
        let list = s
            .pages(
                "mcpServerStatus/list",
                json!({"threadId":s.id(),"serverName":server,"detail":"toolsAndAuthOnly"}),
            )
            .await?;
        return Ok(rows(&list)
            .find(|r| r["name"] == server)
            .and_then(|r| r["tools"].as_object())
            .and_then(|t| t.values().find(|t| t["name"] == name))
            .cloned());
    }
    let servers = claude_servers(s).await?;
    let Some(t) = rows(&servers)
        .find(|r| r["name"] == server && r["status"] == "connected")
        .and_then(|r| rows(&r["tools"]).find(|t| t["name"] == name))
    else {
        return Ok(None);
    };
    let rules = s
        .rpc("list_permission_rules", json!({}))
        .await
        .unwrap_or(Value::Null);
    if for_call {
        let (origin, selected) =
            capability(&servers, &format!("{}{}", prefix(server), clean(name)))?;
        if origin != server || selected["name"] != name || rules_block(&rules, &["deny"]) {
            return Err(Error::Refused(
                "Claude permission rules or ambiguous tool identity prevent this MCP call".into(),
            ));
        }
    }
    let mut schema = t.clone();
    schema["inputSchema"] = json!({"type":"object"});
    let mut annotations = json!({});
    for (src, dst) in [
        ("destructive", "destructiveHint"),
        ("openWorld", "openWorldHint"),
        ("readOnly", "readOnlyHint"),
    ] {
        if let Some(v) = t["annotations"][src].as_bool()
            && !(src == "readOnly" && v && rules_block(&rules, &["ask", "deny"]))
        {
            annotations[dst] = v.into();
        }
    }
    schema["annotations"] = annotations;
    Ok(Some(schema))
}
pub(super) async fn call(s: &Session, server: &str, name: &str, args: Value) -> Result<Value> {
    match s.protocol() {
        Protocol::Codex => {
            s.rpc(
                "mcpServer/tool/call",
                json!({
                    "threadId":s.id(),"server":server,"tool":name,"arguments":args
                }),
            )
            .await
        }
        Protocol::Claude => {
            let value=s.rpc("mcp_call",json!({
                "tool":format!("{}{}",prefix(server),clean(name)),"arguments":args,"result_not_for_model":true
            })).await?;
            let content = value.get("content").unwrap_or(&value);
            let normalized = if let Some(text) = content.as_str() {
                json!([{"type":"text","text":text}])
            } else if content.is_array() {
                content.clone()
            } else {
                json!([])
            };
            let mut reply = json!({"content":normalized});
            for key in ["structuredContent", "_meta"] {
                if let Some(v) = value.get(key) {
                    reply[key] = v.clone();
                }
            }
            if value["isError"] == true {
                reply["isError"] = true.into();
            }
            Ok(reply)
        }
        _ => Err(Error::Unsupported(
            "MCP application calls on this provider".into(),
        )),
    }
}
pub(super) async fn resource(s: &Session, server: &str, uri: &str) -> Result<Value> {
    match s.protocol() {
        Protocol::Codex => {
            s.rpc(
                "mcpServer/resource/read",
                json!({"threadId":s.id(),"server":server,"uri":uri}),
            )
            .await
        }
        Protocol::Claude => {
            s.rpc("mcp_read_resource", json!({"serverName":server,"uri":uri}))
                .await
        }
        _ => Err(Error::Unsupported(
            "MCP applications on this provider".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_native_capability_requires_unique_name_and_complete_policy() {
        let collide =
            json!([{"status":"connected","name":"one","tools":[{"name":"a.b"},{"name":"a_b"}]}]);
        assert!(capability(&collide, "mcp__one__a_b").is_err());
        let x = json!([{"status":"connected","name":"one","tools":[{"name":"show","_meta":{"ui":{"resourceUri":"ui://one/card"}}}]}]);
        assert_eq!(
            uri(capability(&x, "mcp__one__show").unwrap().1),
            Some("ui://one/card")
        );
        assert!(rules_block(&Value::Null, &["deny"]));
        assert!(rules_block(
            &json!({"state":{"rules":[],"errors":["bad settings"]}}),
            &["deny"]
        ));
        assert!(rules_block(
            &json!({"state":{"rules":[{"behavior":"deny","rule":"mcp__one__*"}]}}),
            &["deny"]
        ));
        assert!(!rules_block(
            &json!({"state":{"rules":[{"behavior":"deny","rule":"Read"}]}}),
            &["deny"]
        ));
        assert!(!visible(&json!({"_meta":{"ui":{"visibility":["model"]}}})));
    }
}
