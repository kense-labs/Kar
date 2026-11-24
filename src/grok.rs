use crate::{
    Error, FailureClass, NativeCause, OpenError, Result, Runtime, SessionOptions, TurnFailure,
};
use serde_json::{Value, json};

pub(crate) mod mcp;
pub(crate) mod terminal;
pub(crate) mod turns;

const LIST: &str = "_x.ai/models/list";

impl Runtime {
    /// Read the runtime form's live model choices and preserve the reason for any fallback.
    pub async fn form_model_listing(
        &self,
        options: &SessionOptions,
        bundled: Vec<crate::forms::ModelEntry>,
    ) -> crate::forms::ModelListing {
        use crate::forms::{ListingReason, ModelListing};
        let read = async {
            if let Some(menu) = crate::acp_models::Menu::for_runtime(self) {
                return menu.list(self, options).await;
            }
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            if self.id == "dsh" {
                return crate::inventory::dsh::model_choices(self, options).await;
            }
            if self.protocol == crate::Protocol::Pi && self.id == "pi" {
                return crate::providers::pi::model_choices(self, options).await;
            }
            if self.protocol == crate::Protocol::Qoder && self.id == "qoder" {
                return crate::qoder::picker::read(self, options).await;
            }
            if self.protocol == crate::Protocol::Morph && self.id == "morph" {
                if !options.env.is_empty() || !options.unset_env.is_empty() {
                    return Err(Error::Unsupported(
                        "Morph Console owns its environment".into(),
                    ));
                }
                let value = crate::community::Morph::from_env(&self.program)?
                    .models()
                    .await?;
                return Ok(serde_json::from_value(value)?);
            }
            if self.protocol == crate::Protocol::Claude && self.id == "claude" {
                let rows = crate::inventory::claude_models(&self.selected(options).await?, options)
                    .await?;
                return crate::inventory::model_choices::project(&rows, self.protocol);
            }
            if self.protocol == crate::Protocol::Factory && self.id == "factory" {
                let rows = crate::factory::inventory(
                    &self.selected(options).await?,
                    options,
                    crate::Catalog::Models,
                )
                .await?;
                return crate::inventory::model_choices::project(&rows, self.protocol);
            }
            if self.protocol == crate::Protocol::Codex && self.id == "codex" {
                let models = crate::inventory::codex_models::read(
                    &self.selected(options).await?,
                    options,
                    true,
                )
                .await?;
                return Ok(serde_json::from_value(models)?);
            }
            if self.protocol == crate::Protocol::Qwen && self.id == "qwen" {
                let catalog = self
                    .catalog(crate::Catalog::Models, options.clone())
                    .await?;
                return Ok(serde_json::from_value(catalog)?);
            }
            if self.protocol == crate::Protocol::Acp && self.id == "cursor" {
                let models = crate::cursor_models::catalog(&self.selected(options).await?, options)
                    .await?
                    .ok_or_else(|| {
                        Error::Unsupported("Cursor has no parameterized model catalog".into())
                    })?;
                return Ok(serde_json::from_value(models)?);
            }
            if self.id == "opencode" && self.protocol == crate::Protocol::Acp {
                return crate::options::opencode::models::list(self, options).await;
            }
            if self.id != "grok" || self.protocol != crate::Protocol::Acp {
                return Err(Error::Unsupported(
                    "no model listing for this runtime form".into(),
                ));
            }
            let state = read_state(&self.selected(options).await?, options).await?;
            let rows = models(&state, false)?;
            Ok(serde_json::from_value(rows)?)
        };
        let result = match tokio::time::timeout(options.handshake_timeout, read).await {
            Ok(Ok(models)) => Ok(models),
            Err(_) | Ok(Err(Error::Timeout(_))) => Err(ListingReason::Timeout),
            Ok(Err(Error::Provider(e))) => Err(match e.kind {
                crate::providers::ProviderErrorKind::TimedOut => ListingReason::Timeout,
                crate::providers::ProviderErrorKind::AuthenticationFailed => {
                    ListingReason::Unauthenticated
                }
                crate::providers::ProviderErrorKind::UnsupportedInstallation
                | crate::providers::ProviderErrorKind::VersionUnsupported => {
                    ListingReason::Unsupported
                }
                _ => ListingReason::Failed,
            }),
            Ok(Err(Error::Unsupported(_))) => Err(ListingReason::Unsupported),
            Ok(Err(Error::Open(error))) if matches!(*error, OpenError::RuntimeFailure { ref failure, .. } if failure.class == FailureClass::Auth) => {
                Err(ListingReason::Unauthenticated)
            }
            Ok(Err(_)) => Err(ListingReason::Failed),
        };
        ModelListing::from_result(result, bundled)
    }
}

pub(crate) fn initialize(options: &SessionOptions) -> Value {
    let mut meta = json!({"clientIdentifier":"kar","clientType":"generic",
        "startupHints":{"nonInteractive":true,"skipGitStatus":true,"skipProjectLayout":true}});
    if let Some(prompt) = &options.settings.system_prompt {
        meta["systemPromptOverride"] = prompt.clone().into();
    }
    if let Some(rules) = &options.settings.append_system_prompt {
        meta["rules"] = rules.clone().into();
    }
    meta
}

