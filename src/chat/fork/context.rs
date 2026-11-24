//! A workbench thread transfer copies conversation history without changing the shared checkout.
use super::*;

impl ChatStore {
    pub(crate) async fn fork_context(
        &self,
        source: Uuid,
        turn: Uuid,
        child: Uuid,
        policy: ForkPolicy,
    ) -> Result<super::super::ChatInfo> {
        if source == child {
            return Err(Error::Refused("cannot fork onto the source chat".into()));
        }
        let lease = self.lease(child, true)?;
        lease.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => Error::Busy,
            fs::TryLockError::Error(e) => e.into(),
        })?;
        if self.path(child).try_exists()? {
            let info = self.initial_info(child)?;
            let origin = info
                .forked_from
                .as_ref()
                .filter(|f| f.context_only && f.chat == source && f.turn == turn)
                .ok_or_else(|| Error::Refused("child ID is already in use".into()))?;
            policy.check(origin.mode)?;
            return Ok(info);
        }
        if self.fork_claimed(child)? {
            return Err(Error::Refused(
                "this ID has an existing workspace fork request".into(),
            ));
        }
        let state = self.read_state(File::open(self.path(source))?, source)?;
        let launch = source::Launch::prepare(self, &state, turn, state.info.options())?;
        if launch.staged.is_some() {
            return Err(Error::Busy);
        }
        let end = state
            .turns
            .iter()
            .position(|r| r.id == turn && r.status == TurnStatus::Completed)
            .ok_or_else(|| {
                Error::Refused("context transfer requires a completed source turn".into())
            })?;
        let mut prefix = state.turns[..=end].to_vec();
        trim_followups(&mut prefix, launch.owner.protocol);
        let cut = prefix.last_mut().unwrap();
        let mut opts = launch.options;
        opts.cwd = state.info.workspace.clone();
        opts.resume = None;
        opts.unset_env.insert("KAR_WORKBENCH_TOKEN".into());
        opts.mcp_servers.remove("kar_workbench");
        let same = launch.owner.workspace == opts.cwd;
        let live_safe = state.active().is_none() || launch.owner.protocol == Protocol::Codex;
        let native = (launch.owner.protocol == Protocol::Codex || same)
            && live_safe
            && (matches!(
                launch.owner.protocol,
                Protocol::Claude | Protocol::Codex | Protocol::Dsh | Protocol::Pi
            ) || launch.owner.runtime == "opencode");
        let owner = &launch.owner;
        let plan = CutPlan::prepare(self, owner, &mut opts, cut, native, policy).await?;
        let _pin = state.info.pin_worktree()?;
        // Persist the immutable operation before crossing the native process boundary.
        // An indeterminate remote fork is reported on retry instead of starting another.
        let request = self.dir.join(format!("{child}.context-fork"));
        let mut marker = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(marker.as_file_mut(), &(source, turn, policy))?;
        marker.as_file().sync_all()?;
        marker.persist_noclobber(&request).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Refused("context fork has no confirmed native result".into())
            } else {
                e.error.into()
            }
        })?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        let (branch, anchor) = plan.open(&owner.native_id, &opts, cut, !same).await?;
        let mode = if anchor.is_some() {
            ForkMode::Native
        } else {
            ForkMode::PortableContext
        };
        let info = ChatInfo {
            id: child,
            native_id: branch.id().into(),
            created_ms: super::super::now(),
            workspace: opts.cwd.clone(),
            worktree_id: state.info.worktree_id,
            imported: None,
            process_only: !branch.capabilities().resume,
            forked_from: Some(ForkOrigin {
                context_only: true,
                mode,
                chat: source,
                turn,
                native_source: owner.native_id.clone(),
                native_workspace: Some(owner.workspace.clone()),
                anchor: anchor.unwrap_or_default(),
                checkpoint_store: state.info.checkpoint_store(),
                worktree_base: None,
            }),
            ..launch.owner
        };
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        log::header(tmp.as_file_mut(), &info)?;
        let handoff = (mode == ForkMode::PortableContext)
            .then(|| super::super::Handoff::for_fork(&info, &prefix))
            .transpose()?;
        log::inherit(tmp.as_file_mut(), prefix)?;
        if let Some(handoff) = handoff {
            log::prepare_portable_fork(tmp.as_file_mut(), &info, handoff)?;
        }
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(self.path(child))
            .map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        branch.close().await?;
        Ok(info)
    }
}
