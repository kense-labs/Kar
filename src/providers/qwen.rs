use super::{ProviderAuthOptions, within};
use crate::{
    Error, InstallationSnapshot, Result, Runtime, account::AccountWire, installation::Probe,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{path::PathBuf, sync::Mutex, time::Duration};
const LIST: &str = "qwen/providers/list";
const CONNECT: &str = "qwen/providers/connect";

#[derive(Clone, Copy, Debug, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum QwenProtocol {
    Openai,
    Anthropic,
    Gemini,
}
#[derive(Clone, Copy, Debug, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum QwenWireApi {
    ChatCompletions,
    Responses,
}
#[derive(Clone, Copy, Debug, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum QwenScope {
    User,
    Workspace,
}

/// Options for a private native ACP request. Values stay off the command line.
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QwenConnect {
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<QwenProtocol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wire_api: Option<QwenWireApi>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<QwenScope>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QwenProvider {
    pub id: String,
    pub label: String,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_config: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub metadata: Map<String, Value>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QwenConnected {
    pub success: bool,
    pub provider_id: String,
    pub provider_label: String,
    pub auth_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}
pub struct QwenProviders {
    command: PathBuf,
    probe: Probe,
    timeout: Duration,
    secrets: Mutex<Vec<String>>,
}
impl Runtime {
    pub async fn qwen_provider_auth(&self, options: ProviderAuthOptions) -> Result<QwenProviders> {
        if self.id != "qwen" {
            return Err(Error::Unsupported(
                "Qwen provider configuration requires Qwen Code".into(),
            ));
        }
        let probe = Probe::new(&options.installation)?;
        let installation = within(options.timeout, "Qwen installation", async {
            Ok(probe.discover(self).await)
        })
        .await?;
        let InstallationSnapshot::Available {
            command: Some(command),
            ..
        } = installation
        else {
            return Err(Error::Unsupported(
                "Qwen provider configuration requires an installed executable".into(),
            ));
        };
        let mut secrets = Vec::new();
        if let Some(shape) = self.credentials() {
            for field in shape.env.iter().filter(|f| f.secret) {
                if let Some(s) = probe
                    .get(&field.name)
                    .and_then(|v| v.to_str())
                    .filter(|s| !s.is_empty())
                {
                    secrets.push(s.to_owned());
                }
            }
        }
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Ok(QwenProviders {
            command,
            probe,
            timeout: options.timeout,
            secrets: Mutex::new(secrets),
        })
    }
}
fn result(method: &'static str, mut f: Value) -> Result<Value> {
    if f.get("error").is_some() {
        return match f["error"]["code"].as_i64() {
            Some(code) => Err(Error::ProviderRpc { method, code }),
            None => Err(Error::Protocol(format!("Invalid error from {method}"))),
        };
    }
    f.get_mut("result")
        .map(Value::take)
        .ok_or_else(|| Error::Protocol(format!("Missing response from {method}")))
}
fn redact_keys(v: &mut Value, secrets: &[String]) {
    match v {
        Value::Object(m) => {
            for (mut key, mut v) in std::mem::take(m) {
                for s in secrets {
                    if key.contains(s) {
                        key = key.replace(s, "[redacted]");
                    }
                }
                redact_keys(&mut v, secrets);
                m.insert(key, v);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| redact_keys(v, secrets)),
        _ => {}
    }
}
impl QwenProviders {
    async fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        let mut cmd = self.probe.command(&self.command);
        cmd.arg("--acp");
        let mut wire = AccountWire::open(cmd)?;
        let f = async {
            let init = wire
                .rpc(
                    1,
                    "initialize",
                    json!({"protocolVersion":1,"clientCapabilities":AccountWire::capabilities()}),
                )
                .await?;
            if result("initialize", init)?["protocolVersion"] != 1 {
                return Err(Error::Protocol("Qwen ACP version mismatch".into()));
            }
            let expected = params["providerId"].as_str().map(str::to_owned);
            let mut data = result(method, wire.rpc(2, method, params).await?)?;
            if method == CONNECT
                && (data["providerId"].as_str() != expected.as_deref()
                    || ["modelId", "baseUrl"]
                        .iter()
                        .any(|k| data.get(k).is_some_and(|v| !v.is_string())))
            {
                return Err(Error::Protocol(
                    "Invalid Qwen provider configuration receipt".into(),
                ));
            }
            let secrets = self.secrets.lock().unwrap();
            crate::transport::redact(&mut data, &secrets);
            redact_keys(&mut data, &secrets);
            Ok(data)
        };
        let r = within(self.timeout, "Qwen provider operation", f).await;
        wire.close().await;
        r
    }
    /// Native settings metadata in the selected working directory.
    pub async fn list_providers(&self) -> Result<Vec<QwenProvider>> {
        let mut data = self.request(LIST, json!({})).await?;
        if data["providers"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|p| p.get("existingConfig").is_some_and(|v| !v.is_object()))
        }) {
            return Err(Error::Protocol("Invalid Qwen provider metadata".into()));
        }
        serde_json::from_value(data["providers"].take())
            .map_err(|_| Error::Protocol("Invalid Qwen provider list".into()))
    }
    /// Configuration receipt from Qwen; the provider is not contacted to validate the key.
    /// A lost reply may follow a successful write. Mutations are never retried automatically.
    pub async fn connect(&self, input: &QwenConnect) -> Result<QwenConnected> {
        if let Some(s) = input.api_key.as_deref() {
            let mut secrets = self.secrets.lock().unwrap();
            for v in [s, s.trim()] {
                if !v.is_empty() && !secrets.iter().any(|k| k == v) {
                    secrets.push(v.to_owned());
                }
            }
            secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
        let data = self.request(CONNECT, serde_json::to_value(input)?).await?;
        let result: QwenConnected = serde_json::from_value(data)
            .map_err(|_| Error::Protocol("Invalid Qwen provider configuration receipt".into()))?;
        if !result.success {
            return Err(Error::Refused(
                "Qwen did not confirm provider configuration".into(),
            ));
        }
        Ok(result)
    }
}
