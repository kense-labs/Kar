//! A local workbench that shares Kar's saved chats, their native sessions and event cursors.
mod access;
pub(crate) mod acp_registry;
mod agent_sessions;
mod assets;
mod background;
mod http_content;
mod relay_client;
mod storage;
pub use background::BackgroundTasks;
mod chat_controls;
pub(crate) mod chatgpt;
#[doc(hidden)]
pub use chatgpt::write_managed_chatgpt_token;
mod antigravity_proto;
mod configuration;
mod devices;
mod documents;
mod editors;
mod files;
mod force_stop;
mod forge;
mod git;
mod git_actions;
mod handoff;
mod http;
mod interactions;
pub(crate) mod managed_providers;
mod mcp;
mod mcp_apps;
mod native_child_work;
pub(crate) mod native_sqlite;
mod observability;
mod prepared_runs;
mod project_lifecycle;
mod provider_controls;
mod public_web;
mod search;
mod shell;
mod suggestions;
mod task_folders;
mod terminal_preview;
mod thread_family;
mod thread_find;
mod thread_state;
pub use thread_state::Metadata as ChatMetadata;
pub mod persistence;
pub mod policy;
pub mod secrets;
pub mod service;
pub mod ssh;
pub mod tailscale;
pub mod text_generation;
mod voice;
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
    #[serde(flatten, default)]
    pub metadata: project_lifecycle::Metadata,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Catalog {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    prepared_runs: BTreeMap<Uuid, prepared_runs::Job>,
    #[serde(default)]
    desktop_jobs: BTreeMap<Uuid, configuration::desktop::Job>,
    #[serde(default)]
    family: thread_family::Family,
    #[serde(default)]
    server: configuration::Config,
    #[serde(default)]
    environment: mcp::collaboration_tools::environment::Environment,
    #[serde(default)]
    prompt_suggestions: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_tool_permissions: BTreeMap<Uuid, BTreeMap<Uuid, mcp::Policy>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_worktree_moves: BTreeMap<Uuid, mcp::worktree::Move>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_thread_commands: BTreeMap<Uuid, mcp::operations::Record>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_tasks: BTreeMap<Uuid, mcp::orchestrator::Task>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_thread_batches: BTreeMap<Uuid, (Uuid, Uuid, serde_json::Value)>,
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
    #[serde(default)]
    milestone_watches: mcp::milestones::Log,
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
        if let Some(tools) = &lease {
            tools.attach(session);
        }
        *self.model_tools.lock().unwrap() = lease;
        self.publish(session);
    }
    fn publish(&self, session: &ChatSession) {
        *self.live.lock().unwrap() = session.live_session();
        *self.fork.lock().unwrap() = Some(session.live_fork());
    }
}

