//! Workbench dictation with a checksummed model catalog and host-owned audio services.
mod audio;
mod cloud;
mod engine;
mod local;
mod models;
mod routes;
mod vault;
use super::Workbench;
use crate::{Error, Result};
pub(super) use routes::routes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};
use tokio::sync::{Semaphore, watch};

#[derive(Deserialize)]
pub(super) struct Model {
    id: String,
    filename: String,
    revision: String,
    sha256: String,
    size: u64,
    #[serde(flatten)]
    details: serde_json::Map<String, Value>,
}
#[derive(Deserialize)]
struct Catalog {
    models: Vec<Model>,
}
static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("catalog.json")).expect("pinned speech catalog")
});
fn model(id: &str) -> Result<&'static Model> {
    CATALOG
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| Error::Refused("unknown speech model".into()))
}
#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    model: String,
    acceleration: String,
    #[serde(default)]
    links: BTreeMap<String, PathBuf>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            model: CATALOG.models[0].id.clone(),
            acceleration: "auto".into(),
            links: Default::default(),
        }
    }
}
pub(super) struct Service {
    engine: tokio::sync::OnceCell<engine::Devices>,
    cloud_models: tokio::sync::Mutex<Option<(std::time::Instant, Value)>>,
    dir: PathBuf,
    settings: Mutex<Settings>,
    active: Mutex<Option<Arc<models::Download>>>,
    operation: Arc<Semaphore>,
    updates: watch::Sender<u64>,
    verified: Mutex<BTreeMap<PathBuf, models::Verification>>,
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().expect("voice state parent");
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
impl Service {
    pub(super) fn open(state: &Path) -> Result<Self> {
        let dir = state.join("voice");
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.recursive(true).create(dir.join("models"))?;
        let settings = match fs::read(dir.join("settings.json")) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            engine: Default::default(),
            cloud_models: Default::default(),
            dir,
            settings: Mutex::new(settings),
            active: Default::default(),
            operation: Arc::new(Semaphore::new(1)),
            updates: watch::channel(0).0,
            verified: Default::default(),
        })
    }
    fn changed(&self) {
        self.updates.send_modify(|r| *r = r.wrapping_add(1));
    }
    fn update(&self, change: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut guard = self.settings.lock().unwrap();
        let mut next = guard.clone();
        change(&mut next);
        atomic(&self.dir.join("settings.json"), &serde_json::to_vec(&next)?)?;
        *guard = next;
        drop(guard);
        self.changed();
        Ok(())
    }
    fn config(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }
    fn exclusive(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        self.operation
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)
    }
    fn catalog(&self, bench: bool, paths: bool) -> Value {
        let settings = self.config();
        let active = self.active.lock().unwrap().clone();
        let rows = CATALOG
            .models
            .iter()
            .map(|m| {
                let mut row = Value::Object(m.details.clone());
                row.as_object_mut().unwrap().remove("license");
                if !bench {
                    row.as_object_mut().unwrap().remove("benchmark");
                }
                row["id"] = json!(m.id);
                row["size"] = json!(m.size);
                row["accuracy"] = json!(0);
                row["speed"] = json!(0);
                row["active"] = json!(settings.model == m.id);
                let path = self.path(m, &settings);
                if let Some(job) = active.as_ref().filter(|j| j.id == m.id) {
                    let (size, verify) = job.progress();
                    row["state"] = json!(if verify { "verifying" } else { "downloading" });
                    row["downloaded"] = json!(size);
                } else if self.ready(m, &path) {
                    row["state"] = json!("installed");
                    if paths && settings.links.contains_key(&m.id) {
                        row["linkedFrom"] = json!(path);
                    }
                } else {
                    row["state"] = json!("downloadable");
                }
                row
            })
            .collect::<Vec<_>>();
        json!({"models":rows})
    }
}
