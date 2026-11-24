//! Server-managed browser tabs. A browser profile belongs to one project and device.
mod cdp;
pub(in crate::workbench) mod html;
mod network;
pub(super) mod profiles;
mod settings;
use self::{
    cdp::Browser,
    settings::{Clear, Color},
};
use super::preview::Grant;
use crate::{Error, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
pub(super) use settings::{Adjust, Display, Viewport, profile};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

#[derive(Clone, Eq, Hash, PartialEq)]
struct Key {
    project: Uuid,
    device: Option<Uuid>,
    profile: String,
    ephemeral_tab: Option<Uuid>,
}
struct Tab {
    target: String,
    session: String,
}
struct Host {
    browser: Browser,
    tabs: HashMap<Uuid, Tab>,
    _temporary: Option<tempfile::TempDir>,
}
struct Slot {
    dir: PathBuf,
    ephemeral: bool,
    host: Option<Host>,
}
type Handle = Arc<AsyncMutex<Slot>>;

pub(super) struct Pool {
    root: PathBuf,
    slots: Mutex<HashMap<Key, Weak<AsyncMutex<Slot>>>>,
    active: Mutex<HashMap<Uuid, Handle>>,
}
impl Pool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            slots: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
        }
    }
    fn slot(&self, key: Key) -> Handle {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get(&key).and_then(Weak::upgrade) {
            return slot;
        }
        slots.retain(|_, s| s.strong_count() > 0);
        let dir = self
            .root
            .join(key.project.to_string())
            .join(key.device.map_or_else(|| "local".into(), |x| x.to_string()))
            .join(format!("{:x}", Sha256::digest(key.profile.as_bytes())));
        let slot = Arc::new(AsyncMutex::new(Slot {
            dir,
            ephemeral: key.ephemeral_tab.is_some(),
            host: None,
        }));
        slots.insert(key, Arc::downgrade(&slot));
        slot
    }
    fn bind(self: &Arc<Self>, grant: &Grant) -> Handle {
        let id = grant.info.id;
        let mut bound = self.active.lock().unwrap();
        if let Some(slot) = bound.get(&id) {
            return slot.clone();
        }
        let key = Key {
            project: grant.info.project,
            device: grant.owner(),
            profile: grant.info.profile_id.clone(),
            ephemeral_tab: (grant.info.profile_id == "incognito").then_some(id),
        };
        let slot = self.slot(key);
        bound.insert(id, slot.clone());
        let pool = Arc::downgrade(self);
        let mut stop = grant.revocation();
        tokio::spawn(async move {
            let _ = stop.wait_for(|v| *v).await;
            let Some(pool) = pool.upgrade() else {
                return;
            };
            let handle = pool.active.lock().unwrap().remove(&id);
            if let Some(handle) = handle {
                handle.lock().await.close(id).await;
            }
        });
        slot
    }
    pub async fn clear_profile(
        &self,
        project: Uuid,
        device: Option<Uuid>,
        profile_id: String,
    ) -> Result<()> {
        profile(&profile_id)?;
        if profile_id == "incognito" {
            return Ok(());
        }
        let slot = self.slot(Key {
            project,
            device,
            profile: profile_id,
            ephemeral_tab: None,
        });
        let mut slot = slot.lock().await;
        slot.shutdown().await;
        match tokio::fs::remove_dir_all(&slot.dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn control(
        self: &Arc<Self>,
        grant: Arc<Grant>,
        viewport: Option<Viewport>,
        adjust: Adjust,
    ) -> Result<Value> {
        let handle = self.bind(&grant);
        let mut slot = handle.lock().await;
        let adjusted = viewport.is_some()
            || adjust.zoom_factor.is_some()
            || adjust.color_scheme.is_some()
            || adjust.hard_reload
            || adjust.clear.is_some();
        let mut display = grant.display();
        if let Some(viewport) = viewport {
            display.viewport = viewport;
        }
        if let Some(zoom) = adjust.zoom_factor {
            display.zoom_factor = zoom;
        }
        if let Some(color) = adjust.color_scheme {
            display.color_scheme = color;
        }
        display.validate()?;
        let host = slot.attach(&grant).await?;
        let tab = &host.tabs[&grant.info.id];
        let conn = &mut host.browser.conn;
        let session = Some(tab.session.as_str());
        let (width, height) = display.viewport.pixels()?;
        let zoom = display.zoom_factor;
        conn.request(
            "Emulation.setDeviceMetricsOverride",
            json!({"width":(width as f64/zoom).round() as u32,
            "height":(height as f64/zoom).round() as u32,"deviceScaleFactor":zoom,"mobile":false}),
            session,
        )
        .await?;
        let features = match display.color_scheme {
            Color::System => json!([]),
            Color::Dark => json!([{"name":"prefers-color-scheme","value":"dark"}]),
            Color::Light => json!([{"name":"prefers-color-scheme","value":"light"}]),
        };
        conn.request(
            "Emulation.setEmulatedMedia",
            json!({"features":features}),
            session,
        )
        .await?;
        if let Some(clear) = adjust.clear {
            conn.request(
                match clear {
                    Clear::Cookies => "Network.clearBrowserCookies",
                    Clear::Cache => "Network.clearBrowserCache",
                },
                json!({}),
                session,
            )
            .await?;
        }
        if adjust.hard_reload {
            conn.request("Page.reload", json!({"ignoreCache":true}), session)
                .await?;
        }
        if adjusted {
            grant.set_display(
                display.clone(),
                json!({"hard_reload":adjust.hard_reload,"clear":adjust.clear}),
            );
        }
        let metrics = conn
            .request("Page.getLayoutMetrics", json!({}), session)
            .await?;
        let info = conn
            .request("Target.getTargetInfo", json!({"targetId":tab.target}), None)
            .await?;
        let history = conn
            .request("Page.getNavigationHistory", json!({}), session)
            .await?;
        let index = history["currentIndex"].as_u64().unwrap_or(0);
        grant.native_status(json!({"_tag":"Success","url":info["targetInfo"]["url"],"title":info["targetInfo"]["title"]}),
            index>0,history["entries"].as_array().is_some_and(|a|index+1<a.len() as u64));
        Ok(
            json!({"preview":grant.snapshot(),"title":info["targetInfo"]["title"],"rendered_viewport":metrics["cssLayoutViewport"]}),
        )
    }
    pub async fn screen(self: &Arc<Self>, grant: Arc<Grant>) -> Result<Vec<u8>> {
        // Attach and apply the recorded settings even when the profile has just been cleared.
        self.control(grant.clone(), None, Adjust::default()).await?;
        let handle = self.bind(&grant);
        let mut slot = handle.lock().await;
        let host = slot.attach(&grant).await?;
        let bytes = host
            .browser
            .conn
            .request(
                "Page.captureScreenshot",
                json!({"format":"png","fromSurface":true}),
                Some(&host.tabs[&grant.info.id].session),
            )
            .await?;
        STANDARD
            .decode(
                bytes["data"]
                    .as_str()
                    .ok_or_else(|| Error::Protocol("Chromium omitted the preview image".into()))?,
            )
            .map_err(|_| Error::Protocol("Chromium returned an invalid preview image".into()))
    }
}
impl Slot {
    async fn attach(&mut self, grant: &Arc<Grant>) -> Result<&mut Host> {
        if !grant.visible().await? {
            return Err(Error::Closed(
                "the preview was revoked or its listener stopped".into(),
            ));
        }
        if let Some(host) = &mut self.host
            && host.browser.process.inner().try_wait()?.is_some()
        {
            self.host = None;
        }
        if self.host.is_none() {
            let temporary = self.ephemeral.then(tempfile::tempdir).transpose()?;
            let path = temporary
                .as_ref()
                .map_or(self.dir.as_path(), |dir| dir.path());
            let browser = Browser::open(path).await?;
            self.host = Some(Host {
                browser,
                tabs: HashMap::new(),
                _temporary: temporary,
            });
        }
        let host = self.host.as_mut().unwrap();
        grant.mark_managed();
        host.browser.network.allow(grant);
        if let std::collections::hash_map::Entry::Vacant(entry) = host.tabs.entry(grant.info.id) {
            let conn = &mut host.browser.conn;
            let reply = conn
                .request("Target.createTarget", json!({"url":"about:blank"}), None)
                .await?;
            let target = reply["targetId"]
                .as_str()
                .ok_or_else(|| Error::Protocol("Chromium omitted its new tab".into()))?
                .to_owned();
            let open = async {
                let reply = conn
                    .request(
                        "Target.attachToTarget",
                        json!({"targetId":target,"flatten":true}),
                        None,
                    )
                    .await?;
                let session = reply["sessionId"]
                    .as_str()
                    .ok_or_else(|| Error::Protocol("Chromium omitted the page session".into()))?
                    .to_owned();
                conn.request("Page.enable", json!({}), Some(&session))
                    .await?;
                let reply = conn
                    .request(
                        "Page.navigate",
                        json!({"url":grant.info.url}),
                        Some(&session),
                    )
                    .await?;
                if reply["errorText"].as_str().is_some_and(|s| !s.is_empty()) {
                    return Err(Error::Closed(
                        "Chromium could not load the authorized preview".into(),
                    ));
                }
                Ok(session)
            }
            .await;
            match open {
                Ok(session) => {
                    entry.insert(Tab { target, session });
                }
                Err(e) => {
                    let _ = conn
                        .request("Target.closeTarget", json!({"targetId":target}), None)
                        .await;
                    return Err(e);
                }
            }
        }
        Ok(host)
    }
    async fn close(&mut self, id: Uuid) {
        if let Some(host) = &mut self.host {
            if let Some(tab) = host.tabs.remove(&id) {
                let _ = host
                    .browser
                    .conn
                    .request("Target.closeTarget", json!({"targetId":tab.target}), None)
                    .await;
            }
            if host.tabs.is_empty() {
                self.shutdown().await;
            }
        }
    }
    async fn shutdown(&mut self) {
        if let Some(mut host) = self.host.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                let _ = host
                    .browser
                    .conn
                    .request("Browser.close", json!({}), None)
                    .await;
                host.browser.process.wait().await
            })
            .await;
        }
    }
}