struct Inner {
    run_workers: Mutex<BTreeSet<Uuid>>,
    run_events: tokio::sync::watch::Sender<u64>,
    desktop_updates: configuration::desktop::Service,
    family_gate: AsyncMutex<()>,
    managed_providers: managed_providers::Manager,
    config_changes: tokio::sync::watch::Sender<u64>,
    keybindings_lock: Mutex<()>,
    relay: relay_client::tunnel::Service,
    acp_registry: acp_registry::Catalog,
    acp_auth: acp_registry::auth::Jobs,
    chatgpt: chatgpt::Jobs,
    mcp: mcp::Registry,
    milestone_hub: mcp::milestones::Hub,
    shell: shell::Service,
    suggestions: crate::PromptSuggestions,
    observability: observability::Service,
    chats: ChatStore,
    roots: Vec<Root>,
    catalog_wal: persistence::WalStore<Catalog>,
    catalog: Mutex<Catalog>,
    viewed_gates: forge::ViewedGates,
    forge_updates: tokio::sync::watch::Sender<forge::signals::Epochs>,
    thread_updates: tokio::sync::watch::Sender<u64>,
    card_updates: tokio::sync::watch::Sender<u64>,
    clones: forge::clone_jobs::Tracker,
    tracker_accounts: forge::trackers::Store,
    signin: forge::signin::Service,
    provider_oauth: provider_controls::Flows,
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
        let (catalog, catalog_wal) =
            persistence::WalStore::<Catalog>::open(&state.join("projects.json"))?;
        let task_folders = task_folders::Store::new(state.join("task-folders"));
        let terminal_preview = terminal_preview::TerminalPreview::open(&state)?;
        let workflows = workflows::Engine::open(&state, clock)?;
        let tracker_accounts = forge::trackers::Store::open(&state)?;
        let milestone_hub = mcp::milestones::Hub::open(&state)?;
        let shell = shell::Service::open(&state, &allowed)?;
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
            let milestone_wake = weak.clone();
            let chats = chats.with_turn_checkpointed(Arc::new(move |chat| {
                let _ = checkpoints.send(chat);
                if let Some(inner) = milestone_wake.upgrade() {
                    inner.milestone_hub.notify();
                }
            }));
            Inner {
                run_workers: Default::default(),
                run_events: Default::default(),
                family_gate: Default::default(),
                relay: Default::default(),
                managed_providers: Default::default(),
                desktop_updates: Default::default(),
                config_changes: tokio::sync::watch::channel(catalog.server.revision).0,
                keybindings_lock: Default::default(),
                acp_registry: Default::default(),
                acp_auth: Default::default(),
                chatgpt: Default::default(),
                mcp: Default::default(),
                milestone_hub,
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
                catalog_wal,
                catalog: Mutex::new(catalog),
                tracker_accounts,
                clones: Default::default(),
                signin: Default::default(),
                provider_oauth: Default::default(),
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
        app.recover_thread_deletions()?;
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
        configuration::next_revision(&saved, &mut next);
        next.thread_state.compact();
        self.0.catalog_wal.record(&next)?;
        if saved.prompt_suggestions != next.prompt_suggestions {
            self.0.suggestions.set_enabled(next.prompt_suggestions);
        }
        for chat in next.archived.difference(&saved.archived) {
            self.0.suggestions.detach(*chat);
        }
        let revision = next.server.revision;
        *saved = next;
        self.0.config_changes.send_if_modified(|n| {
            if *n == revision {
                false
            } else {
                *n = revision;
                true
            }
        });
        self.0.shell.changed();
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
                self.allowed(&actual)?;
                if actual != path || !actual.is_dir() {
                    return Err(Error::Refused(
                        "an additional workspace directory changed or is not a directory".into(),
                    ));
                }
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
        self.create_chat_as(project, id, runtime, model, approvals, settings, None, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_chat_as(
        &self,
        project: Uuid,
        id: Uuid,
        runtime: &str,
        model: Option<String>,
        approvals: crate::ApprovalMode,
        mut settings: crate::SessionSettings,
        launching_chat: Option<Uuid>,
        prepared_run: Option<Uuid>,
    ) -> Result<ProjectChat> {
        forge::clone_jobs::ready(self, project)?;
        let slot = self.controller(id);
        let mut owner = slot.owner.lock().await;
        let reserved = {
            let catalog = self.0.catalog.lock().unwrap();
            prepared_runs::reservation(&catalog, id) != prepared_run
                || (prepared_run.is_none() && mcp::pending_launch(&catalog, id) != launching_chat)
        };
        if reserved {
            return Err(Error::Refused(
                "chat ID is reserved for another launch".into(),
            ));
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
        if let Some(lease) = &tools {
            lease.configure(&mut opts);
        }
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
            let needs_auto_title = !text.trim().is_empty()
                && app
                    .0
                    .catalog
                    .lock()
                    .unwrap()
                    .thread_state
                    .rows
                    .get(&chat)
                    .and_then(|m| m.title.as_deref())
                    .is_none()
                && app
                    .0
                    .chats
                    .history(chat)
                    .map(|h| h.turns.is_empty())
                    .unwrap_or(false);
            let prompt_copy = needs_auto_title.then(|| text.clone());
            let result = owner
                .as_ref()
                .expect("open session")
                .send_attached(input, text, attachments)
                .await;
            drop(owner);
            if let Some(first_msg) = prompt_copy {
                let linked_ctx = app
                    .0
                    .catalog
                    .lock()
                    .unwrap()
                    .forge
                    .chat_links()
                    .remove(&chat)
                    .and_then(|v| {
                        v.first().map(|item| {
                            format!(
                                "{} {}#{}",
                                item["kind"].as_str().unwrap_or("link"),
                                item["repository"].as_str().unwrap_or(""),
                                item["number"].as_u64().unwrap_or(0)
                            )
                        })
                    });
                let generated = text_generation::generate_thread_title_fast(
                    &app,
                    &first_msg,
                    linked_ctx.as_deref(),
                )
                .await;
                let _ = thread_state::set_auto_title_if_empty(&app, project, chat, generated);
            }
            if mcp::worktree::pending(&app, chat) {
                app.wake_queue();
            }
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
        self.archive_family(project, chat, archived).await
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
