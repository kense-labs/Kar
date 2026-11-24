mod request;
mod source;
mod worktree;
use super::{ChatInfo, ChatSession, ChatStore, CheckpointPoint, Sink, TurnStatus, log};
use crate::{Error, Protocol, Result, Session, SessionOptions};
use request::Request;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Policy for creating a child through a selected checkpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkPolicy {
    Native,
    #[default]
    PreferNative,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkMode {
    #[default]
    Native,
    PortableContext,
}

impl ForkPolicy {
    fn check(self, mode: ForkMode) -> Result<()> {
        if self == Self::Native && mode == ForkMode::PortableContext {
            return Err(Error::Unsupported(
                "this fork requires portable context".into(),
            ));
        }
        Ok(())
    }
}
/// The parent chat and the provider-issued anchor copied into a new native session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForkOrigin {
    #[serde(default)]
    pub mode: ForkMode,
    pub chat: Uuid,
    pub turn: Uuid,
    pub native_source: String,
    /// Directory of the native provider session which owns the selected turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub anchor: String,
    pub checkpoint_store: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_base: Option<ForkWorktreeBase>,
}

/// Git checkout and immutable preimage used to resume a partially applied independent fork.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForkWorktreeBase {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<ForkRepositoryBase>,
    pub tree: String,
    pub head: String,
    pub branch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForkRepositoryBase {
    pub scope: super::checkpoint::RepositoryScope,
    pub tree: String,
    pub head: String,
    pub branch: Option<String>,
}

impl ForkRepositoryBase {
    pub(in crate::chat) fn verify(&self) -> Result<()> {
        let work = &self.scope.worktree_path;
        let repo = crate::workbench_git::Repository::open(work)?;
        let status = repo.status()?;
        if work != repo.root()
            || work.canonicalize()? != *work
            || status.head.as_ref() != Some(&self.head)
            || status.branch != self.branch
        {
            return Err(Error::Refused(format!(
                "fork repository checkout changed: {}",
                work.display()
            )));
        }
        Ok(())
    }
}
/// A workbench send holds its owner until the turn settles. An independent
/// checkpoint fork can read the live journal through this non-owning handle.
#[derive(Clone)]
pub(crate) struct LiveFork {
    sender: std::sync::Weak<super::Sender>,
    options: std::sync::Weak<SessionOptions>,
}
impl LiveFork {
    pub(crate) async fn fork(
        self,
        turn: Uuid,
        child: Uuid,
        destination: PathBuf,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<super::ChatHistory> {
        let sender = self.sender.upgrade().ok_or(Error::Busy)?;
        if sender.session.is_closed() {
            return Err(Error::Busy);
        }
        {
            let log = sender.sink.0.lock().unwrap();
            if !isolated_codex_cut(&log.state, turn) || log.state.active().is_none() {
                return Err(Error::Busy);
            }
        }
        sender
            .store
            .fork_request(
                sender.info.id,
                turn,
                child,
                Some(destination),
                ForkPolicy::Native,
                Some((
                    sender.sink.clone(),
                    self.options.upgrade().ok_or(Error::Busy)?.as_ref().clone(),
                )),
                configure,
            )
            .await
    }
}

fn isolated_codex_cut(state: &log::State, id: Uuid) -> bool {
    state.info.protocol == Protocol::Codex
        && state.find(id).is_some_and(|t| {
            t.status == TurnStatus::Completed
                && t.fork_anchor.as_ref().is_some_and(|anchor| {
                    !anchor.is_empty() && Some(anchor) == t.native_turn_id.as_ref()
                })
                && t.checkpoint
                    .as_ref()
                    .is_some_and(|c| c.after_tree.is_some())
                && source::witness(t).ok() == Some(state.info.native_id.as_str())
        })
}

impl ChatStore {
    pub(crate) fn fork_claimed(&self, id: Uuid) -> Result<bool> {
        Ok(Request::path(self, id).try_exists()?)
    }

