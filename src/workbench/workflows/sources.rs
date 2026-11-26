//! Named Kar workflow sources. A rejected edit leaves the last runnable definitions in place.
use super::{Data, Definition, Status, Workbench, install_definition, views::erase_workflow};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::{Path as Param, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

const LIMIT: usize = 1024 * 1024;
const SUFFIX: &str = ".kar.json";
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct Saved {
    good: Option<String>,
    observed: Option<String>,
    tags: BTreeMap<Uuid, (String, u32)>,
    problem: Option<String>,
}
#[derive(Deserialize)]
struct Source {
    workflows: Vec<Named>,
}
#[derive(Deserialize)]
struct Named {
    id: Uuid,
    version: u32,
    #[serde(flatten)]
    definition: Definition,
}

fn validate_name(name: &str) -> Result<()> {
    if name.len() > 128
        || !name.strip_suffix(SUFFIX).is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
    {
        Err(Error::Refused(
            "expected a name such as review.kar.json".into(),
        ))
    } else {
        Ok(())
    }
}
fn fingerprint(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn directory(app: &Workbench, project: Uuid) -> PathBuf {
    app.0
        .state
        .join("workflow-definitions")
        .join(project.to_string())
}
fn read(path: &Path) -> Result<Option<String>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(Error::Refused("expected a regular definition file".into()));
    }
    let mut text = String::new();
    file.take(LIMIT as u64 + 1).read_to_string(&mut text)?;
    if text.len() > LIMIT {
        return Err(Error::Refused("definition source exceeds 1 MiB".into()));
    }
    Ok(Some(text))
}
fn write(path: &Path, source: Option<&str>) -> Result<()> {
    let dir = path.parent().expect("project definition directory");
    if let Some(text) = source {
        fs::create_dir_all(dir)?;
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        f.write_all(text.as_bytes())?;
        f.as_file().sync_all()?;
        f.persist(path).map_err(|e| e.error)?;
    } else {
        match fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        }
    }
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}
pub(super) fn allow_direct(db: &Data, id: Uuid) -> Result<()> {
    if db
        .sources
        .values()
        .flat_map(|s| s.values())
        .any(|s| s.tags.contains_key(&id))
    {
        return Err(Error::Refused(
            "edit this workflow through its definition source".into(),
        ));
    }
    Ok(())
}
fn apply(db: &mut Data, p: Uuid, name: &str, source: Option<&str>, now: i64) -> Result<()> {
    let old = db
        .sources
        .get(&p)
        .and_then(|s| s.get(name))
        .cloned()
        .unwrap_or_default();
    let mut tags = BTreeMap::new();
    let mut keys = BTreeSet::new();
    if let Some(text) = source {
        if text.len() > LIMIT {
            return Err(Error::Refused("definition source exceeds 1 MiB".into()));
        }
        let defs: Source = serde_json::from_str(text).map_err(|e| Error::Refused(e.to_string()))?;
        for w in defs.workflows {
            let tag = (w.definition.name.clone(), w.version);
            if w.version == 0
                || !keys.insert(tag.clone())
                || tags.insert(w.id, tag.clone()).is_some()
            {
                return Err(Error::Refused(
                    "workflow IDs and versioned names must be unique; versions begin at one".into(),
                ));
            }
            if !old.tags.contains_key(&w.id) && db.workflows.contains_key(&w.id) {
                return Err(Error::Refused(
                    "workflow ID belongs to another definition".into(),
                ));
            }
            if db
                .sources
                .get(&p)
                .into_iter()
                .flatten()
                .any(|(file, s)| file != name && s.tags.values().any(|t| t == &tag))
            {
                return Err(Error::Refused(
                    "workflow name and version exist in another source".into(),
                ));
            }
            install_definition(db, p, w.id, w.definition, None, now)?;
        }
    }
    for (id, tag) in old.tags {
        if tags.get(&id) == Some(&tag) {
            continue;
        }
        if db
            .runs
            .values()
            .any(|r| r.workflow == id && !r.status.is_terminal() && r.status != Status::Failed)
        {
            return Err(Error::Refused(format!(
                "unfinished runs still use {} version {}",
                tag.0, tag.1
            )));
        }
        if !tags.contains_key(&id) {
            erase_workflow(db, id);
        }
    }
    let files = db.sources.entry(p).or_default();
    if let Some(s) = source {
        let hash = Some(fingerprint(s));
        files.insert(
            name.into(),
            Saved {
                good: hash.clone(),
                observed: hash,
                tags,
                problem: None,
            },
        );
    } else {
        files.remove(name);
    }
    Ok(())
}

