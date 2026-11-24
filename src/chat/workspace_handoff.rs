use super::{ChatInfo, ChatStore, ChatUpdate, Handoff, HandoffRequest, ProviderSession};
use crate::{Error, Result};
use std::path::Path;
use uuid::Uuid;

impl ChatStore {
    /// Publish a new cwd, native session and pending context in one journal entry. A worker
    /// can die after opening an empty native session, but no user input is sent until publication.
    pub(crate) async fn move_workspace(
        &self,
        chat: Uuid,
        command: Uuid,
        from: &Path,
        to: &Path,
        checkout: Option<Uuid>,
        configure: impl FnOnce(&mut crate::SessionOptions),
    ) -> Result<Option<super::ChatSession>> {
        let mut log = self.open_controller(chat).await?;
        if log.state.info.workspace == to
            && log
                .state
                .handoffs
                .iter()
                .any(|h| h.request.id == command && h.source_workspace.as_deref() == Some(from))
        {
            return Ok(None);
        }
        if log.state.info.workspace != from || log.state.info.worktree_id.is_some() {
            return Err(Error::Refused(
                "chat no longer occupies its original workspace".into(),
            ));
        }
        self.require_no_pending_fork(chat)?;
        if log.state.needs_controller()
            || log.state.pending_handoff().is_some()
            || log.state.restoring.is_some()
        {
            return Err(Error::Busy);
        }
        let info = &log.state.info;
        if to.canonicalize()? != to {
            return Err(Error::Refused("checkout path changed".into()));
        }
        let _pin = crate::workbench_git::pin_saved_checkout(to, checkout)?;
        let mut opts = info.options();
        opts.cwd = to.into();
        opts.pi_resume_file = None;
        opts.remove_env("KAR_WORKBENCH_TOKEN")?;
        // Additional workspace roots were granted to the original placement.
        opts.settings.additional_directories.clear();
        configure(&mut opts);
        let runtime = info.launch_runtime(self, &mut opts).await?;
        let native = runtime.open(opts.before_chat_attach()?).await?;
        if !native.capabilities().resume || native.id() == info.native_id {
            native.close().await?;
            return Err(Error::Refused(
                "provider did not allocate a new recoverable native session".into(),
            ));
        }
        let target_info = ChatInfo {
            workspace: to.into(),
            worktree_id: checkout,
            native_id: native.id().into(),
            settings: opts.settings.clone(),
            ..info.clone()
        };
        let instance = info.active_instance().cloned();
        let mut h = Handoff::prepare(
            HandoffRequest {
                id: command,
                runtime: info.runtime.clone(),
                model: info.model.clone(),
                instance_id: instance.as_ref().map(|i| i.id.clone()),
                budget_bytes: 65536,
            },
            instance,
            info,
            &log.state.turns,
            &[],
        )?;
        h.source_workspace = Some(from.into());
        log.commit(ChatUpdate::WorkspaceHandoff {
            workspace: to.into(),
            worktree_id: checkout,
            handoff: Box::new(h),
            target: Box::new(ProviderSession::current(&target_info)),
        })?;
        Ok(Some(super::ChatSession::bind(
            native,
            log,
            self.clone(),
            opts,
        )?))
    }
}
