use super::{ChatInfo, ChatSession, ChatStore, CheckpointPoint, Sink, TurnStatus, log};
use crate::{Error, Protocol, Result, Session, SessionOptions};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// The parent chat and the provider-issued anchor copied into a new native session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForkOrigin {
    pub chat: Uuid,
    pub turn: Uuid,
    pub native_source: String,
    pub anchor: String,
    pub checkpoint_store: Uuid,
}

impl ChatStore {
    fn fork_stage(&self, source: Uuid) -> PathBuf {
        self.dir.join(format!("{source}.forking"))
    }
    pub(super) fn pending_fork(&self, source: Uuid) -> Result<Option<ForkOrigin>> {
        let file = match File::open(self.fork_stage(source)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        log::staged_info(file)?
            .forked_from
            .filter(|f| f.chat == source)
            .map(Some)
            .ok_or_else(|| Error::Protocol("invalid staged fork".into()))
    }
    pub(super) fn require_no_pending_fork(&self, source: Uuid) -> Result<()> {
        if let Some(f) = self.pending_fork(source)? {
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
        let mut opts = log.state.info.options();
        configure(&mut opts);
        self.fork_from(Sink(Arc::new(Mutex::new(log))), turn, opts)
            .await
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
        Ok(session)
    }
    async fn fork_from(
        &self,
        source: Sink,
        turn: Uuid,
        opts: SessionOptions,
    ) -> Result<ChatSession> {
        crate::stream::retention::check_limit(opts.retained_bytes)?;
        let store = self.clone();
        tokio::spawn(async move { store.run_fork(source, turn, opts).await })
            .await
            .map_err(|e| Error::Closed(e.to_string()))?
    }
    async fn run_fork(
        &self,
        source: Sink,
        turn: Uuid,
        mut opts: SessionOptions,
    ) -> Result<ChatSession> {
        let parent = source.0.lock().unwrap().state.info.clone();
        let _pin = parent.pin_worktree()?;
        opts.cwd = parent.workspace.clone();
        opts.resume = None;
        let opencode = parent.protocol == Protocol::Acp && parent.runtime == "opencode";
        if !opencode
            && !matches!(
                parent.protocol,
                Protocol::Codex | Protocol::Claude | Protocol::Dsh | Protocol::Pi
            )
        {
            return Err(Error::Unsupported(format!(
                "{} does not support native forks through a selected turn",
                parent.runtime
            )));
        }
        let stage = self.fork_stage(parent.id);
        let workspace = Self::workspace_lease_for(&parent.workspace, Some(&stage))?;
        let target = CheckpointPoint {
            turn,
            before: false,
        };
        let staged = stage.try_exists()?;
        let mut prefix = {
            let log = source.0.lock().unwrap();
            log.healthy()?;
            if log.state.active().is_some()
                || log.state.compacting()
                || log.state.waiting_steering()
                || log.state.observing_notifications()
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
        let cut = prefix.last_mut().expect("selected turn is in prefix");
        let native_source = cut
            .items
            .first()
            .and_then(|i| i.source.session_id.clone())
            .ok_or_else(|| Error::Protocol("turn lacks its native session identity".into()))?;
        let saved = if staged {
            let info = log::staged_info(File::open(&stage)?)?;
            if !info.forked_from.as_ref().is_some_and(|f| {
                f.chat == parent.id
                    && f.turn == turn
                    && (cut.fork_anchor.as_ref() == Some(&f.anchor)
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
            self.fork_restore(&source, target, true).await?;
            if cut.fork_anchor.is_none()
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
            let (branch, anchor) = if opencode && cut.fork_anchor.is_none() {
                use crate::providers::opencode_fork::{self, Target};
                opencode_fork::open(&rt, opts.clone(), &native_source, Target::Turn(cut)).await?
            } else {
                let anchor = cut.fork_anchor.clone().ok_or_else(|| {
                    Error::Unsupported("selected turn has no native fork anchor".into())
                })?;
                let session = rt
                    .fork_selected(&native_source, Some(&anchor), opts.clone(), None)
                    .await?;
                (session, anchor)
            };
            cut.fork_anchor = Some(anchor.clone());
            let origin = ForkOrigin {
                chat: parent.id,
                turn,
                native_source,
                anchor,
                checkpoint_store: parent.checkpoint_store(),
            };
            let child = ChatInfo {
                id: Uuid::new_v4(),
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
            log::inherit(tmp.as_file_mut(), prefix)?;
            workspace.reserve_fork(&stage)?;
            tmp.persist_noclobber(&stage).map_err(|e| e.error)?;
            #[cfg(unix)]
            File::open(&self.dir)?.sync_all()?;
            branch.close().await?;
            child
        };
        let lease = self.lease(saved.id, true)?;
        lease.try_lock().map_err(std::io::Error::from)?;
        if self.path(saved.id).try_exists()? {
            return Err(Error::Refused("fork destination already exists".into()));
        }
        let log = Self::load_log(&stage, saved.id, lease)?;
        if let Err(error) = self.fork_restore(&source, target, false).await {
            // A local edit between the preflight and the native handshake may veto a
            // fresh fork. No restore started in that case, so the parent stays usable.
            if !staged && source.0.lock().unwrap().state.restoring.is_none() {
                drop(log);
                fs::remove_file(&stage)?;
                #[cfg(unix)]
                File::open(&self.dir)?.sync_all()?;
            }
            return Err(error);
        }
        // Launch in the restored checkout so workspace-derived instructions are loaded there.
        let session = Self::open_saved(&log.state, opts.before_chat_attach()?).await?;
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
            store.restore_leased(&mut log, target, preview, true)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        if !report.conflicts.is_empty() {
            return Err(Error::Refused(format!(
                "workspace conflicts: {}",
                report
                    .conflicts
                    .iter()
                    .map(|f| f.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(())
    }
}
impl ChatSession {
    /// Start a child from this idle chat, preserving its launch environment and the full parent history.
    pub async fn fork_at(&self, turn: Uuid) -> Result<ChatSession> {
        self.sender
            .store
            .fork_from(self.sender.sink.clone(), turn, self.launch.clone())
            .await
    }
}
