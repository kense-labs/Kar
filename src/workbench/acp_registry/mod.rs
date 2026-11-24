//! Workbench-owned, versioned ACP installations. A saved chat resolves its pinned
//! executable from this store without npm, uv or registry network access.
mod archive;
mod catalog;
mod packages;
use super::{Workbench, http::ApiError};
use crate::{Error, Protocol, Result, Runtime, SessionOptions, chat::ChatStore};
use axum::{
    Json, Router,
    extract::{Path as Param, Query, State},
    routing::{delete, get, post},
};
use catalog::{Launch, Selected};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

pub(super) use catalog::Catalog;
fn bad(detail: &str) -> Error {
    Error::Refused(detail.to_string())
}
fn root(store: &ChatStore) -> PathBuf {
    store.directory().join("workbench/acp-registry")
}
fn kind_ok(k: &str) -> bool {
    matches!(k, "npx" | "uvx" | "binary")
}
struct Key<'a> {
    agent: &'a str,
    version: &'a str,
    kind: &'a str,
}
impl<'a> Key<'a> {
    fn parse(runtime: &'a str) -> Result<Self> {
        let mut parts = runtime.split(':');
        let (Some("acp"), Some(agent), Some(version), Some(kind), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(bad("invalid registry runtime ID"));
        };
        if !catalog::id_ok(agent) || !catalog::version_ok(version) || !kind_ok(kind) {
            return Err(bad("invalid registry runtime ID"));
        }
        Ok(Self {
            agent,
            version,
            kind,
        })
    }
    fn dir(&self, store: &ChatStore) -> PathBuf {
        root(store)
            .join("instances")
            .join(self.agent)
            .join(self.version)
            .join(self.kind)
    }
}
fn lock(store: &ChatStore, id: &str, exclusive: bool) -> Result<File> {
    let dir = root(store).join("locks");
    fs::create_dir_all(&dir)?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(id))?;
    let locked = if exclusive {
        f.try_lock()
    } else {
        f.try_lock_shared()
    };
    locked.map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => Error::Busy,
        std::fs::TryLockError::Error(e) => e.into(),
    })?;
    Ok(f)
}
pub(crate) fn pin(store: &ChatStore, runtime: &str) -> Result<Option<File>> {
    if !runtime.starts_with("acp:") {
        return Ok(None);
    }
    let key = Key::parse(runtime)?;
    Ok(Some(lock(store, key.agent, false)?))
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    runtime: String,
    name: String,
    program: PathBuf,
    settings: Launch,
}
impl Receipt {
    fn load(key: &Key, store: &ChatStore, runtime: &str) -> Result<Self> {
        let receipt: Self =
            serde_json::from_reader(File::open(key.dir(store).join("launch.json"))?)?;
        if receipt.runtime != runtime {
            return Err(bad("ACP install receipt identifies another runtime"));
        }
        Ok(receipt)
    }
    fn program(&self, dir: &Path) -> Result<PathBuf> {
        let program = dir.join(&self.program);
        let real = program.canonicalize()?;
        if !real.starts_with(dir.join("payload").canonicalize()?) || !real.is_file() {
            return Err(bad("prepared ACP executable is outside its installation"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if real.metadata()?.permissions().mode() & 0o111 == 0 {
                return Err(bad("prepared ACP command is not executable"));
            }
        }
        Ok(program)
    }
    fn summary(&self, key: &Key) -> Value {
        json!({"agent_id":key.agent,"version":key.version,"distribution":key.kind,"runtime":self.runtime,"name":self.name,"prepared":true})
    }
}
pub(crate) fn resolve(
    store: &ChatStore,
    runtime: &str,
    opts: &mut SessionOptions,
) -> Result<Runtime> {
    let k = Key::parse(runtime)?;
    let r = Receipt::load(&k, store, runtime)?;
    let program = r.program(&k.dir(store))?;
    for (key, value) in r.settings.env {
        if opts.unset_env.iter().any(|n| n.eq_ignore_ascii_case(&key))
            || key.eq_ignore_ascii_case("KAR_WORKBENCH_TOKEN")
        {
            continue;
        }
        opts.env.entry(key.into()).or_insert(value.into());
    }
    if k.kind != "binary"
        && let Some(path) = opts.env_value("PATH")
    {
        let bin = program.parent().unwrap().to_path_buf();
        let entries =
            std::iter::once(bin.clone()).chain(std::env::split_paths(&path).filter(|p| p != &bin));
        opts.env.insert(
            "PATH".into(),
            std::env::join_paths(entries).map_err(|_| bad("invalid ACP executable search path"))?,
        );
    }
    let args = r
        .settings
        .args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    Ok(Runtime::new(runtime, program, &args, Protocol::Acp))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/acp-registry/search", get(search))
        .route("/acp-registry/prepare", post(prepare))
        .route("/acp-registry/instances", get(instances))
        .route("/acp-registry/instances/{runtime}", delete(uninstall))
        .route(
            "/acp-registry/agents/{agent}/managed-binary",
            delete(uninstall_binary),
        )
}
type Reply = std::result::Result<Json<Value>, ApiError>;
#[derive(Default, Deserialize)]
struct Search {
    #[serde(default)]
    query: String,
    limit: Option<usize>,
}
async fn search(State(app): State<Workbench>, Query(q): Query<Search>) -> Reply {
    let (_, agents) = app.0.acp_registry.agents(false).await?;
    let mut ranked = Vec::new();
    for a in agents {
        let (Some(rank), Some(selected)) = (a.rank(&q.query), a.select("auto")) else {
            continue;
        };
        let kind = selected.kind();
        if kind != "binary" && packages::manager(kind).is_none() {
            continue;
        }
        let integrity = if matches!(selected,Selected::Binary(b) if b.sha256.is_some()) {
            "sha256"
        } else {
            "registry"
        };
        ranked.push(((rank,a.name.to_lowercase(),a.id.clone()),json!({"id":a.id,"name":a.name,"version":a.version,"description":a.description,"authors":a.authors,"license":a.license,"website":a.website,"repository":a.repository,"icon":a.icon,"distribution":kind,"integrity":integrity})));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0));
    let limit = q.limit.unwrap_or(20).min(512);
    Ok(Json(
        json!({"agents":ranked.into_iter().take(limit).map(|(_,a)|a).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
struct Prepare {
    agent_id: String,
    distribution: Option<String>,
}
async fn prepare(State(app): State<Workbench>, Json(input): Json<Prepare>) -> Reply {
    // Finish an admitted installation even if the HTTP client loses its receipt.
    Ok(Json(
        tokio::spawn(async move { prepare_instance(&app, input).await })
            .await
            .map_err(|e| bad(&e.to_string()))??,
    ))
}
async fn prepare_instance(app: &Workbench, input: Prepare) -> Result<Value> {
    let (source, agents) = app.0.acp_registry.agents(true).await?;
    let a = agents
        .into_iter()
        .find(|a| a.id == input.agent_id)
        .ok_or_else(|| bad("unknown ACP registry agent"))?;
    let kind = input.distribution.as_deref().unwrap_or("auto");
    let selected = a
        .select(kind)
        .ok_or_else(|| bad("no compatible ACP distribution for this platform"))?;
    let kind = selected.kind();
    let runtime = format!("acp:{}:{}:{kind}", a.id, a.version);
    let key = Key::parse(&runtime)?;
    let store = &app.0.chats;
    let _lock = lock(store, &a.id, true)?;
    let dir = key.dir(store);
    if dir.join("launch.json").try_exists()? {
        let r = Receipt::load(&key, store, &runtime)?;
        r.program(&dir)?;
        return prepare_receipt(&r, &key, &dir);
    }
    fs::create_dir_all(&dir)?;
    let settings = selected.launch().clone();
    let program = match selected {
        Selected::Binary(b) => {
            let payload = dir.join("payload");
            if payload.try_exists()? {
                fs::remove_dir_all(&payload)?;
            }
            archive::install(&dir, &source, b).await?
        }
        Selected::Npx(p) | Selected::Uvx(p) => {
            packages::install(&dir, &a.id, kind, &p.package).await?
        }
    };
    let receipt = Receipt {
        runtime: runtime.clone(),
        name: a.name,
        program,
        settings,
    };
    receipt.program(&dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(&mut tmp, &receipt)?;
    tmp.as_file().sync_all()?;
    tmp.persist(dir.join("launch.json")).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(&dir)?.sync_all()?;
    prepare_receipt(&receipt, &key, &dir)
}
fn prepare_receipt(receipt: &Receipt, key: &Key, dir: &Path) -> Result<Value> {
    if key.kind == "binary" {
        // A cleanup from a second client must not race the first client's chat creation.
        fs::write(dir.join("prepared"), [])?;
    }
    Ok(receipt.summary(key))
}
fn reserved(dir: &Path) -> Result<bool> {
    match fs::metadata(dir.join("prepared")) {
        Ok(file) => {
            Ok(file.modified()?.elapsed().unwrap_or_default() < std::time::Duration::from_secs(30))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn subdirs(path: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut dirs = Vec::new();
    for e in entries {
        let e = e?;
        if e.file_type()?.is_dir() {
            dirs.push(e.path())
        }
    }
    dirs.sort();
    Ok(dirs)
}
fn prepared(store: &ChatStore) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for agent in subdirs(&root(store).join("instances"))? {
        for version in subdirs(&agent)? {
            for dir in subdirs(&version)? {
                let path = dir.join("launch.json");
                if !path.try_exists()? {
                    continue;
                }
                let receipt: Receipt = serde_json::from_reader(File::open(path)?)?;
                let key = Key::parse(&receipt.runtime)?;
                if key.dir(store) != dir {
                    return Err(bad("misplaced ACP install receipt"));
                }
                out.push(receipt.summary(&key));
            }
        }
    }
    Ok(out)
}
async fn instances(State(app): State<Workbench>) -> Reply {
    super::http::blocking(move || Ok(json!({"instances":prepared(&app.0.chats)?}))).await
}
fn in_use(store: &ChatStore, runtime: &str) -> Result<bool> {
    for chat in store.list()? {
        let mut used = false;
        store.visit_native_bindings(chat.id, |info| {
            used |= info.runtime == runtime;
        })?;
        if used {
            return Ok(true);
        }
        let h = store.history(chat.id)?;
        if h.handoffs.iter().any(|h| {
            h.status == crate::chat::HandoffStatus::Prepared && h.request.runtime == runtime
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn remove(store: &ChatStore, key: &Key, runtime: &str) -> Result<bool> {
    let dir = key.dir(store);
    if !dir.try_exists()? || in_use(store, runtime)? {
        return Ok(false);
    }
    // A crash after the rename leaves the public handle fully uninstalled.
    let trash = tempfile::tempdir_in(root(store))?;
    fs::rename(dir, trash.path().join("installation"))?;
    trash.close()?;
    Ok(true)
}
async fn uninstall(State(app): State<Workbench>, Param(runtime): Param<String>) -> Reply {
    super::http::blocking(move || {
        let key = Key::parse(&runtime)?;
        let _lock = lock(&app.0.chats, key.agent, true)?;
        Ok(json!({"runtime":runtime,"removed":remove(&app.0.chats,&key,&runtime)?}))
    })
    .await
}
async fn uninstall_binary(State(app): State<Workbench>, Param(agent): Param<String>) -> Reply {
    super::http::blocking(move || {
        if !catalog::id_ok(&agent) {
            return Err(bad("invalid ACP agent ID"));
        }
        let store = &app.0.chats;
        let _lock = lock(store, &agent, true)?;
        let mut handles = vec![];
        for version in subdirs(&root(store).join("instances").join(&agent))? {
            if version.join("binary").is_dir() {
                let v = version.file_name().unwrap().to_string_lossy();
                let id = format!("acp:{agent}:{v}:binary");
                if reserved(&version.join("binary"))? || in_use(store, &id)? {
                    return Ok(json!({"agent_id":agent,"removed":false}));
                }
                handles.push(id);
            }
        }
        let mut removed = false;
        for id in handles {
            removed |= remove(store, &Key::parse(&id)?, &id)?;
        }
        Ok(json!({"agent_id":agent,"removed":removed}))
    })
    .await
}
