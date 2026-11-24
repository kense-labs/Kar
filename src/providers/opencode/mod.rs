use crate::providers::ProviderErrorKind as Kind;
pub(crate) mod fork;
pub(crate) mod history;
mod oauth;
pub(crate) mod recovery;
mod registry;
mod server;
use super::{
    LoginProvider, Method, Prompt, ProviderAuthOptions, ProviderInteraction, ProviderStatus,
    interruptible,
};
use crate::{Error, Result, Runtime, account::LoginCancellation};
use registry::Integration;
use reqwest::{Method as Http, StatusCode};
use serde_json::{Value, json};
pub(crate) use server::workspace_skills;
use server::{Pool, Server};
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

/// A provider registry and private, on-demand OpenCode 2 service.
/// No conversation is opened and credentials stay in the vendor's store.
pub struct OpenCodeProviders {
    pool: Arc<Pool>,
    menu: Vec<LoginProvider>,
    options: ProviderAuthOptions,
    mutations: std::sync::Mutex<HashMap<String, Weak<Mutex<()>>>>,
}
impl Runtime {
    pub async fn opencode_provider_auth(
        &self,
        options: ProviderAuthOptions,
    ) -> Result<OpenCodeProviders> {
        let pool = Pool::new(self, &options).await?;
        let lease = pool.lease().await?;
        let menu = list(&lease.server)
            .await?
            .iter()
            .filter_map(Integration::menu)
            .collect();
        Ok(OpenCodeProviders {
            pool,
            menu,
            options,
            mutations: Default::default(),
        })
    }
}
async fn list(s: &Server) -> Result<Vec<Integration>> {
    let mut body = s.request(Http::GET, &["integration"], None).await?;
    serde_json::from_value(body["data"].take())
        .map_err(|_| Error::Protocol("Invalid OpenCode integration registry".into()))
}
async fn find(s: &Server, id: &str) -> Result<Option<Integration>> {
    let (status, mut body) = s.call(Http::GET, &["integration", id], None).await?;
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(Kind::Rejected.http("OpenCode refused the provider request", status));
    }
    let integration: Integration = serde_json::from_value(body["data"].take())
        .map_err(|_| Error::Protocol("Invalid OpenCode integration".into()))?;
    if integration.id != id {
        return Err(Error::Protocol(
            "OpenCode returned another integration".into(),
        ));
    }
    Ok(Some(integration))
}
async fn get(s: &Server, id: &str) -> Result<Integration> {
    find(s, id)
        .await?
        .ok_or_else(|| Kind::UnknownProvider.error("Unknown OpenCode provider"))
}
async fn replace_earlier(s: &Server, before: &Integration) -> Result<ProviderStatus> {
    let after = get(s, &before.id).await?;
    let earlier: std::collections::HashSet<_> = before.credentials().collect();
    if after.credentials().any(|id| !earlier.contains(id)) {
        for id in after.credentials().filter(|id| earlier.contains(id)) {
            if s.request(Http::DELETE, &["credential", id], None)
                .await
                .is_err()
            {
                break;
            }
        }
    }
    Ok(after.status())
}
async fn save_key(
    s: &Server,
    id: &str,
    key: &str,
    answers: serde_json::Map<String, Value>,
) -> Result<()> {
    let mut body = json!({"key":key});
    if !answers.is_empty() {
        body["answer"] = Value::Object(answers);
    }
    s.request(
        Http::POST,
        &["integration", id, "connect", "key"],
        Some(body),
    )
    .await?;
    Ok(())
}
impl OpenCodeProviders {
    // Serialize replacements of one provider without holding up independent sign-ins.
    async fn edit(&self, id: &str) -> Result<OwnedMutexGuard<()>> {
        let lock = {
            let mut entries = self.mutations.lock().unwrap();
            entries.retain(|_, v| v.strong_count() > 0);
            if let Some(lock) = entries.get(id).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                entries.insert(id.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        super::within(self.options.timeout, "OpenCode provider operation", async {
            Ok(lock.lock_owned().await)
        })
        .await
    }
    pub fn login_providers(&self) -> &[LoginProvider] {
        &self.menu
    }
    /// Stored credentials only. OpenCode reports metadata without resolving or refreshing them.
    pub async fn list_providers(&self) -> Result<Vec<ProviderStatus>> {
        let l = self.pool.lease().await?;
        Ok(list(&l.server)
            .await?
            .iter()
            .filter(|i| i.credentials().next().is_some())
            .map(Integration::status)
            .collect())
    }
    pub async fn status(&self, id: &str) -> Result<ProviderStatus> {
        let lease = self.pool.lease().await?;
        Ok(find(&lease.server, id)
            .await?
            .map(|i| i.status())
            .unwrap_or_else(|| ProviderStatus {
                provider_id: id.into(),
                configured: false,
                method: None,
                label: None,
                subscription: None,
            }))
    }
    /// Store a literal key without prompting. A provider with extra required fields must use login().
    pub async fn set_api_key(&self, id: &str, key: &str) -> Result<()> {
        let _lock = self.edit(id).await?;
        let lease = self.pool.lease().await?;
        let s = &lease.server;
        let i = get(s, id).await?;
        let (_, form) = i.key()?;
        if registry::requires_input(form) {
            return Err(Kind::MethodUnavailable
                .error("This provider asks for more than a key. Use login() with api_key."));
        }
        save_key(s, id, key, Default::default()).await?;
        let _ = replace_earlier(s, &i).await;
        Ok(())
    }
    pub async fn logout(&self, id: &str) -> Result<()> {
        let _lock = self.edit(id).await?;
        let lease = self.pool.lease().await?;
        let i = get(&lease.server, id).await?;
        for cred in i.credentials() {
            lease
                .server
                .request(Http::DELETE, &["credential", cred], None)
                .await?;
        }
        Ok(())
    }
    pub async fn login(
        &self,
        id: &str,
        method: Method,
        host: &mut impl ProviderInteraction,
        cancel: &LoginCancellation,
    ) -> Result<ProviderStatus> {
        let deadline = tokio::time::Instant::now()
            .checked_add(self.options.login_timeout)
            .ok_or_else(|| Error::Protocol("Invalid sign-in timeout".into()))?;
        let (_lock, lease, i) = super::until(
            deadline,
            interruptible(cancel, async {
                let lock = self.edit(id).await?;
                let lease = self.pool.lease().await?;
                let i = get(&lease.server, id).await?;
                Ok((lock, lease, i))
            }),
        )
        .await?;
        let s = &lease.server;
        match method {
            Method::Oauth => {
                oauth::login(
                    s.clone(),
                    &i,
                    host,
                    self.options.refuse_loopback,
                    cancel,
                    deadline,
                )
                .await?
            }
            Method::ApiKey => {
                let (message, form) = i.key()?;
                let (key, answer) = super::until(
                    deadline,
                    interruptible(cancel, async {
                        let key = registry::ask(
                            host,
                            Prompt::Secret {
                                message,
                                placeholder: None,
                            },
                        )
                        .await?;
                        Ok((key, registry::answers(form, host).await?))
                    }),
                )
                .await?;
                if cancel.is_cancelled() {
                    return Err(super::cancelled());
                }
                super::until(deadline, save_key(s, id, &key, answer)).await?;
            }
        }
        replace_earlier(s, &i).await
    }
    /// Stop the service immediately. A later request starts a new one.
    pub async fn close(&self) {
        self.pool.close().await
    }
}
