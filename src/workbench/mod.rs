//! A local workbench that shares Kar's saved chats, their native sessions and event cursors.
mod access;
mod relay_client;
mod storage;
pub(crate) mod acp_registry;
mod agent_sessions;
mod assets;
mod http_content;
mod background;
pub use background::BackgroundTasks;
mod chat_controls;
mod configuration;
pub(crate) mod managed_providers;
mod devices;
mod antigravity_proto;
pub(crate) mod native_sqlite;
mod native_child_work;
mod observability;
mod mcp;
mod documents;
mod files;
mod editors;
mod force_stop;
mod forge;
mod git;
mod git_actions;
mod handoff;
mod http;
mod interactions;
mod suggestions;
mod project_lifecycle;
mod public_web;
mod search;
mod shell;
mod task_folders;
mod terminal_preview;
mod thread_find;
mod thread_state;
pub use thread_state::Metadata as ChatMetadata;
pub mod workflows;
mod voice;
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
    #[serde(flatten, default)]
    pub metadata: project_lifecycle::Metadata,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Catalog {
    #[serde(default)] server:configuration::Config,
    #[serde(default)]
    environment: mcp::collaboration_tools::environment::Environment,
    #[serde(default)]
    prompt_suggestions: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_tool_permissions: BTreeMap<Uuid, BTreeMap<Uuid, mcp::Policy>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_worktree_moves: BTreeMap<Uuid, mcp::worktree::Move>,
    #[serde(default,skip_serializing_if="BTreeMap::is_empty")]
    model_thread_commands:BTreeMap<Uuid,mcp::operations::Record>,
    #[serde(default,skip_serializing_if="BTreeMap::is_empty")]
    model_tasks:BTreeMap<Uuid,mcp::orchestrator::Task>,
    #[serde(default,skip_serializing_if="BTreeMap::is_empty")]
    model_thread_batches:BTreeMap<Uuid,(Uuid,Uuid,serde_json::Value)>,
    #[serde(default)]
    model_project_commands: BTreeMap<Uuid, mcp::project_tools::Receipt>,
    #[serde(default)]
    assets: assets::Registry,
    #[serde(default)]
    thread_state: thread_state::Store,
    #[serde(default)]
    forge: forge::ForgeState,
    #[serde(default)]
    git_actions: BTreeMap<Uuid, git_actions::Record>,
    #[serde(default)]
    imported_transcripts: BTreeMap<Uuid, BTreeMap<PathBuf, (String, Uuid)>>,
    projects: BTreeMap<Uuid, Project>,
    #[serde(default)]
    lifecycle: project_lifecycle::State,
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
    model_tools: Mutex<Option<mcp::Lease>>,
    // Control requests must be able to answer a native prompt while send holds the owner.
    live: Mutex<std::sync::Weak<crate::Session>>,
    fork: Mutex<Option<crate::chat::LiveFork>>,
}

impl Controller {
    fn bind_model_tools(&self, session: &mut ChatSession, lease: Option<mcp::Lease>) {
        if let Some(tools) = &lease { tools.attach(session); }
        *self.model_tools.lock().unwrap() = lease;
        self.publish(session);
    }
    fn publish(&self, session: &ChatSession) {
        *self.live.lock().unwrap() = session.live_session();
        *self.fork.lock().unwrap() = Some(session.live_fork());
    }
}

