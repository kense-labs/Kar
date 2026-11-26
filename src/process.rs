//! Scoped subprocesses used for private native management operations.
pub(crate) mod diagnostic;
use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
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
        self.output_limited(OUTPUT_LIMIT).await
    }

    pub(crate) async fn output_limited(&mut self, stdout_limit: u64) -> io::Result<Output> {
        let out = self.inner().stdout.take().expect("piped stdout");
        let err = self.inner().stderr.take().expect("piped stderr");
        let (stdout, stderr) =
            tokio::try_join!(bounded(out, stdout_limit), bounded(err, OUTPUT_LIMIT))?;
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

async fn bounded(input: impl AsyncRead + Unpin, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input.take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("Native output exceeded its size limit"));
    }
    Ok(bytes)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (meta.permissions().mode() & 0o111) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_sh_with(fixed: &[&Path], path_var: Option<&OsStr>) -> PathBuf {
    for candidate in fixed {
        if is_executable_file(candidate) {
            return (*candidate).to_path_buf();
        }
    }
    if let Some(paths) = path_var {
        for dir in std::env::split_paths(paths) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join("sh");
            if is_executable_file(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from("/bin/sh")
}

#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn find_sh() -> PathBuf {
    find_sh_with(
        &[Path::new("/bin/sh"), Path::new("/usr/bin/sh")],
        std::env::var_os("PATH").as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn find_sh_checks_fixed_locations_then_path_for_executable_sh() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let non_exec = dir.path().join("non_exec_sh");
        std::fs::write(&non_exec, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).unwrap();

        let bin_dir = dir.path().join("custom_bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let custom_sh = bin_dir.join("sh");
        std::fs::write(&custom_sh, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&custom_sh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let missing = dir.path().join("missing_sh");
        let path_env = std::env::join_paths([dir.path(), bin_dir.as_path()]).unwrap();
        assert_eq!(
            find_sh_with(&[&missing, &non_exec], Some(&path_env)),
            custom_sh
        );
        assert_eq!(find_sh_with(&[&custom_sh, &missing], None), custom_sh);
        assert!(find_sh().is_absolute());
    }
}
