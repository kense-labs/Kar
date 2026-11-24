//! Scoped subprocesses used for private native management operations.
use std::{
    io,
    process::{ExitStatus, Output},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

pub(crate) const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;
/// An exited response carries the OS exit code, or null for termination by signal.
pub(crate) fn exit_body(status: &io::Result<ExitStatus>, reason: &str) -> serde_json::Value {
    match status {
        Ok(status) => serde_json::json!({"type":"exited", "code":status.code(), "reason":reason}),
        Err(error) => {
            serde_json::json!({"type":"rejected", "code":"error", "reason":error.to_string()})
        }
    }
}

pub(crate) struct OwnedProcess {
    #[cfg(windows)]
    child: Box<dyn process_wrap::tokio::TokioChildWrapper>,
    #[cfg(not(windows))]
    child: Child,
}

impl OwnedProcess {
    pub(crate) fn spawn(mut cmd: Command) -> io::Result<Self> {
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        let child = {
            use process_wrap::tokio::{JobObject, KillOnDrop, TokioCommandWrap};
            TokioCommandWrap::from(cmd)
                .wrap(KillOnDrop)
                .wrap(JobObject)
                .spawn()?
        };
        #[cfg(not(windows))]
        let child = cmd.spawn()?;
        Ok(Self { child })
    }

    pub(crate) fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub(crate) fn inner(&mut self) -> &mut Child {
        #[cfg(windows)]
        {
            self.child.inner_mut()
        }
        #[cfg(not(windows))]
        {
            &mut self.child
        }
    }

    /// An ACP terminal keeps its child handle until its pipes have closed.
    pub(crate) fn signal_terminal(&mut self, hard: bool) -> io::Result<bool> {
        #[cfg(unix)]
        {
            use nix::{
                errno::Errno,
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            let Some(pid) = self.id() else {
                return Ok(false);
            };
            let signal = if hard {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            };
            match killpg(Pid::from_raw(pid as i32), signal) {
                Ok(()) => Ok(true),
                Err(Errno::ESRCH) => Ok(false),
                Err(e) => Err(e.into()),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = hard;
            self.child.start_kill().map(|_| true)
        }
    }

    fn kill_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.child.start_kill();
    }

    pub(crate) async fn stop(&mut self) {
        let _ = self.stop_with_status().await;
    }

    pub(crate) async fn stop_with_status(&mut self) -> io::Result<ExitStatus> {
        self.kill_tree();
        self.inner().wait().await
    }

    /// Look for an exited leader without freeing its process group before cleanup.
    pub(crate) fn exited(&mut self) -> io::Result<bool> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
            let Some(pid) = self.child.id().and_then(|p| Pid::from_raw(p as i32)) else {
                return Ok(true);
            };
            let flags = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
            loop {
                match waitid(WaitId::Pid(pid), flags) {
                    Err(rustix::io::Errno::INTR) => continue,
                    result => return Ok(result?.is_some()),
                }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Ok(self.inner().try_wait()?.is_some())
        }
    }
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            while !self.exited()? {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            self.kill_tree();
        }
        self.inner().wait().await
    }

    /// Read both pipes before reaping the leader so timeout can also stop any
    /// descendant which inherited a pipe and kept it open.
    pub(crate) async fn output(&mut self) -> io::Result<Output> {
        let out = self.inner().stdout.take().expect("piped stdout");
        let err = self.inner().stderr.take().expect("piped stderr");
        let (stdout, stderr) = tokio::try_join!(bounded(out), bounded(err))?;
        Ok(Output {
            status: self.wait().await?,
            stdout,
            stderr,
        })
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        self.kill_tree();
    }
}

async fn bounded(input: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(io::Error::other("Native output exceeded its size limit"));
    }
    Ok(bytes)
}