struct Inner {
    managed_providers:managed_providers::Manager,
    config_changes:tokio::sync::watch::Sender<u64>,
    relay: relay_client::tunnel::Service,
    acp_registry: acp_registry::Catalog,
    acp_auth: acp_registry::auth::Jobs,
    mcp: mcp::Registry,
    shell: shell::Service,
    suggestions: crate::PromptSuggestions,
    observability: observability::Service,
    chats: ChatStore,
    roots: Vec<Root>,
    catalog: Mutex<Catalog>,
    viewed_gates: forge::ViewedGates,
    forge_updates: tokio::sync::watch::Sender<forge::signals::Epochs>,
    thread_updates: tokio::sync::watch::Sender<u64>,
    card_updates: tokio::sync::watch::Sender<u64>,
    clones: forge::clone_jobs::Tracker,
    tracker_accounts:forge::trackers::Store,
    signin: forge::signin::Service,
    state: PathBuf,
    pump: chat_controls::QueuePump,
    access: access::AccessStore,
    devices: devices::Service,
    voice: voice::Service,
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
        let workflows = workflows::Engine::open(&state, clock)?;
        let tracker_accounts=forge::trackers::Store::open(&state)?;
        let shell = shell::Service::open(&state,&allowed)?;
        let access = access::AccessStore::open(&state)?;
        let devices = devices::Service::open(&state)?;
        let (checkpoints, completed) = std::sync::mpsc::channel();
        let voice = voice::Service::open(&state)?;
        let app = Self(Arc::new_cyclic(|weak: &std::sync::Weak<Inner>| {
            let service = weak.clone();
            let chats = chats.with_repository_scopes(Arc::new(move |chat| {
                let inner = service
                    .upgrade()
                    .ok_or_else(|| Error::Closed("workbench stopped".into()))?;
                task_folders::checkpoints::repositories(&Workbench(inner), chat)
            }));
            let chats = chats.with_turn_checkpointed(Arc::new(move |chat| { let _ = checkpoints.send(chat); }));
            Inner {
                relay: Default::default(),
                managed_providers:Default::default(),
                config_changes:tokio::sync::watch::channel(catalog.server.revision).0,
                acp_registry: Default::default(),
                acp_auth: Default::default(),
                mcp: Default::default(),
                shell,
                suggestions: crate::PromptSuggestions::new(catalog.prompt_suggestions),
                observability: observability::Service::open(&state),
                workflows,
                terminal_preview,
                task_folders,
                chats,
                roots: allowed,
                viewed_gates: Default::default(),
                forge_updates: tokio::sync::watch::channel(catalog.forge.epochs()).0,
                card_updates: tokio::sync::watch::channel(0).0,
                thread_updates: tokio::sync::watch::channel(catalog.thread_state.sequence()).0,
                catalog: Mutex::new(catalog),
                tracker_accounts,
                clones: Default::default(),
                signin: Default::default(),
                pump: Default::default(),
                access,
                state,
                devices,
                voice,
                documents: Default::default(),
                _lease: lease,
                controllers: Default::default(),
            }
        }));
        forge::signals::checkpoint_worker(&app, completed)?;
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
        configuration::next_revision(&saved,&mut next);
        let mut tmp = tempfile::NamedTempFile::new_in(&self.0.state)?;
        serde_json::to_writer(&mut tmp, &next)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.0.state.join("projects.json"))
            .map_err(|e| e.error)?;
        if saved.prompt_suggestions != next.prompt_suggestions {
            self.0.suggestions.set_enabled(next.prompt_suggestions);
        }
        for chat in next.archived.difference(&saved.archived) {
            self.0.suggestions.detach(*chat);
        }
        let revision=next.server.revision;
        *saved = next;
        self.0.config_changes.send_if_modified(|n|if *n==revision{false}else{*n=revision;true});
        self.0.shell.changed();
        #[cfg(unix)]
        File::open(&self.0.state)?.sync_all()?;
        forge::signals::notify(&self.0.forge_updates, saved.forge.epochs());
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
                metadata: Default::default(),
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
        let catalog = self.0.catalog.lock().unwrap();
        if !self.owns_chat(&catalog, &project, &info)? {
            return Err(missing());
        }
        self.workspace_at(&info.workspace)?;
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
        let p = self.project(project)?;
        let infos = self.0.chats.list()?;
        let catalog = self.0.catalog.lock().unwrap();
        let mut chats = Vec::new();
        for info in infos {
            let archived = catalog.archived.contains(&info.id);
            if (include_archived || !archived) && self.owns_chat(&catalog, &p, &info)? {
                chats.push(ProjectChat {
                    project,
                    metadata: catalog
                        .thread_state
                        .rows
                        .get(&info.id)
                        .cloned()
                        .unwrap_or_default(),
                    info,
                    archived,
                });
            }
        }
        Ok(chats)
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
        settings: crate::SessionSettings,
    ) -> Result<ProjectChat> {
        self.create_chat_as(project, id, runtime, model, approvals, settings, None).await
    }

    async fn create_chat_as(
        &self,
        project: Uuid,
        id: Uuid,
        runtime: &str,
        model: Option<String>,
        approvals: crate::ApprovalMode,
        mut settings: crate::SessionSettings,
        launching_chat: Option<Uuid>,
    ) -> Result<ProjectChat> {
        forge::clone_jobs::ready(self, project)?;
        let slot = self.controller(id);
        let mut owner = slot.owner.lock().await;
        if mcp::pending_launch(&self.0.catalog.lock().unwrap(), id) != launching_chat {
            return Err(Error::Refused("chat ID is reserved for another launch".into()));
        }
        let cwd = self.reserve_chat_directory(project, id)?;
        settings.additional_directories = self.chat_directories(&cwd, &settings)?;
        match self.0.chats.initial_info(id) {
            Ok(info)
                if info.forked_from.is_none()
                    && self.0.chats.saved_info(id)?.workspace == cwd
                    && info.runtime == runtime
                    && info.approvals == approvals
                    && info.settings == settings
                    && info.model == model =>
            {
                return Ok(ProjectChat {
                    project,
                    metadata: self.thread_metadata(id),
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
        let mut opts = SessionOptions::new(cwd);
        opts.remove_env("KAR_WORKBENCH_TOKEN")?;
        opts.prompt_suggestions = self.suggestion_source(id);
        opts.model = model;
        opts.approvals = approvals;
        opts.settings = settings;
        let tools = self.0.mcp.issue(project, id, runtime)?;
        if let Some(lease) = &tools { lease.configure(&mut opts); }
        let mut session = self.0.chats.create_with_id(id, runtime, None, opts).await?;
        let info = session.info().clone();
        slot.bind_model_tools(&mut session, tools);
        *owner = Some(session);
        Ok(ProjectChat {
            project,
            metadata: self.thread_metadata(id),
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
        self.send_attached(project, chat, input, text, Vec::new())
            .await
    }
    pub async fn send_attached(
        &self,
        project: Uuid,
        chat: Uuid,
        input: Uuid,
        text: String,
        attachments: Vec<Uuid>,
    ) -> Result<ChatTurn> {
        self.binding(project, chat)?;
        let app = self.clone();
        tokio::spawn(async move {
            let controller = app.controller(chat);
            let mut owner = controller.owner.lock().await;
            app.binding(project, chat)?;
            if app.archived(chat) {
                return Err(Error::Refused("chat is archived".into()));
            }
            app.attach(project, chat, &mut owner).await?;
            let result = owner.as_ref().expect("open session")
                .send_attached(input, text, attachments).await;
            drop(owner);
            if mcp::worktree::pending(&app,chat) {app.wake_queue();}
            result
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
            let changed = if archived {
                catalog.archived.insert(chat)
            } else {
                catalog.archived.remove(&chat)
            };
            if changed {
                catalog.forge.linked_chat_changed(chat)?;
                let now = jiff::Timestamp::now().as_millisecond() as u64;
                let meta = catalog.thread_state.rows.entry(chat).or_default();
                meta.archived_ms = archived.then_some(now);
                meta.updated_ms = now;
            }
            Ok(())
        })?;
        self.0.access.notify();
        Ok(ProjectChat {
            project,
            metadata: self.thread_metadata(chat),
            info,
            archived,
        })
    }

    /// Publish the actual bound loopback listener to native processes with chat-scoped credentials.
    pub fn router_at(&self, bearer: String, address: std::net::SocketAddr) -> Result<axum::Router> {
        self.0.mcp.set_endpoint(address)?;
        self.router(bearer)
    }

    pub fn router(&self, bearer: String) -> Result<axum::Router> {
        http::router(self.clone(), bearer)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectChat {
    #[serde(default)]
    pub metadata: ChatMetadata,
    pub project: Uuid,
    pub info: ChatInfo,
    pub archived: bool,
}
