//! Read a temporary ACP session's model menu without submitting a prompt.
use crate::{
    Error, FailureClass, OpenError, Protocol, Result, Runtime, SessionOptions, TurnFailure,
    account::{AccountWire, AuthStatus},
    forms::ModelEntry,
    installation::Probe,
};
use serde_json::{Map, Value, json};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Menu {
    Devin,
    CodeBuddy,
    Kimi,
}

impl Menu {
    pub(crate) fn for_runtime(runtime: &Runtime) -> Option<Self> {
        match (runtime.id.as_str(), runtime.protocol) {
            ("devin", Protocol::Acp) => Some(Self::Devin),
            ("codebuddy", Protocol::Acp) => Some(Self::CodeBuddy),
            ("kimi", Protocol::KimiKap | Protocol::Acp) => Some(Self::Kimi),
            _ => None,
        }
    }

    pub(crate) async fn list(self, rt: &Runtime, opts: &SessionOptions) -> Result<Vec<ModelEntry>> {
        let read = async {
            let rt = rt.selected(opts).await?;
            let probe = Probe::for_session(opts)?;
            let mut cmd = probe.command(&rt.program);
            // Kimi's conversation opener negotiates KAP, while its account model menu
            // is exposed by the executable's ACP subcommand on both release lines.
            if self == Self::Kimi && rt.protocol == Protocol::KimiKap {
                cmd.arg("acp");
            }
            cmd.args(&rt.args);
            if self == Self::CodeBuddy {
                cmd.args(["--permission-mode", "bypassPermissions"]);
            }
            let mut query = Query {
                wire: AccountWire::open(cmd)?,
                menu: self,
                next_id: 1,
            };
            let hello = query
                .request(
                    "initialize",
                    json!({"protocolVersion":1,
                "clientInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")},
                "clientCapabilities": AccountWire::capabilities()}),
                )
                .await?;
            if hello.get("protocolVersion").is_some_and(|v| *v != 1) {
                return Err(Error::Protocol(
                    "unexpected ACP version in model query".into(),
                ));
            }
            if self == Self::Kimi && rows(&hello["authMethods"]).any(|m| m["id"] == "login") {
                query
                    .request("authenticate", json!({"methodId":"login"}))
                    .await?;
            }
            let opened = query
                .request("session/new", json!({"cwd":probe.cwd,"mcpServers":[]}))
                .await?;
            let models = project(self, &opened);
            let capabilities = &hello["agentCapabilities"]["sessionCapabilities"];
            let offered = |name: &str| capabilities[name].is_object() || capabilities[name] == true;
            let cleanup = match self {
                Self::Devin if offered("delete") => Some("session/delete"),
                Self::Devin if offered("close") => Some("session/close"),
                Self::Kimi => Some("session/close"),
                _ => None,
            };
            if let (Some(method), Some(id)) = (cleanup, text(&opened["sessionId"])) {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    query.request(method, json!({"sessionId":id})),
                )
                .await;
            }
            drop(query);
            let models = models?;
            if self == Self::Devin && models.is_empty() {
                match crate::account::devin_auth_status(&probe, &rt.program).await? {
                    AuthStatus::LoggedIn { .. } => {}
                    AuthStatus::LoggedOut { detail, .. } => {
                        return Err(auth_error(
                            detail.as_deref().unwrap_or("Devin requires authentication"),
                        ));
                    }
                    AuthStatus::Unknown {
                        configured, detail, ..
                    } if !configured.is_empty() => {
                        return Err(Error::Unsupported(detail));
                    }
                    AuthStatus::Unknown { detail, .. } => return Err(Error::Refused(detail)),
                }
            }
            Ok(models)
        };
        tokio::time::timeout(opts.handshake_timeout, read)
            .await
            .map_err(|_| Error::Timeout(format!("{} model listing", rt.id)))?
    }
}

fn auth_error(message: &str) -> Error {
    OpenError::RuntimeFailure {
        failure: TurnFailure::new(FailureClass::Auth, message),
        code: None,
        cause: None,
    }
    .into()
}

struct Query {
    wire: AccountWire,
    menu: Menu,
    next_id: u64,
}
impl Query {
    async fn request(&mut self, method: &'static str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut reply = self.wire.rpc(id, method, params).await?;
        if let Some(error) = reply.get("error").filter(|v| !v.is_null()) {
            let code = error["code"].as_i64();
            let message = error["message"].as_str().unwrap_or("").to_lowercase();
            if self.menu == Menu::Kimi && code == Some(-32601) {
                return Err(Error::Unsupported(format!("Kimi has no ACP {method}")));
            }
            if code == Some(-32000)
                || message.contains("auth")
                || message.contains("login")
                || message.contains("log in")
                || message.contains("logged in")
                || (self.menu == Menu::Devin && message.contains("invalid api key"))
            {
                return Err(auth_error(
                    "The runtime requires authentication to list models",
                ));
            }
            if let Some(code) = code {
                return Err(Error::ProviderRpc { method, code });
            }
            return Err(Error::Refused(format!("ACP rejected {method}")));
        }
        if !reply["result"].is_object() {
            return Err(Error::Protocol(format!(
                "ACP omitted the result of {method}"
            )));
        }
        Ok(reply["result"].take())
    }
}

fn rows(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|row| row.is_object())
}
fn text(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}
fn selector(v: &Value) -> Option<&Value> {
    rows(&v["configOptions"]).find(|o| o["id"] == "model")
}
fn thought(v: &Value) -> Option<&Value> {
    rows(&v["configOptions"]).find(|o| o["category"] == "thought_level")
}

fn project(menu: Menu, opened: &Value) -> Result<Vec<ModelEntry>> {
    let models = selector(opened).ok_or_else(|| {
        Error::Unsupported("the ACP server returned no model option on session/new".into())
    })?;
    let current = text(&models["currentValue"]);
    let level = |v: &Value| -> Option<String> {
        let value = if menu == Menu::Kimi {
            v.as_str()
                .or_else(|| v.get("effort").unwrap_or(&v["id"]).as_str())
        } else {
            text(v)
        }?;
        if value.is_empty()
            || (menu == Menu::Kimi && value == "off")
            || (menu == Menu::CodeBuddy && matches!(value, "enabled" | "disabled"))
        {
            return None;
        }
        Some(value.into())
    };
    let thinking = thought(opened);
    let levels: Vec<_> = thinking
        .into_iter()
        .flat_map(|o| rows(&o["options"]))
        .filter_map(|o| level(&o["value"]))
        .collect();
    let initial = thinking.and_then(|o| level(&o["currentValue"]));
    let mut result = vec![];
    for option in rows(&models["options"]) {
        let Some(id) = text(&option["value"]) else {
            continue;
        };
        let default = Some(id) == current;
        let mut metadata = Map::new();
        if let Some(name) = text(&option["name"]).filter(|s| menu == Menu::Kimi || *s != id) {
            metadata.insert("displayName".into(), name.into());
        }
        if default
            && let Some(effort) = &initial
            && (menu != Menu::CodeBuddy || levels.contains(effort))
        {
            metadata.insert("defaultEffort".into(), effort.clone().into());
        }
        result.push(ModelEntry {
            id: id.into(),
            default,
            metadata,
            disabled: None,
            effort_levels: (default && !levels.is_empty()).then(|| levels.clone()),
        });
    }
    Ok(result)
}
