//! Process-owned terminal shared by interactive native commands.
use crate::{Error, Result};
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use std::io::{self, Read, Write};
use tokio::sync::{mpsc, watch};

pub(crate) struct Terminal {
    killer: Box<dyn ChildKiller + Send + Sync>,
    #[cfg(unix)]
    pid: Option<u32>,
    pub output: mpsc::Receiver<io::Result<Vec<u8>>>,
    pub exit: watch::Receiver<Option<bool>>,
    input: mpsc::UnboundedSender<String>,
}
impl Terminal {
    pub(crate) fn open(cmd: CommandBuilder, size: PtySize) -> std::io::Result<Self> {
        let startup = |e| std::io::Error::other(format!("{e}"));
        let pair = native_pty_system().openpty(size).map_err(startup)?;
        let mut writer = pair.master.take_writer().map_err(startup)?;
        let mut reader = pair.master.try_clone_reader().map_err(startup)?;
        let mut child = pair.slave.spawn_command(cmd).map_err(startup)?;
        drop(pair.slave);
        #[cfg(unix)]
        let pid = child.process_id();
        let killer = child.clone_killer();
        let (tx, output) = mpsc::channel(16);
        let (input, mut replies) = mpsc::unbounded_channel::<String>();
        let errors = tx.clone();
        tokio::task::spawn_blocking(move || {
            while let Some(reply) = replies.blocking_recv() {
                if let Err(e) = writer
                    .write_all(reply.as_bytes())
                    .and_then(|_| writer.flush())
                {
                    let _ = errors.blocking_send(Err(e));
                    break;
                }
            }
        });
        tokio::task::spawn_blocking(move || {
            let mut buf = [0; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        let (done, exit) = watch::channel(None);
        tokio::task::spawn_blocking(move || {
            let ok = child.wait().is_ok_and(|s| s.success());
            done.send_replace(Some(ok));
            // Keep the master (and the Windows pseudoconsole) for the child lifetime.
            drop(pair.master);
        });
        Ok(Self {
            killer,
            #[cfg(unix)]
            pid,
            input,
            output,
            exit,
        })
    }
    pub fn write(&self, value: String) -> Result<()> {
        self.input
            .send(value)
            .map_err(|_| Error::Closed("Native terminal input closed".into()))
    }
}

impl Terminal {
    pub(crate) fn terminate(&mut self) {
        if self.exit.borrow().is_none() {
            #[cfg(unix)]
            if let Some(pid) = self.pid {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = self.killer.kill();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.terminate();
    }
}
