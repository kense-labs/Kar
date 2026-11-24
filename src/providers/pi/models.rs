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