pub(crate) fn auth_method(hello: &Value) -> Option<&str> {
    let methods = hello["authMethods"].as_array()?;
    let offered = |id: &str| methods.iter().any(|m| m["id"] == id);
    hello["_meta"]["defaultAuthMethodId"]
        .as_str()
        .filter(|id| offered(id))
        .or_else(|| offered("cached_token").then_some("cached_token"))
}

pub(crate) fn reader_hello(options: &SessionOptions) -> Value {
    json!({"protocolVersion":1,"clientCapabilities":crate::account::AccountWire::capabilities(),
        "clientInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")},"_meta":initialize(options)})
}

fn result(frame: Value, method: &str, secrets: &[String]) -> Result<Value> {
    if let Some(native) = frame.get("error") {
        let code = native["code"].as_i64();
        if code == Some(-32601) {
            return Err(Error::Unsupported(format!(
                "Grok does not implement {method}"
            )));
        }
        let mut native = native.clone();
        crate::transport::redact(&mut native, secrets);
        let message = native["message"]
            .as_str()
            .or_else(|| native.as_str())
            .unwrap_or("Grok refused the request");
        let lower = message.to_ascii_lowercase();
        if code == Some(-32000)
            || lower.contains("auth")
            || lower.contains("log in")
            || lower.contains("logged in")
            || lower.contains("login")
        {
            return Err(OpenError::RuntimeFailure {
                failure: TurnFailure::new(FailureClass::Auth, message),
                code: None,
                cause: Some(NativeCause {
                    method: method.into(),
                    native,
                }),
            }
            .redacted(secrets));
        }
        return Err(Error::Refused(format!("{method}: {message}")));
    }
    frame
        .get("result")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| Error::Protocol(format!("Grok omitted the result of {method}")))
}

fn first<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(k).filter(|v| !v.is_null()))
}
fn name<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    first(v, keys)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
fn effort(v: &Value) -> Option<&str> {
    v.as_str()
        .or_else(|| first(v, &["effort", "id"])?.as_str())
        .filter(|s| !s.is_empty())
}
fn models(state: &Value, keep_native_fields: bool) -> Result<Value> {
    let current = name(state, &["currentModelId", "current_model_id"]);
    let available = first(state, &["availableModels", "available_models"])
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("Grok omitted its available models".into()))?;
    let entries = available
        .iter()
        .filter_map(|m| {
            let id = name(m, &["modelId", "model_id", "id"])?;
            if m["hidden"] == true || m["userSelectable"] == false || m["user_selectable"] == false
            {
                return None;
            }
            let mut entry = if keep_native_fields {
                m.clone()
            } else {
                json!({})
            };
            entry["id"] = id.into();
            if let Some(label) = name(m, &["displayName", "display_name", "name"]) {
                entry["displayName"] = label.into();
            }
            if current == Some(id) {
                entry["default"] = true.into();
            }
            let meta = &m["_meta"];
            if meta["supportsReasoningEffort"] != false {
                let menus = &["reasoningEfforts", "reasoning_efforts"];
                if let Some(levels) = first(m, menus)
                    .or_else(|| first(meta, menus))
                    .and_then(Value::as_array)
                {
                    entry["effortLevels"] =
                        json!(levels.iter().filter_map(effort).collect::<Vec<_>>());
                }
                let default = first(
                    m,
                    &[
                        "defaultReasoningEffort",
                        "default_reasoning_effort",
                        "reasoningEffort",
                        "reasoning_effort",
                    ],
                )
                .or_else(|| first(meta, &["reasoningEffort", "reasoning_effort"]))
                .and_then(effort);
                if let Some(default) = default {
                    entry["defaultEffort"] = default.into();
                }
            }
            Some(entry)
        })
        .collect();
    Ok(Value::Array(entries))
}

pub(crate) async fn catalog(runtime: &Runtime, options: &SessionOptions) -> Result<Value> {
    // Existing CLI and catalog callers can still read the native ACP model fields.
    models(&read_state(runtime, options).await?, true)
}

async fn read_state(runtime: &Runtime, options: &SessionOptions) -> Result<Value> {
    let mut cmd = options.command(&runtime.program)?;
    cmd.args(&runtime.args).current_dir(&options.cwd);
    let mut wire = crate::account::AccountWire::open(cmd)?;
    let secrets = crate::options::session_secrets(options);
    let read = async {
        let hello = reader_hello(options);
        let info = result(
            wire.rpc(1, "initialize", hello).await?,
            "initialize",
            &secrets,
        )?;
        if info["protocolVersion"] != 1 {
            return Err(Error::Protocol(
                "Grok did not negotiate ACP version 1".into(),
            ));
        }
        if let Some(id) = auth_method(&info) {
            result(
                wire.rpc(2, "authenticate", json!({"methodId":id})).await?,
                "authenticate",
                &secrets,
            )?;
        }
        let envelope = result(wire.rpc(3, LIST, json!({})).await?, LIST, &secrets)?;
        let mut state = if envelope.get("error").is_some() || envelope.get("result").is_some() {
            result(envelope, LIST, &secrets)?
        } else {
            envelope
        };
        crate::transport::redact(&mut state, &secrets);
        Ok(state)
    };
    let outcome = tokio::time::timeout(options.handshake_timeout, read).await;
    wire.close().await;
    outcome.map_err(|_| Error::Timeout("Grok model listing".into()))?
}
