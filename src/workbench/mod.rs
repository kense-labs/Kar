//! A local workbench that shares Kar's saved chats, their native sessions and event cursors.
mod access;
mod chat_controls;
mod documents;
mod files;
mod forge;
mod git;
mod git_actions;
mod handoff;
mod http;
mod interactions;
mod search;
mod task_folders;
mod terminal_preview;
pub mod workflows;
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
    #[serde(default)]
    forge: forge::ForgeState,
    #[serde(default)]
    git_actions: BTreeMap<Uuid, git_actions::Record>,
    projects: BTreeMap<Uuid, Project>,
    #[serde(default)]
    tracked_folders: BTreeMap<Uuid, Vec<task_folders::Tracked>>,
    archived: BTreeSet<Uuid>,
}

struct Root {
    path: PathBuf,
    dir: Dir,
}

#[derive(Default)]
struct Controller {
    owner: AsyncMutex<Option<ChatSession>>,
    // Control requests must be able to answer a native prompt while send holds the owner.
    live: Mutex<std::sync::Weak<crate::Session>>,
}

impl Controller {
    fn publish(&self, session: &ChatSession) {
        *self.live.lock().unwrap() = session.live_session();
    }
}

struct Inner {
    chats: ChatStore,
    roots: Vec<Root>,
    catalog: Mutex<Catalog>,
    clones: forge::clone_jobs::Tracker,
    state: PathBuf,
    pump: chat_controls::QueuePump,
    access: access::AccessStore,
    documents: documents::Documents,
    terminal_preview: terminal_preview::TerminalPreview,
    task_folders: task_folders::Store,
    workflows: workflows::Engine,
    _lease: File,
    controllers: Mutex<HashMap<Uuid, Arc<Controller>>>,
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
        Self::open_with_workflow_clock(chats, roots, Arc::new(workflows::SystemClock))
    }

    pub fn open_with_workflow_clock(
        chats: ChatStore,
        roots: &[PathBuf],
        clock: Arc<dyn workflows::Clock>,
    ) -> Result<Self> {
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
        let task_folders = task_folders::Store::new(state.join("task-folders"));
        let terminal_preview = terminal_preview::TerminalPreview::open(&state)?;
        let app = Self(Arc::new(Inner {
            workflows: workflows::Engine::open(&state, clock)?,
            terminal_preview,
            task_folders,
            chats,
            roots: allowed,
            catalog: Mutex::new(catalog),
            clones: Default::default(),
            pump: Default::default(),
            access: access::AccessStore::open(&state)?,
            state,
            documents: Default::default(),
            _lease: lease,
            controllers: Default::default(),
        }));
        git_actions::recover(&app)?;
        forge::clone_jobs::recover(&app)?;
        Ok(app)
    }

    fn allowed(&self, path: &Path) -> Result<&Root> {
        if path.starts_with(self.0.chats.directory())
            || path
                .components()
                .any(|p| p.as_os_str().eq_ignore_ascii_case(".git"))
        {
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

    fn chat_directories(
        &self,
        workspace: &Path,
        settings: &crate::SessionSettings,
    ) -> Result<Vec<PathBuf>> {
        let paths = settings.directories(workspace)?;
        paths
            .into_iter()
            .map(|path| {
                let actual = path.canonicalize()?;
                if actual != path || !actual.is_dir() {
                    return Err(Error::Refused(
                        "an additional workspace directory changed or is not a directory".into(),
                    ));
                }
                self.allowed(&actual)?;
                Ok(actual)
            })
            .collect()
    }

    fn binding(&self, project: Uuid, chat: Uuid) -> Result<ChatInfo> {
        let project = self.project(project)?;
        let info = self.0.chats.saved_info(chat)?;
        if info.workspace != project.root
            && self.0.task_folders.workspace(project.id, chat)?.as_ref() != Some(&info.workspace)
        {
            return Err(missing());
        }
        Ok(info)
    }

    fn controller(&self, id: Uuid) -> Arc<Controller> {
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
            .filter(|c| {
                c.workspace == root
                    || self
                        .0
                        .task_folders
                        .workspace(project, c.id)
                        .ok()
                        .flatten()
                        .as_ref()
                        == Some(&c.workspace)
            })
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
        self.create_chat_with_settings(
            project,
            id,
            runtime,
            model,
            crate::ApprovalMode::Deny,
            Default::default(),
        )
        .await
    }

    pub async fn create_chat_with_settings(
        &self,
        project: Uuid,
        id: Uuid,
        runtime: &str,
        model: Option<String>,
        approvals: crate::ApprovalMode,
        mut settings: crate::SessionSettings,
    ) -> Result<ProjectChat> {
        forge::clone_jobs::ready(self, project)?;
        let slot = self.controller(id);
        let mut owner = slot.owner.lock().await;
        let mut p = self.project(project)?;
        if let Some(cwd) = self.0.task_folders.workspace(project, id)? {
            p.root = cwd;
        }
        self.workspace(&p)?;
        settings.additional_directories = self.chat_directories(&p.root, &settings)?;
        match self.0.chats.initial_info(id) {
            Ok(info)
                if info.forked_from.is_none()
                    && info.workspace == p.root
                    && info.runtime == runtime
                    && info.approvals == approvals
                    && info.settings == settings
                    && info.model == model =>
            {
                return Ok(ProjectChat {
                    project,
                    info: self.0.chats.saved_info(id)?,
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
        opts.remove_env("KAR_WORKBENCH_TOKEN")?;
        opts.model = model;
        opts.approvals = approvals;
        opts.settings = settings;
        let session = self.0.chats.create_with_id(id, runtime, None, opts).await?;
        let info = session.info().clone();
        slot.publish(&session);
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
            let mut owner = controller.owner.lock().await;
            if app.archived(chat) {
                return Err(Error::Refused("chat is archived".into()));
            }
            app.attach(chat, &mut owner).await?;
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
        let mut owner = slot.owner.lock().await;
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
        self.0.access.notify();
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
