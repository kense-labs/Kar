//! Tracked worktree initialization. Each attempt has its own terminal and durable exit status.
use super::*;
impl Workbench {
    pub(in crate::workbench) fn launch_setup_terminal(
        &self,
        project: Uuid,
        cwd: &Path,
        id: Uuid,
        command: &str,
    ) -> Result<()> {
        self.terminal_command(project, id, cwd.to_owned(), 120, 30, Some(command))
            .map(|_| ())
    }
    pub(in crate::workbench) async fn wait_setup_terminal(
        &self,
        project: Uuid,
        id: Uuid,
    ) -> Result<()> {
        let terminal = self.0.terminal_preview.find(project, id)?;
        let mut rx = terminal.changed.subscribe();
        loop {
            match terminal.snapshot().status {
                Status::Starting | Status::Running | Status::Closing => {}
                Status::Exited {
                    code: 0,
                    signal: None,
                } => return Ok(()),
                Status::Exited { code, signal } => {
                    return Err(Error::Refused(format!(
                        "setup exited with {code} (signal {signal:?})"
                    )));
                }
                Status::Interrupted { .. } => {
                    return Err(Error::Closed(
                        "Kar restarted before the setup exit was recorded".into(),
                    ));
                }
                Status::Error { reason } => return Err(Error::Closed(reason)),
            }
            rx.changed()
                .await
                .map_err(|_| Error::Closed("setup terminal closed".into()))?;
        }
    }
    pub(in crate::workbench) fn setup_still_running(
        &self,
        project: Uuid,
        id: Uuid,
    ) -> Result<bool> {
        match self.0.terminal_preview.find(project, id) {
            Ok(terminal) => Ok(matches!(
                terminal.snapshot().status,
                Status::Starting
                    | Status::Running
                    | Status::Closing
                    | Status::Interrupted {
                        process_alive: Some(true)
                    }
            )),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}
