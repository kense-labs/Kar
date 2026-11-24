//! A local workbench that shares Kar's saved chats, their native sessions and event cursors.
mod files;
mod http;
mod search;
pub use search::{Hit, SearchKind};

use crate::{
    Error, Result, SessionOptions,
    chat::{ChatInfo, ChatSession, ChatStore, ChatTurn},
};
use cap_std::{ambient_authority, fs::Dir};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub root: PathBuf,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Catalog {
    projects: BTreeMap<Uuid, Project>,
    archived: BTreeSet<Uuid>,
}

struct Root {
    path: PathBuf,
    dir: Dir,
}

type Controller = Arc<AsyncMutex<Option<ChatSession>>>;

struct Inner {
    chats: ChatStore,
    roots: Vec<Root>,
    catalog: Mutex<Catalog>,
    state: PathBuf,
    _lease: File,
    controllers: Mutex<HashMap<Uuid, Controller>>,
}

#[derive(Clone)]
pub struct Workbench(Arc<Inner>);

fn missing() -> Error {
    io::Error::new(io::ErrorKind::NotFound, "project or chat not found").into()
}

fn denied() -> Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "outside the authorized workspace",
    )
    .into()
}

impl Workbench {
    /// Root authorization comes from the host, independently of registration over HTTP.
    pub fn open(chats: ChatStore, roots: &[PathBuf]) -> Result<Self> {
        let state = chats.directory().join("workbench");
        std::fs::create_dir_all(&state)?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join("service.lock"))?;
        lease.try_lock().map_err(io::Error::from)?;
        let mut allowed = Vec::new();
        for root in roots {
            let path = root.canonicalize()?;
            if !path.is_dir() || path.starts_with(chats.directory()) {
                return Err(denied());
            }
            let dir = Dir::open_ambient_dir(&path, ambient_authority())?;
            allowed.push(Root { path, dir });
        }
        if allowed.is_empty() {
            return Err(Error::Refused(
                "specify at least one authorized workbench root".into(),
            ));
        }
        let catalog = match File::open(state.join("projects.json")) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Catalog::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self(Arc::new(Inner {
            chats,
            roots: allowed,
            catalog: Mutex::new(catalog),
            state,
            _lease: lease,
            controllers: Default::default(),
        })))
    }

    fn allowed(&self, path: &Path) -> Result<&Root> {
        if path.starts_with(self.0.chats.directory()) {
            return Err(denied());
        }
        self.0
            .roots
            .iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.components().count())
            .ok_or_else(denied)
    }

    fn project(&self, id: Uuid) -> Result<Project> {
        let p = self
            .0
            .catalog
            .lock()
            .unwrap()
            .projects
            .get(&id)
            .cloned()
            .ok_or_else(missing)?;
        self.allowed(&p.root)?;
        Ok(p)
    }

    pub fn projects(&self) -> Vec<Project> {
        self.0
            .catalog
            .lock()
            .unwrap()
            .projects
            .values()
            .filter(|p| self.allowed(&p.root).is_ok())
            .cloned()
            .collect()
    }

    fn update<T>(&self, f: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        let mut saved = self.0.catalog.lock().unwrap();
        let mut next = saved.clone();
        let result = f(&mut next)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.0.state)?;
        serde_json::to_writer(&mut tmp, &next)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.0.state.join("projects.json"))
            .map_err(|e| e.error)?;
        *saved = next;
        #[cfg(unix)]
        File::open(&self.0.state)?.sync_all()?;
        Ok(result)
    }

    pub fn register(&self, root: &Path, name: &str) -> Result<Project> {
        let root = root.canonicalize()?;
        self.allowed(&root)?;
        if !root.is_dir() || name.trim().is_empty() {
            return Err(Error::Refused(
                "a project needs a directory and a name".into(),
            ));
        }
        self.update(|catalog| {
            if let Some(project) = catalog.projects.values().find(|p| p.root == root) {
                return Ok(project.clone());
            }
            let project = Project {
                id: Uuid::new_v4(),
                root,
                name: name.trim().to_owned(),
            };
            catalog.projects.insert(project.id, project.clone());
            Ok(project)
        })
    }

    fn binding(&self, project: Uuid, chat: Uuid) -> Result<ChatInfo> {
        let project = self.project(project)?;
        let info = self.0.chats.saved_info(chat)?;
        if info.workspace != project.root {
            return Err(missing());
        }
        Ok(info)
    }

    fn controller(&self, id: Uuid) -> Controller {
        self.0
            .controllers
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .clone()
    }

    fn archived(&self, chat: Uuid) -> bool {
        self.0.catalog.lock().unwrap().archived.contains(&chat)
    }

    pub fn chats(&self, project: Uuid, include_archived: bool) -> Result<Vec<ProjectChat>> {
        let root = self.project(project)?.root;
        let infos = self.0.chats.list()?;
        let catalog = self.0.catalog.lock().unwrap();
        Ok(infos
            .into_iter()
            .filter(|c| c.workspace == root)
            .filter_map(|info| {
                let archived = catalog.archived.contains(&info.id);
                (include_archived || !archived).then_some(ProjectChat {
                    project,
                    info,
                    archived,
                })
            })
            .collect())
    }

    pub async fn create_chat(
        &self,
        project: Uuid,
        id: Uuid,
        runtime: &str,
        model: Option<String>,
    ) -> Result<ProjectChat> {
        let p = self.project(project)?;
        self.workspace(&p)?;
        let slot = self.controller(id);
        let mut owner = slot.lock().await;
        match self.0.chats.saved_info(id) {
            Ok(info)
                if info.workspace == p.root && info.runtime == runtime && info.model == model =>
            {
                return Ok(ProjectChat {
                    project,
                    info,
                    archived: self.archived(id),
                });
            }
            Ok(_) => {
                return Err(Error::Refused(
                    "chat ID belongs to a different creation request".into(),
                ));
            }
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut opts = SessionOptions::new(p.root);
        opts.model = model;
        let session = self.0.chats.create_with_id(id, runtime, None, opts).await?;
        let info = session.info().clone();
        *owner = Some(session);
        Ok(ProjectChat {
            project,
            info,
            archived: false,
        })
    }

    pub async fn send(
        &self,
        project: Uuid,
        chat: Uuid,
        input: Uuid,
        text: String,
    ) -> Result<ChatTurn> {
        self.binding(project, chat)?;
        let app = self.clone();
        tokio::spawn(async move {
            let controller = app.controller(chat);
            let mut owner = controller.lock().await;
            if app.archived(chat) {
                return Err(Error::Refused("chat is archived".into()));
            }
            if owner.is_none() {
                *owner = Some(app.0.chats.resume(chat).await?);
            }
            owner
                .as_ref()
                .expect("open session")
                .send(input, text)
                .await
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }

    pub async fn set_archived(
        &self,
        project: Uuid,
        chat: Uuid,
        archived: bool,
    ) -> Result<ProjectChat> {
        let info = self.binding(project, chat)?;
        let slot = self.controller(chat);
        let mut owner = slot.lock().await;
        if archived && let Some(session) = owner.take() {
            session.close().await?;
        }
        self.update(|catalog| {
            if archived {
                catalog.archived.insert(chat);
            } else {
                catalog.archived.remove(&chat);
            }
            Ok(())
        })?;
        Ok(ProjectChat {
            project,
            info,
            archived,
        })
    }

    pub fn router(&self, bearer: String) -> Result<axum::Router> {
        http::router(self.clone(), bearer)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectChat {
    pub project: Uuid,
    pub info: ChatInfo,
    pub archived: bool,
}