    fn fork_stage(&self, source: Uuid) -> PathBuf {
        self.dir.join(format!("{source}.forking"))
    }
    fn staged_fork(&self, source: Uuid) -> Result<Option<ChatInfo>> {
        let file = match File::open(self.fork_stage(source)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let staged = log::staged_info(file)?;
        if staged.forked_from.as_ref().is_none_or(|f| f.chat != source) {
            return Err(Error::Protocol("invalid staged fork".into()));
        }
        Ok(Some(staged))
    }
    pub(super) fn pending_fork(&self, source: Uuid) -> Result<Option<ForkOrigin>> {
        Ok(self
            .staged_fork(source)?
            .and_then(|child| child.forked_from))
    }

    pub(super) fn require_no_pending_fork(&self, source: Uuid) -> Result<()> {
        if let Some(f) = self
            .pending_fork(source)?
            .filter(|f| f.worktree_base.is_none())
        {
            return Err(Error::Refused(format!(
                "finish the pending fork: kar chat fork {} --chat {source}",
                f.turn
            )));
        }
        Ok(())
    }
    /// Fork the native context through the end of a saved turn and restore its Git tree.
    pub async fn fork_at(&self, source: Uuid, turn: Uuid) -> Result<ChatSession> {
        self.fork_at_with(source, turn, |_| {}).await
    }
    pub async fn fork_at_with(
        &self,
        source: Uuid,
        turn: Uuid,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<ChatSession> {
        let log = self.open_controller(source).await?;
        let opts = log.state.info.options();
        let source = Sink(Arc::new(Mutex::new(log)));
        let mut launch = self.prepare_fork(&source, turn, opts, false).await?;
        configure(&mut launch.options);
        self.fork_from(source, turn, launch, None).await
    }
    /// Fork files and native context into a linked checkout previously created by Kar.
    pub async fn fork_at_in_worktree(
        &self,
        source: Uuid,
        turn: Uuid,
        checkout: impl Into<PathBuf>,
    ) -> Result<ChatSession> {
        let log = self.open_controller(source).await?;
        let opts = log.state.info.options();
        let source = Sink(Arc::new(Mutex::new(log)));
        let launch = self.prepare_fork(&source, turn, opts, true).await?;
        self.fork_from(source, turn, launch, Some(checkout.into()))
            .await
    }
    /// Return the child history for a stable public request, including replays after publication.
    pub async fn fork_at_with_id(
        &self,
        source: Uuid,
        turn: Uuid,
        child_id: Uuid,
        worktree: Option<PathBuf>,
    ) -> Result<super::ChatHistory> {
        self.fork_at_with_id_configured(source, turn, child_id, worktree, |_| {})
            .await
    }

    pub async fn fork_at_with_id_configured(
        &self,
        source: Uuid,
        turn: Uuid,
        child_id: Uuid,
        worktree: Option<PathBuf>,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<super::ChatHistory> {
        self.fork_at_with_policy(
            source,
            turn,
            child_id,
            worktree,
            ForkPolicy::Native,
            configure,
        )
        .await
    }

    pub async fn fork_at_with_policy(
        &self,
        source: Uuid,
        turn: Uuid,
        child_id: Uuid,
        worktree: Option<PathBuf>,
        policy: ForkPolicy,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<super::ChatHistory> {
        self.fork_request(source, turn, child_id, worktree, policy, None, configure)
            .await
    }

    async fn fork_request(
        &self,
        source: Uuid,
        turn: Uuid,
        child_id: Uuid,
        worktree: Option<PathBuf>,
        policy: ForkPolicy,
        live: Option<(Sink, SessionOptions)>,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<super::ChatHistory> {
        let path = worktree.as_deref();
        if source == child_id {
            return Err(Error::Refused(
                "fork source and child UUID must differ".into(),
            ));
        }
        if let Some(published) = Request::published(self, source, turn, child_id, path, policy)? {
            return Ok(published);
        }
        let lease = self.lease(child_id, true)?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Request::while_busy(self, source, turn, child_id, path, policy);
            }
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        if let Some(published) = Request::published(self, source, turn, child_id, path, policy)? {
            return Ok(published);
        }
        let request = Request::claim(self, source, turn, child_id, path, lease)?;
        let (source, opts) = match live {
            Some(controller) => controller,
            None => {
                let log = self.open_controller(source).await?;
                let opts = log.state.info.options();
                (Sink(Arc::new(Mutex::new(log))), opts)
            }
        };
        let mut launch = self
            .prepare_fork(&source, turn, opts, worktree.is_some())
            .await?;
        configure(&mut launch.options);
        crate::stream::retention::check_limit(launch.options.retained_bytes)?;
        let store = self.clone();
        // Closing the HTTP stream does not cancel native dispatch, publication, or release.
        tokio::spawn(async move {
            let child = store
                .run_fork(source, turn, launch, worktree, Some(request), policy)
                .await?;
            child.close().await?;
            store.history(child_id)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
    pub(super) async fn open_saved(
        state: &log::State,
        mut opts: SessionOptions,
    ) -> Result<Session> {
        let info = &state.info;
        opts.cwd = info.workspace.clone();
        let rt = info.launch_runtime(&opts).await?;
        // Claude persists the fork transcript on its first new prompt. Until then the
        // durable identity is the source, inclusive anchor and the preassigned child UUID.
        if rt.protocol == Protocol::Claude
            && let Some(origin) = &info.forked_from
            && origin.mode == ForkMode::Native
            && state.turns.last().is_some_and(|t| t.id == origin.turn)
        {
            opts.resume = None;
            let id = Uuid::parse_str(&info.native_id)
                .map_err(|_| Error::Protocol("invalid Claude branch id".into()))?;
            return rt
                .fork_selected(&origin.native_source, Some(&origin.anchor), opts, Some(id))
                .await;
        }
        // Grok retains a saved append-only rule. Resending it alone would be ignored.
        if rt.id == "grok"
            && opts.settings.system_prompt.is_none()
            && opts.settings.append_system_prompt == info.settings.append_system_prompt
        {
            opts.settings.append_system_prompt = None;
        }
        // Droid persists the original instructions with the native session.
        if rt.protocol == Protocol::Factory
            && opts.settings.system_prompt == info.settings.system_prompt
            && opts.settings.append_system_prompt == info.settings.append_system_prompt
        {
            opts.settings.system_prompt = None;
            opts.settings.append_system_prompt = None;
        }
        opts.resume = Some(info.native_id.clone());
        let session = if rt.id == "opencode"
            && rt.protocol == Protocol::Acp
            && state.unfinished_turn().is_some_and(|t| {
                crate::providers::opencode_recovery::has_witness(t, &info.native_id)
            }) {
            Session::open_with_replay(&rt, opts, None, None, true, None).await?
        } else if rt.protocol == Protocol::KimiKap {
            crate::kimi::open(&rt, opts, false, state.unfinished_turn()).await?
        } else if rt.protocol == Protocol::Dsh {
            crate::dsh::open(&rt, opts, None, state.unfinished_turn(), None).await?
        } else {
            rt.open_selected(opts).await?
        };
        if session.id() != info.native_id
            || (rt.protocol == Protocol::Claude
                && session
                    .rpc("get_status", serde_json::json!({}))
                    .await?
                    .as_str()
                    != Some(&info.native_id))
        {
            session.close().await?;
            return Err(Error::Protocol("provider resumed a different chat".into()));
        }
        if rt.protocol == Protocol::Codex
            && info.forked_from.as_ref().is_some_and(|f| {
                f.worktree_base.is_some()
                    || f.native_workspace
                        .as_ref()
                        .is_some_and(|dir| dir != &info.workspace)
            })
        {
            worktree::confirm(&session, &info.workspace).await?;
        }
        Ok(session)
    }
    async fn prepare_fork(
        &self,
        source: &Sink,
        turn: Uuid,
        options: SessionOptions,
        independent: bool,
    ) -> Result<source::Launch> {
        let store = self.clone();
        let source = source.clone();
        tokio::task::spawn_blocking(move || {
            let log = source.0.lock().unwrap();
            if log.state.pending_handoff().is_some()
                || (log.state.active().is_some()
                    && !(independent && isolated_codex_cut(&log.state, turn)))
            {
                return Err(Error::Busy);
            }
            source::Launch::prepare(&store, &log.state, turn, options)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
    async fn fork_from(
        &self,
        source: Sink,
        turn: Uuid,
        launch: source::Launch,
        checkout: Option<PathBuf>,
    ) -> Result<ChatSession> {
        crate::stream::retention::check_limit(launch.options.retained_bytes)?;
        let store = self.clone();
        tokio::spawn(async move {
            store
                .run_fork(source, turn, launch, checkout, None, ForkPolicy::Native)
                .await
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
    async fn run_fork(
        &self,
        source: Sink,
        turn: Uuid,
        launch: source::Launch,
        checkout: Option<PathBuf>,
        request: Option<Request>,
        policy: ForkPolicy,
    ) -> Result<ChatSession> {
        let mut parent = {
            let log = source.0.lock().unwrap();
            if log.state.pending_handoff().is_some() {
                return Err(Error::Busy);
            }
            log.state.info.clone()
        };
        launch.activate(&mut parent);
        // All forks share the source stage. Serialize its publication separately
        // from writes to the working files so two destination forks cannot race.
        let _publication = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(format!("{}.fork.lock", parent.id)))?;
        match _publication.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let _pin = parent.pin_worktree()?;
        let stage = self.fork_stage(parent.id);
        let staged_info = self.staged_fork(parent.id)?;
        if staged_info.as_ref().map(|i| i.id) != launch.staged {
            return Err(Error::Busy);
        }
        let changed_workspace =
            launch.staged.is_none() && launch.owner.workspace != parent.workspace;
        if changed_workspace
            && launch.owner.protocol != Protocol::Codex
            && policy == ForkPolicy::Native
        {
            return Err(Error::Refused("native owner of this inherited turn belongs to another workspace without a verified directory override".into()));
        }
        let _origin_pin = if changed_workspace {
            if launch.owner.workspace.canonicalize()? != launch.owner.workspace {
                return Err(Error::Refused(
                    "historical native workspace now resolves to another directory".into(),
                ));
            }
            launch.owner.pin_worktree()?
        } else {
            None
        };
        let mut opts = launch.options;
        opts.cwd = parent.workspace.clone();
        opts.resume = None;
        let opencode = parent.protocol == Protocol::Acp && parent.runtime == "opencode";
        let native_route = (matches!(
            parent.protocol,
            Protocol::Codex | Protocol::Claude | Protocol::Dsh | Protocol::Pi
        ) || opencode)
            && (parent.protocol == Protocol::Codex || (!changed_workspace && checkout.is_none()));
        if !native_route && policy == ForkPolicy::Native {
            return Err(Error::Unsupported(format!(
                "{} does not support native forks through a selected turn in this checkout",
                parent.runtime
            )));
        }
        let stable = request.is_some();
        if let (Some(staged), Some(request)) = (&staged_info, &request)
            && staged.id != request.id
        {
            return Err(Error::Refused(
                "a different child UUID is pending for this chat".into(),
            ));
        }
        let staged_checkout = staged_info.as_ref().filter(|i| {
            !stable
                && i.forked_from
                    .as_ref()
                    .is_some_and(|f| f.worktree_base.is_some())
        });
        let destination = checkout
            .as_deref()
            .or_else(|| staged_checkout.map(|i| i.workspace.as_path()))
            .map(|path| worktree::Destination::open(&parent, path, &mut opts).map(Arc::new))
            .transpose()?;
        let independent_cut =
            destination.is_some() && isolated_codex_cut(&source.0.lock().unwrap().state, turn);
        let _parent_lease = if destination.is_some() && !independent_cut {
            Some(self.workspace_lease(&parent.workspace)?)
        } else {
            None
        };
        let workspace = Self::workspace_lease_for(&opts.cwd, Some(&stage))?;
        if staged_info.as_ref().map(|info| info.id)
            != self.staged_fork(parent.id)?.map(|info| info.id)
        {
            return Err(Error::Busy);
        }
        let target = CheckpointPoint {
            turn,
            before: false,
        };
        let staged = staged_info.is_some();
        let mut prefix = {
            let log = source.0.lock().unwrap();
            log.healthy()?;
            if !independent_cut
                && (log.state.active().is_some()
                    || !log.state.pending_requests.is_empty()
                    || log.state.compacting()
                    || log.state.waiting_steering()
                    || log.state.observing_notifications())
            {
                return Err(Error::Busy);
            }
            if log.state.restoring.is_some() && !staged {
                return Err(Error::Refused(
                    "finish the pending workspace rollback first".into(),
                ));
            }
            let index = log
                .state
                .turns
                .iter()
                .position(|t| t.id == turn)
                .ok_or_else(|| Error::Refused("unknown turn in this chat".into()))?;
            let cut = &log.state.turns[index];
            if source::witness(cut)? != parent.native_id {
                return Err(Error::Refused(
                    "selected turn changed its native session binding".into(),
                ));
            }
            if cut.status == TurnStatus::Running
                || cut
                    .checkpoint
                    .as_ref()
                    .and_then(|c| c.after_tree.as_ref())
                    .is_none()
            {
                return Err(Error::Refused(
                    "fork needs a finished turn with a final workspace checkpoint".into(),
                ));
            }
            log.state.turns[..=index].to_vec()
        };
        // A submitted steer may have executed only after the selected anchor.
        // Keep its link on a branch only when the actual input is inherited.
        if parent.protocol == Protocol::Claude {
            let carried: std::collections::HashSet<Uuid> = prefix
                .iter()
                .flat_map(|t| &t.items)
                .filter_map(|i| match &i.item {
                    super::Item::Message {
                        role: super::Role::User,
                        provider_id: Some(key),
                        ..
                    } => Uuid::parse_str(key).ok(),
                    _ => None,
                })
                .collect();
            for t in &mut prefix {
                t.steered_inputs.retain(|id, status| {
                    *status != crate::SteerStatus::FollowUp || carried.contains(id)
                });
            }
        }
        let mut recorded_repositories = std::collections::BTreeMap::new();
        for row in prefix
            .iter()
            .filter_map(|t| t.checkpoint.as_ref())
            .flat_map(|cp| &cp.repositories)
        {
            recorded_repositories.insert(row.scope.repository_path.clone(), row.scope.clone());
        }
        let cut = prefix.last_mut().expect("selected turn is in prefix");
        let native_source = parent.native_id.clone();
        let saved = if let Some(info) = staged_info {
            let mode = info
                .forked_from
                .as_ref()
                .expect("validated staged fork")
                .mode;
            policy.check(mode)?;
            if destination.as_ref().map(|d| (d.id, d.workspace.as_path()))
                != info
                    .forked_from
                    .as_ref()
                    .and_then(|f| f.worktree_base.as_ref())
                    .and_then(|_| info.worktree_id.map(|id| (id, info.workspace.as_path())))
            {
                return Err(Error::Refused(
                    "retry the pending fork in its original worktree".into(),
                ));
            }
            if !info.forked_from.as_ref().is_some_and(|f| {
                f.chat == parent.id
                    && f.turn == turn
                    && (mode == ForkMode::PortableContext
                        || cut.fork_anchor.as_ref() == Some(&f.anchor)
                        || (parent.protocol == Protocol::Pi
                            && cut.fork_anchor.is_none()
                            && cut.pi_input_timestamp_ms.is_some()
                            && cut.status == TurnStatus::Completed)
                        || (opencode && cut.fork_anchor.is_none()))
                    && f.native_source == native_source
            }) {
                return Err(Error::Refused(
                    "another fork is pending; retry its original turn".into(),
                ));
            }
            info
        } else {
            let child_id = request.as_ref().map(|r| r.id).unwrap_or_else(Uuid::new_v4);
            let worktree_base = if let Some(dest) = &destination {
                let (dest, store, parent) = (dest.clone(), self.clone(), parent.clone());
                Some(
                    tokio::task::spawn_blocking(move || {
                        dest.baseline(&store, &parent, child_id, recorded_repositories)
                    })
                    .await
                    .map_err(|e| Error::Closed(e.to_string()))??,
                )
            } else {
                self.fork_restore(&source, target, true).await?;
                None
            };
            if native_route
                && cut.fork_anchor.is_none()
                && parent.protocol == Protocol::Pi
                && cut.status == TurnStatus::Completed
            {
                let (local, root, opts) = (cut.clone(), native_source.clone(), opts.clone());
                cut.fork_anchor = tokio::task::spawn_blocking(move || {
                    log::pi::saved_anchor(&local, &root, &opts)
                })
                .await
                .map_err(|e| Error::Closed(e.to_string()))??;
            }
            let rt = parent.launch_runtime(&opts).await?;
            let mut native = native_route && (cut.fork_anchor.is_some() || opencode);
            let mut version = None;
            if native
                && policy == ForkPolicy::PreferNative
                && (opencode || matches!(rt.protocol, Protocol::Codex | Protocol::Claude))
            {
                let reported = rt.launch_version(&opts).await?;
                native = if opencode {
                    crate::account::release(&reported).is_some_and(|v| v.0 == 2)
                } else {
                    rt.check_turn_fork_version(&reported, &opts).is_ok()
                };
                version = Some(reported);
            }
            if !native && policy == ForkPolicy::Native {
                return Err(Error::Unsupported(
                    "selected turn has no native fork anchor".into(),
                ));
            }
            if let Some(r) = &request {
                r.begin(parent.id, turn, destination.as_deref())?;
            }
            let opened = async {
                if !native {
                    let branch = rt.open_selected(opts.before_chat_attach()?).await?;
                    if !branch.capabilities().resume || branch.id() == native_source {
                        branch.close().await?;
                        return Err(Error::Refused(
                            "portable fork needs an independent recoverable provider session"
                                .into(),
                        ));
                    }
                    return Ok((branch, None));
                }
                let (branch, anchor) = if let Some(anchor) = cut.fork_anchor.clone() {
                    let session = rt
                        .fork_selected_version(
                            &native_source,
                            Some(&anchor),
                            opts.clone(),
                            None,
                            version.as_deref(),
                        )
                        .await?;
                    (session, anchor)
                } else {
                    use crate::providers::opencode_fork::{self, Target};
                    opencode_fork::open(&rt, opts.clone(), &native_source, Target::Turn(cut))
                        .await?
                };
                if changed_workspace || destination.is_some() {
                    worktree::confirm(&branch, &opts.cwd).await?;
                }
                Ok::<_, Error>((branch, Some(anchor)))
            }
            .await;
            let (branch, anchor) = match opened {
                Ok(result) => result,
                Err(error) => {
                    if let Some(r) = &request {
                        r.record_error()?;
                    }
                    return Err(error);
                }
            };
            if let Some(base) = &worktree_base {
                self.pin_fork_baseline(&parent, child_id, "fork-base", &base.tree)?;
                for (i, scope) in base.repositories.iter().enumerate() {
                    self.pin_fork_baseline(
                        &parent,
                        child_id,
                        &format!("fork-repositories/{i}"),
                        &scope.tree,
                    )?;
                }
            }
            let mode = if let Some(anchor) = &anchor {
                cut.fork_anchor = Some(anchor.clone());
                ForkMode::Native
            } else {
                ForkMode::PortableContext
            };
            let origin = ForkOrigin {
                mode,
                chat: parent.id,
                turn,
                native_source,
                native_workspace: Some(launch.owner.workspace.clone()),
                anchor: anchor.unwrap_or_default(),
                checkpoint_store: parent.checkpoint_store(),
                worktree_base,
            };
            let child = ChatInfo {
                provider_instance: parent.active_instance().cloned(),
                imported: None,
                id: child_id,
                workspace: opts.cwd.clone(),
                worktree_id: destination.as_ref().map(|d| d.id).or(parent.worktree_id),
                native_id: branch.id().into(),
                created_ms: super::now(),
                model: opts.model.clone(),
                settings: opts.settings.clone(),
                unset_env: opts.unset_env.clone(),
                approvals: opts.approvals,
                forked_from: Some(origin),
                ..parent
            };
            let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
            log::header(tmp.as_file_mut(), &child)?;
            let bootstrap = (mode == ForkMode::PortableContext)
                .then(|| super::Handoff::for_fork(&child, &prefix))
                .transpose()?;
            log::inherit(tmp.as_file_mut(), prefix)?;
            if let Some(handoff) = bootstrap {
                log::prepare_portable_fork(tmp.as_file_mut(), &child, handoff)?;
            }
            workspace.reserve_fork(&stage)?;
            tmp.persist_noclobber(&stage).map_err(|e| e.error)?;
            #[cfg(unix)]
            File::open(&self.dir)?.sync_all()?;
            branch.close().await?;
            child
        };
        let lease = if let Some(request) = request {
            request.lease
        } else {
            let lease = self.lease(saved.id, true)?;
            lease.try_lock().map_err(std::io::Error::from)?;
            lease
        };
        if self.path(saved.id).try_exists()? {
            return Err(Error::Refused("fork destination already exists".into()));
        }
        let mut log = Self::load_log(&stage, saved.id, lease)?;
        let (mut log, restored) = if let (Some(dest), Some(base)) = (
            &destination,
            saved
                .forked_from
                .as_ref()
                .and_then(|f| f.worktree_base.as_ref()),
        ) {
            let (dest, store, base) = (dest.clone(), self.clone(), base.clone());
            tokio::task::spawn_blocking(move || {
                let result = dest.verify(&base).and_then(|_| {
                    store
                        .restore_leased(&mut log, target, false, true, Some(&base.tree))
                        .and_then(worktree::no_conflicts)
                });
                (log, result)
            })
            .await
            .map_err(|e| Error::Closed(e.to_string()))?
        } else {
            (log, self.fork_restore(&source, target, false).await)
        };
        if let Err(error) = restored {
            // A local edit between the preflight and the native handshake may veto a
            // fresh fork. No restore started in that case, so the parent stays usable.
            if !staged
                && !stable
                && log.state.restoring.is_none()
                && source.0.lock().unwrap().state.restoring.is_none()
            {
                drop(log);
                fs::remove_file(&stage)?;
                #[cfg(unix)]
                File::open(&self.dir)?.sync_all()?;
            }
            return Err(error);
        }
        // Launch in the restored checkout so workspace-derived instructions are loaded there.
        let session = match Self::open_saved(&log.state, opts.before_chat_attach()?).await {
            Ok(s) => s,
            Err(e) => self.renew_empty_target(&mut log, &opts, e).await?,
        };
        let dest = destination.clone();
        let store = self.clone();
        let (log, verified) = tokio::task::spawn_blocking(move || {
            let result = (|| {
                if let Some(dest) = &dest {
                    let base = log
                        .state
                        .info
                        .forked_from
                        .as_ref()
                        .and_then(|f| f.worktree_base.as_ref())
                        .ok_or_else(|| Error::Protocol("missing staged fork baseline".into()))?;
                    dest.verify(base)?;
                }
                store.verify_restored(&log.state, target, dest.is_some())
            })();
            (log, result)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?;
        if let Err(e) = verified {
            session.close().await?;
            return Err(e);
        }
        let child = ChatSession::bind(session, log, self.clone(), opts)?;
        fs::rename(stage, self.path(saved.id))?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        workspace.finish_fork();
        Ok(child)
    }
    async fn fork_restore(
        &self,
        source: &Sink,
        target: CheckpointPoint,
        preview: bool,
    ) -> Result<()> {
        let store = self.clone();
        let sink = source.clone();
        let report = tokio::task::spawn_blocking(move || {
            let mut log = sink.0.lock().unwrap();
            store.restore_leased(&mut log, target, preview, true, None)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        worktree::no_conflicts(report)
    }
}
impl ChatSession {
    pub(crate) fn live_fork(&self) -> LiveFork {
        LiveFork {
            sender: Arc::downgrade(&self.sender),
            options: Arc::downgrade(&self.launch),
        }
    }
    /// Start a child from this idle chat, preserving its launch environment and the full parent history.
    pub async fn fork_at(&self, turn: Uuid) -> Result<ChatSession> {
        self.sender
            .store
            .fork_from(
                self.sender.sink.clone(),
                turn,
                self.sender
                    .store
                    .prepare_fork(&self.sender.sink, turn, self.launch.as_ref().clone(), false)
                    .await?,
                None,
            )
            .await
    }
    /// Submit a stable fork request through this controller, preserving its live native session.
    pub async fn fork_at_with_id(
        &self,
        turn: Uuid,
        child_id: Uuid,
        worktree: Option<PathBuf>,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<super::ChatHistory> {
        self.fork_at_with_policy(turn, child_id, worktree, ForkPolicy::Native, configure)
            .await
    }
    pub async fn fork_at_with_policy(
        &self,
        turn: Uuid,
        child_id: Uuid,
        worktree: Option<PathBuf>,
        policy: ForkPolicy,
        configure: impl FnOnce(&mut SessionOptions),
    ) -> Result<super::ChatHistory> {
        self.sender
            .store
            .fork_request(
                self.info().id,
                turn,
                child_id,
                worktree,
                policy,
                Some((self.sender.sink.clone(), self.launch.as_ref().clone())),
                configure,
            )
            .await
    }

    pub async fn fork_at_in_worktree(
        &self,
        turn: Uuid,
        checkout: impl Into<PathBuf>,
    ) -> Result<ChatSession> {
        self.sender
            .store
            .fork_from(
                self.sender.sink.clone(),
                turn,
                self.sender
                    .store
                    .prepare_fork(&self.sender.sink, turn, self.launch.as_ref().clone(), true)
                    .await?,
                Some(checkout.into()),
            )
            .await
    }
}

#[cfg(test)]
mod staged_route_tests {
    use super::*;

    #[test]
    fn staged_child_uses_its_launch_after_a_parent_provider_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("store")).unwrap();
        let parent = ChatInfo {
            antigravity_profile: None,
            provider_instance: None,
            imported: None,
            id: Uuid::new_v4(),
            workspace: dir.path().into(),
            worktree_id: None,
            runtime: "qwen".into(),
            protocol: Protocol::Qwen,
            native_id: "new-parent".into(),
            created_ms: 1,
            program: "qwen".into(),
            model: Some("current-model".into()),
            settings: Default::default(),
            unset_env: Default::default(),
            approvals: crate::ApprovalMode::Deny,
            forked_from: None,
        };
        let mut child = parent.clone();
        child.id = Uuid::new_v4();
        child.runtime = "codex".into();
        child.protocol = Protocol::Codex;
        child.program = "codex".into();
        child.native_id = "original-child".into();
        child.model = Some("selected-turn-model".into());
        child.settings.system_prompt = Some("original instructions".into());
        child.forked_from = Some(ForkOrigin {
            mode: ForkMode::Native,
            chat: parent.id,
            turn: Uuid::new_v4(),
            native_source: "previous-parent".into(),
            native_workspace: None,
            anchor: "original-turn".into(),
            checkpoint_store: parent.id,
            worktree_base: None,
        });
        log::header(
            &mut File::create(store.fork_stage(parent.id)).unwrap(),
            &child,
        )
        .unwrap();
        let mut options = parent.options();
        options.launch_args.push("--current-provider-only".into());
        options
            .env
            .insert("CURRENT_PROVIDER_KEY".into(), "not-for-child".into());
        let restored = source::options(&parent, &child, options);
        assert_eq!(restored.model, child.model);
        assert_eq!(
            restored.settings.system_prompt,
            child.settings.system_prompt
        );
        assert!(restored.launch_args.is_empty() && restored.env.is_empty());
        assert_eq!(
            store
                .pending_fork(parent.id)
                .unwrap()
                .unwrap()
                .native_source,
            "previous-parent"
        );
    }
}