impl Workbench {
    fn source_names(&self, p: Uuid) -> Result<BTreeSet<String>> {
        let mut names: BTreeSet<_> = self
            .0
            .workflows
            .db
            .lock()
            .unwrap()
            .sources
            .get(&p)
            .into_iter()
            .flat_map(|s| s.keys().cloned())
            .collect();
        match fs::read_dir(directory(self, p)) {
            Ok(entries) => {
                for entry in entries {
                    let name = entry?.file_name();
                    if let Some(name) = name
                        .to_str()
                        .filter(|n| !n.starts_with('.') && n.ends_with(".json"))
                    {
                        names.insert(name.into());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(names)
    }
    fn source_error(
        &self,
        p: Uuid,
        name: &str,
        observed: Option<String>,
        reason: String,
    ) -> Result<()> {
        let engine = &self.0.workflows;
        if engine
            .db
            .lock()
            .unwrap()
            .sources
            .get(&p)
            .and_then(|m| m.get(name))
            .is_some_and(|s| {
                s.observed == observed && s.problem.as_deref() == Some(reason.as_str())
            })
        {
            return Ok(());
        }
        engine.update(|db| {
            let saved = db
                .sources
                .entry(p)
                .or_default()
                .entry(name.into())
                .or_default();
            saved.observed = observed;
            saved.problem = Some(reason);
            Ok(())
        })
    }
    // Caller holds source_lock for the enumeration, validation and publication pass.
    fn refresh_sources(&self, p: Uuid) -> Result<Vec<Value>> {
        let engine = &self.0.workflows;
        let mut rows = vec![];
        for name in self.source_names(p)? {
            let path = directory(self, p).join(&name);
            let seen = read(&path);
            let text = seen.as_ref().ok().and_then(|v| v.as_deref());
            let hash = text.map(fingerprint);
            let cached = engine
                .db
                .lock()
                .unwrap()
                .sources
                .get(&p)
                .and_then(|s| s.get(&name))
                .cloned();
            let unchanged = cached
                .as_ref()
                .is_some_and(|s| s.good == hash && s.problem.is_none());
            let result = match &seen {
                Err(e) => Err(Error::Refused(e.to_string())),
                Ok(None) if cached.is_none() => Ok(()),
                Ok(_) if unchanged => Ok(()),
                Ok(value) => {
                    // An invalid filename may have been removed externally as well.
                    let valid = if value.is_some() {
                        validate_name(&name)
                    } else {
                        Ok(())
                    };
                    valid.and_then(|()| {
                        engine.update(|d| apply(d, p, &name, text, engine.clock.now_ms()))
                    })
                }
            };
            if let Err(e) = result {
                self.source_error(p, &name, hash, e.to_string())?;
            }
            if let Some(saved) = engine
                .db
                .lock()
                .unwrap()
                .sources
                .get(&p)
                .and_then(|s| s.get(&name))
            {
                rows.push(json!({"name":name,"source":text,"workflows":saved.tags,"problem":saved.problem}));
            }
        }
        Ok(rows)
    }
    fn edit_source(&self, p: Uuid, name: &str, source: Option<&str>) -> Result<Value> {
        self.project(p)?;
        if let Err(e) = validate_name(name) {
            return Ok(json!({"changed":false,"problems":[e.to_string()]}));
        }
        let engine = &self.0.workflows;
        let _lock = engine.source_lock.lock().unwrap();
        self.refresh_sources(p)?;
        let path = directory(self, p).join(name);
        if read(&path).ok().is_some_and(|old| old.as_deref() == source)
            && engine
                .db
                .lock()
                .unwrap()
                .sources
                .get(&p)
                .and_then(|s| s.get(name))
                .is_none_or(|s| s.problem.is_none())
        {
            return Ok(json!({"changed":false,"problems":[]}));
        }
        let result = engine.update(|d| {
            apply(d, p, name, source, engine.clock.now_ms())?;
            write(&path, source)
        });
        match result {
            Ok(()) => Ok(json!({"changed":true,"problems":[]})),
            Err(Error::Refused(s)) => Ok(json!({"changed":false,"problems":[s]})),
            Err(e) => Err(e),
        }
    }
    pub(super) fn watch_sources(&self) {
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            loop {
                let Some(app) = weak.upgrade().map(Workbench) else {
                    break;
                };
                let scanned = tokio::task::spawn_blocking(move || -> Result<()> {
                    let _lock = app.0.workflows.source_lock.lock().unwrap();
                    for p in app.projects() {
                        app.refresh_sources(p.id)?;
                    }
                    Ok(())
                })
                .await;
                if let Ok(Err(e)) = scanned {
                    eprintln!("Kar workflow sources: {e}");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
}
type Reply = std::result::Result<Json<Value>, super::super::http::ApiError>;
async fn list(State(app): State<Workbench>, Param(p): Param<Uuid>) -> Reply {
    super::super::http::blocking(move || {
        app.project(p)?;
        let _lock = app.0.workflows.source_lock.lock().unwrap();
        let files = app.refresh_sources(p)?;
        let revision = app.0.workflows.db.lock().unwrap().revision;
        Ok(json!({"directory":directory(&app,p),"revision":revision,"files":files}))
    })
    .await
}
#[derive(Deserialize)]
struct Edit {
    source: String,
}
async fn save(
    State(app): State<Workbench>,
    Param((p, name)): Param<(Uuid, String)>,
    Json(req): Json<Edit>,
) -> Reply {
    super::super::http::blocking(move || app.edit_source(p, &name, Some(&req.source))).await
}
async fn remove(State(app): State<Workbench>, Param((p, name)): Param<(Uuid, String)>) -> Reply {
    super::super::http::blocking(move || app.edit_source(p, &name, None)).await
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{project}/workflow-definitions", get(list))
        .route(
            "/projects/{project}/workflow-definitions/{name}",
            axum::routing::put(save).delete(remove),
        )
}
