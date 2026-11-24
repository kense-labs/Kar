use super::{PiModelRuntimePaths, PiProviders, sdk::Host};
use crate::{
    Error, Result, Runtime,
    providers::{ProviderAuthOptions, ProviderErrorKind, within},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::{mpsc, oneshot};

pub use crate::account::LoginCancellation as CatalogCancellation;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    pub configured: bool,
    #[serde(skip_serializing)]
    default_model: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    pub id: String,
    pub provider_id: String,
    pub wire: String,
    pub base_url: String,
    pub context_window: serde_json::Number,
    pub max_tokens: serde_json::Number,
    pub reasoning: bool,
    pub input: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRefreshOptions {
    /// None refreshes all providers, an empty list refreshes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<String>>,
    pub allow_network: bool,
}
impl Default for CatalogRefreshOptions {
    fn default() -> Self {
        Self {
            providers: None,
            allow_network: true,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogRefreshResult {
    pub aborted: bool,
    pub errors: BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct Snapshot {
    providers: Vec<CatalogProvider>,
    #[serde(skip)]
    models: Vec<CatalogModel>,
    count: usize,
    refresh: Option<CatalogRefreshResult>,
}
async fn receive(host: &mut Host, models: &mut Vec<CatalogModel>) -> Result<Snapshot> {
    loop {
        let (tag, data) = host.next().await?;
        match tag.as_str() {
            "MODELS" => models.extend(serde_json::from_str::<Vec<CatalogModel>>(&data)?),
            "CATALOG" => {
                let mut snapshot: Snapshot = serde_json::from_str(&data)?;
                if snapshot.count != models.len() {
                    return Err(Error::Protocol("Incomplete Pi catalog".into()));
                }
                snapshot.models = std::mem::take(models);
                return Ok(snapshot);
            }
            _ => return Err(Error::Protocol("Expected a Pi catalog snapshot".into())),
        }
    }
}

struct Request {
    options: CatalogRefreshOptions,
    cancel: CatalogCancellation,
    reply: oneshot::Sender<Result<Snapshot>>,
}
impl Request {
    async fn run(&mut self, host: &mut Host) -> Result<Snapshot> {
        host.send(&json!({"refresh": self.options, "aborted":self.cancel.is_cancelled()}))
            .await?;
        let mut models = Vec::new();
        tokio::select! {
            result = receive(host,&mut models) => return result,
            _ = self.cancel.cancelled() => {},
            _ = self.reply.closed() => {},
        }
        host.send(&json!({"cancel":true})).await?;
        receive(host, &mut models).await
    }
}

/// A live Pi ModelRuntime with synchronous snapshot reads and explicit refresh.
pub struct PiModelCatalog {
    snapshot: Snapshot,
    requests: mpsc::Sender<Request>,
}
impl Runtime {
    pub async fn pi_model_catalog(
        &self,
        options: ProviderAuthOptions,
        paths: PiModelRuntimePaths,
    ) -> Result<PiModelCatalog> {
        self.pi_provider_auth_with_paths(options, paths)
            .await?
            .model_catalog()
            .await
    }
}
impl PiProviders {
    pub async fn model_catalog(&self) -> Result<PiModelCatalog> {
        let mut host = self.host().await?.ok_or_else(|| {
            ProviderErrorKind::UnsupportedInstallation
                .error("Pi model catalogs require an installation with a loadable Pi SDK")
        })?;
        let timeout = self.timeout;
        let snapshot = within(timeout, "Pi model catalog", async {
            host.send(&Host::config(self, "catalog")?).await?;
            receive(&mut host, &mut Vec::new()).await
        })
        .await?;
        let (requests, reader) = mpsc::channel(1);
        tokio::spawn(serve(host, reader, timeout));
        Ok(PiModelCatalog { snapshot, requests })
    }
}
// One worker owns the pipe and drains each reply, including when a caller
// drops its refresh future. The next request cannot consume a stale reply.
async fn serve(mut host: Host, mut requests: mpsc::Receiver<Request>, timeout: Duration) {
    while let Some(mut job) = requests.recv().await {
        if job.reply.is_closed() {
            continue;
        }
        let result = within(timeout, "Pi catalog refresh", job.run(&mut host)).await;
        let failed = result.is_err();
        let _ = job.reply.send(result);
        if failed {
            break;
        }
    }
    host.close().await;
}
impl PiModelCatalog {
    pub fn providers(&self) -> &[CatalogProvider] {
        &self.snapshot.providers
    }
    pub fn models<'a>(
        &'a self,
        provider: Option<&'a str>,
    ) -> impl Iterator<Item = &'a CatalogModel> {
        self.snapshot
            .models
            .iter()
            .filter(move |model| provider.is_none_or(|id| id == model.provider_id))
    }
    pub fn default_model(&self, provider: &str) -> Option<&str> {
        self.providers()
            .iter()
            .find(|p| p.id == provider)
            .map(|p| p.default_model.as_str())
    }
    pub async fn refresh(
        &mut self,
        options: CatalogRefreshOptions,
        cancel: &CatalogCancellation,
    ) -> Result<CatalogRefreshResult> {
        if cancel.is_cancelled() {
            return Ok(CatalogRefreshResult {
                aborted: true,
                errors: BTreeMap::new(),
            });
        }
        let (reply, response) = oneshot::channel();
        let closed = || Error::Closed("Pi catalog host stopped".into());
        self.requests
            .send(Request {
                options,
                cancel: cancel.clone(),
                reply,
            })
            .await
            .map_err(|_| closed())?;
        let mut snapshot = response.await.map_err(|_| closed())??;
        let result = snapshot
            .refresh
            .take()
            .ok_or_else(|| Error::Protocol("Missing Pi refresh result".into()))?;
        self.snapshot = snapshot;
        Ok(result)
    }
}

pub(crate) async fn picker(
    rt: &Runtime,
    opts: &crate::SessionOptions,
) -> Result<Vec<crate::forms::ModelEntry>> {
    let config = ProviderAuthOptions {
        installation: opts.installation_options(),
        timeout: opts.handshake_timeout,
        ..Default::default()
    };
    let providers = rt.pi_provider_auth(config).await?;
    let mut host = providers.host().await?.ok_or_else(|| {
        Error::Unsupported("Pi model choices need an installed matching Pi SDK".into())
    })?;
    let mut cfg = Host::config(&providers, "picker")?;
    cfg["timeoutMs"] = json!(opts.handshake_timeout.as_millis().min(i32::MAX as u128) as u64);
    let result = async {
        host.send(&cfg).await?;
        let data = host.picker().await.map_err(|e| match e {
            Error::Timeout(_) => e,
            _ => Error::Unsupported("Pi SDK could not list workspace models".into()),
        })?;
        project_picker(&serde_json::from_str(&data)?)
    }
    .await;
    host.close().await;
    result
}

fn project_picker(data: &serde_json::Value) -> Result<Vec<crate::forms::ModelEntry>> {
    let rows = data["models"]
        .as_array()
        .ok_or_else(|| Error::Protocol("Pi omitted available models".into()))?;
    let all = data["all"]
        .as_u64()
        .ok_or_else(|| Error::Protocol("Pi omitted its complete model count".into()))?;
    if rows.is_empty() && all > 0 {
        return Err(ProviderErrorKind::AuthenticationFailed
            .error("Pi has catalog entries but no credentialed model"));
    }
    rows.iter()
        .map(|m| {
            let id = m["id"]
                .as_str()
                .ok_or_else(|| Error::Protocol("Pi omitted its model ID".into()))?;
            let provider = m["provider"]
                .as_str()
                .ok_or_else(|| Error::Protocol("Pi omitted the model's provider".into()))?;
            let mut native = json!({"id":provider});
            if let Some(name) = m["providerName"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != provider)
            {
                native["displayName"] = name.into();
            }
            let mut metadata = serde_json::Map::new();
            metadata.insert("provider".into(), native);
            metadata.insert(
                "displayName".into(),
                m["name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(id)
                    .into(),
            );
            let efforts = m["levels"]
                .as_array()
                .filter(|l| !l.is_empty())
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                });
            Ok(crate::forms::ModelEntry {
                id: format!("{provider}/{id}"),
                default: data["defaultProvider"] == provider && data["defaultModel"] == id,
                effort_levels: efforts,
                disabled: None,
                metadata,
            })
        })
        .collect()
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    #[test]
    fn saved_workspace_default_and_native_thinking_are_scoped_to_available_models() {
        let data = json!({"all":9,"models":[{"id":"same","provider":"a","name":" Alias ","providerName":"a"},{"id":"same","provider":"b","providerName":"Bee","levels":["off","medium","xhigh"]}],"defaultProvider":"b","defaultModel":"same"});
        let rows = project_picker(&data).unwrap();
        assert_eq!(rows[0].id, "a/same");
        assert!(!rows[0].default);
        assert_eq!(rows[0].metadata["displayName"], "Alias");
        assert!(
            rows[0].metadata["provider"].get("displayName").is_none()
                && rows[0].effort_levels.is_none()
        );
        assert!(rows[1].default);
        assert_eq!(rows[1].metadata["provider"]["displayName"], "Bee");
        assert_eq!(
            rows[1].effort_levels.as_ref().unwrap(),
            &["off", "medium", "xhigh"]
        );
        assert!(!rows[1].metadata.contains_key("defaultEffort"));
        assert!(
            project_picker(&json!({"all":0,"models":[]}))
                .unwrap()
                .is_empty()
        );
        assert!(
            matches!(project_picker(&json!({"all":2,"models":[]})),Err(Error::Provider(e)) if e.kind==ProviderErrorKind::AuthenticationFailed)
        );
    }
}
