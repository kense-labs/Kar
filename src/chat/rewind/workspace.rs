use super::{ContextRewind, PreparedRewind};
use crate::chat::{ChatStore, CheckpointPoint, RollbackReport, log};
use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn require_clean(report: &RollbackReport) -> Result<()> {
    if report.conflicts.is_empty() {
        return Ok(());
    }
    let paths = report
        .conflicts
        .iter()
        .map(|f| f.path.display().to_string())
        .collect::<Vec<_>>();
    Err(Error::Refused(format!(
        "local changes conflict with workspace rewind: {}",
        paths.join(", ")
    )))
}

impl ChatStore {
    pub(in crate::chat) fn rewind_reservation(&self, chat: Uuid, id: Uuid) -> PathBuf {
        self.dir.join(format!("{chat}.{id}.karrewind"))
    }

    pub(super) fn preflight_workspace_rewind(&self, log: &mut log::Log, turn: Uuid) -> Result<()> {
        let target = CheckpointPoint {
            turn,
            before: false,
        };
        let report = self.rollback_locked(log, target, true)?;
        require_clean(&report)
    }

    pub(super) fn finish_workspace_rewind(
        &self,
        log: &mut log::Log,
        pending: PreparedRewind,
    ) -> Result<ContextRewind> {
        let receipt = pending.receipt;
        let reservation = self.rewind_reservation(receipt.chat, receipt.id);
        let lease = Self::workspace_lease_for(&log.state.info.workspace, Some(&reservation))?;
        let _pin = log.state.info.pin_worktree()?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&reservation)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        lease.reserve_rewind(&reservation)?;
        let target = CheckpointPoint {
            turn: receipt.turn,
            before: false,
        };
        let report = self.restore_leased(log, target, false, false, None)?;
        require_clean(&report)?;
        // All repository changes and the visible timeline are now committed. A reader
        // can clean up a reservation after a crash at this exact point.
        std::fs::remove_file(&reservation)?;
        lease.finish_fork();
        Ok(log
            .state
            .rewinds
            .last()
            .expect("committed workspace rewind")
            .clone())
    }
}

/// Workspace leases are shared by chats in different stores. Consult the durable
/// decision before clearing a reservation left by a dead controller.
pub(in crate::chat) fn completed_reservation(path: &Path) -> Result<bool> {
    let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
        return Ok(false);
    };
    let Some((chat, command)) = name.split_once('.') else {
        return Ok(false);
    };
    let (Ok(chat), Ok(command)) = (Uuid::parse_str(chat), Uuid::parse_str(command)) else {
        return Ok(false);
    };
    let journal = path.with_file_name(format!("{chat}.karchat"));
    let (state, _) = log::read(&mut File::open(journal)?, chat)?;
    Ok(state
        .rewinds
        .iter()
        .any(|r| r.id == command && r.restore_files))
}
