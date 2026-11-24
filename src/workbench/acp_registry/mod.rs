//! Workbench-owned, versioned ACP installations. A saved chat resolves its pinned
//! executable from this store without npm, uv or registry network access.
mod archive;
mod catalog;
mod packages;
mod profiles;
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
pub(crate) fn dynamic(id: &str) -> bool {
    id.starts_with("acp:") || id.starts_with("acp-profile:")
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
        self.path_in(&root(store).join("instances"))
    }
    fn path_in(&self, base: &Path) -> PathBuf {
        base.join(self.agent).join(self.version).join(self.kind)
    }
}
fn lock(store: &ChatStore, id: &str, exclusive: bool) -> Result<File> {
    lock_path(&root(store).join("locks").join(id), exclusive)
}
fn lock_path(path: &Path, exclusive: bool) -> Result<File> {
    fs::create_dir_all(path.parent().unwrap())?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let locked = if exclusive {
        f.try_lock()
    } else {
        f.try_lock_shared()
    };
    locked.map_err(lock_error)?;
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
    if runtime.starts_with("acp-profile:") {
        return profiles::resolve(store, runtime, opts);
    }
    let k = Key::parse(runtime)?;
    let r = Receipt::load(&k, store, runtime)?;
    let program = r.program(&k.dir(store))?;
    configure(&r.settings, opts);
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
fn configure(launch: &Launch, opts: &mut SessionOptions) {
    for (key, value) in &launch.env {
        if opts.unset_env.iter().any(|n| n.eq_ignore_ascii_case(key))
            || key.eq_ignore_ascii_case("KAR_WORKBENCH_TOKEN")
        {
            continue;
        }
        opts.env
            .entry(key.as_str().into())
            .or_insert(value.as_str().into());
    }
}
pub(super) fn owner_routes() -> Router<Workbench> {
    profiles::owner_routes()
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/acp-registry/search", get(search))
        .route("/acp-registry/prepare", post(prepare))
        .route("/acp-registry/instances", get(instances))
        .route(
            "/acp-registry/instances/{runtime}/reinstall",
            post(reinstall),
        )
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
    let release = latest(
        app,
        &input.agent_id,
        input.distribution.as_deref().unwrap_or("auto"),
    )
    .await?;
    install(app, &release.runtime, Some(&release)).await
}
#[derive(Clone, Serialize, Deserialize)]
struct Release {
    runtime: String,
    name: String,
    registry_url: String,
    distribution: Selected,
}
async fn latest(app: &Workbench, agent: &str, kind: &str) -> Result<Release> {
    let (source, agents) = app.0.acp_registry.agents(true).await?;
    let a = agents
        .into_iter()
        .find(|a| a.id == agent)
        .ok_or_else(|| bad("unknown ACP registry agent"))?;
    let distribution = a
        .select(kind)
        .ok_or_else(|| bad("no compatible ACP distribution for this platform"))?;
    Ok(Release {
        runtime: format!("acp:{}:{}:{}", a.id, a.version, distribution.kind()),
        name: a.name,
        registry_url: source.into(),
        distribution,
    })
}
fn record<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut tmp, value)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
async fn reinstall(State(app): State<Workbench>, Param(runtime): Param<String>) -> Reply {
    Ok(Json(
        tokio::spawn(async move {
            let key = Key::parse(&runtime)?;
            let pinned = key.path_in(&root(&app.0.chats).join("releases"));
            if pinned.try_exists()? {
                return install(&app, &runtime, None).await;
            }
            // Earlier Kar receipts recorded the launch but did not retain its distribution.
            let release = latest(&app, key.agent, key.kind).await?;
            if release.runtime != runtime {
                return Err(bad(
                    "exact ACP release is absent from the saved metadata and current registry",
                ));
            }
            install(&app, &runtime, Some(&release)).await
        })
        .await
        .map_err(|e| bad(&e.to_string()))??,
    ))
}
async fn install(app: &Workbench, runtime: &str, current: Option<&Release>) -> Result<Value> {
    let key = Key::parse(runtime)?;
    let store = &app.0.chats;
    let _lock = lock(store, key.agent, true)?;
    let pinned = key.path_in(&root(store).join("releases"));
    let release_exists = pinned.try_exists()?;
    let mut release: Release = if release_exists {
        serde_json::from_reader(File::open(&pinned)?)?
    } else {
        let Some(release) = current else {
            return Err(bad("missing saved ACP release"));
        };
        release.clone()
    };
    if release.runtime != runtime || release.distribution.kind() != key.kind {
        return Err(bad("ACP release metadata identifies another instance"));
    }
    let dir = key.dir(store);
    if dir.join("launch.json").try_exists()? {
        let r = Receipt::load(&key, store, runtime)?;
        r.program(&dir)?;
        if !release_exists {
            record(&pinned, &release)?;
        }
        return prepare_receipt(&r, &key, &dir);
    }
    fs::create_dir_all(&dir)?;
    let settings = release.distribution.launch().clone();
    let program = match &mut release.distribution {
        Selected::Binary(b) => {
            let source = url::Url::parse(&release.registry_url)
                .map_err(|_| bad("invalid saved ACP registry URL"))?;
            let payload = dir.join("payload");
            if payload.try_exists()? {
                fs::remove_dir_all(&payload)?;
            }
            let (program, checksum) = archive::install(&dir, &source, b).await?;
            b.sha256 = Some(checksum);
            program
        }
        Selected::Npx(p) | Selected::Uvx(p) => {
            packages::install(&dir, key.agent, key.kind, &p.package).await?
        }
    };
    let receipt = Receipt {
        runtime: runtime.to_owned(),
        name: release.name.clone(),
        program,
        settings,
    };
    receipt.program(&dir)?;
    record(&pinned, &release)?;
    record(&dir.join("launch.json"), &receipt)?;
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
    super::http::blocking(move || Ok(json!({"instances":prepared(&app.0.chats)?,"profiles":profiles::instances(&app.0.chats)?}))).await
}
// A lasting chat identity never owns a binary. The file lock follows the native
// process, including sessions opened from a second Kar process using this store.
pub(crate) fn pin_process(runtime: &Runtime) -> Result<Option<File>> {
    if !dynamic(&runtime.id) {
        return Ok(None);
    }
    let Some(registry_root) = runtime
        .program
        .ancestors()
        .find(|p| p.ends_with("workbench/acp-registry"))
    else {
        return Ok(None);
    };
    let Ok(relative) = runtime
        .program
        .strip_prefix(registry_root.join("instances"))
    else {
        return Ok(None);
    };
    let mut parts = relative.iter().filter_map(|s| s.to_str());
    let (Some(agent), Some(version), Some(kind), Some("payload")) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("invalid managed ACP executable path"));
    };
    let managed = format!("acp:{agent}:{version}:{kind}");
    let key = Key::parse(&managed)?;
    if runtime.id.starts_with("acp:") && managed != runtime.id {
        return Err(bad("prepared executable differs from selected runtime"));
    }
    let guard = lock_path(&key.path_in(&registry_root.join("usage-locks")), false)?;
    let dir = key.path_in(&registry_root.join("instances"));
    let receipt: Receipt = serde_json::from_reader(File::open(dir.join("launch.json"))?)?;
    if receipt.runtime != managed || receipt.program(&dir)? != runtime.program {
        return Err(bad("ACP launch receipt differs from the selected instance"));
    }
    // Resolving the prepared release consumes its short install-to-chat reservation.
    match fs::remove_file(dir.join("prepared")) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Some(guard))
}
fn lock_error(error: std::fs::TryLockError) -> Error {
    match error {
        std::fs::TryLockError::WouldBlock => Error::Busy,
        std::fs::TryLockError::Error(e) => e.into(),
    }
}
fn remove(store: &ChatStore, key: &Key) -> Result<bool> {
    let dir = key.dir(store);
    if !dir.try_exists()? {
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
        let _guard = match lock_path(&key.path_in(&root(&app.0.chats).join("usage-locks")), true) {
            Err(Error::Busy) => return Ok(json!({"runtime":runtime,"removed":false})),
            other => other?,
        };
        Ok(json!({"runtime":runtime,"removed":remove(&app.0.chats,&key)?}))
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
                if reserved(&version.join("binary"))? {
                    return Ok(json!({"agent_id":agent,"removed":false}));
                }
                let key = Key::parse(&id)?;
                match lock_path(&key.path_in(&root(store).join("usage-locks")), true) {
                    Err(Error::Busy) => return Ok(json!({"agent_id":agent,"removed":false})),
                    other => handles.push((id, other?)),
                }
            }
        }
        let mut removed = false;
        for (id, _guard) in &handles {
            removed |= remove(store, &Key::parse(id)?)?;
        }
        Ok(json!({"agent_id":agent,"removed":removed}))
    })
    .await
}
