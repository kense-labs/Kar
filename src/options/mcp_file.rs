//! Single-read MCP configuration for runtimes that accept a file path at launch.
use crate::{Result, SessionOptions, private_dir::PrivateDir};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

const SIGNATURE: &[u8] = b"kar-mcp-config-v1";
const FILENAME: &str = "mcp.json";

pub(crate) struct Config {
    _directory: PrivateDir,
    path: PathBuf,
    #[cfg(unix)]
    writer: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl Config {
    pub(crate) async fn finish(self, timeout: std::time::Duration) -> Result<()> {
        #[cfg(unix)]
        {
            let mut config = self;
            if let Some(task) = &mut config.writer {
                tokio::time::timeout(timeout, task)
                    .await
                    .map_err(|_| {
                        crate::Error::Timeout("MCP configuration was not consumed".into())
                    })?
                    .map_err(|_| io::Error::other("MCP configuration writer stopped"))??;
                config.writer.take();
            }
        }
        #[cfg(not(unix))]
        let _ = timeout;
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn release_reader(path: &Path) {
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_fifo())
    {
        // A runtime may have blocked in open(2) before the previous host died.
        let _ = OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
            .open(path);
    }
}

#[cfg(unix)]
impl Drop for Config {
    fn drop(&mut self) {
        if let Some(task) = self.writer.take() {
            task.abort();
            release_reader(&self.path);
        }
    }
}

#[cfg(unix)]
async fn feed(path: &Path, payload: &[u8]) -> io::Result<()> {
    use std::time::Duration;
    use tokio::{io::AsyncWriteExt, net::unix::pipe};
    let mut wait = Duration::from_millis(5);
    loop {
        match pipe::OpenOptions::new().open_sender(path) {
            Ok(mut sender) => {
                // Only this reader has the pathname open. Future opens must fail.
                std::fs::remove_file(path)?;
                return sender.write_all(payload).await;
            }
            Err(err) if err.raw_os_error() == Some(nix::libc::ENXIO) => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(100));
            }
            Err(err) => return Err(err),
        }
    }
}

fn write_private_file(path: &Path, payload: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(payload)
}

fn sweep(root: &Path, prefix: &str) -> Result<()> {
    PrivateDir::sweep_with(root, prefix, SIGNATURE, |_dir| {
        #[cfg(unix)]
        release_reader(&_dir.join(FILENAME));
    })
}

pub(crate) fn prepare(prefix: &str, options: &SessionOptions) -> Result<Option<Config>> {
    let root = std::env::temp_dir();
    sweep(&root, prefix)?;
    if options.mcp_servers.is_empty() {
        return Ok(None);
    }
    let servers: Result<BTreeMap<_, _>> = options
        .mcp_servers
        .iter()
        .map(|(name, server)| Ok((name, server.vendor_config()?)))
        .collect();
    let payload = Zeroizing::new(serde_json::to_vec(
        &serde_json::json!({"mcpServers":servers?}),
    )?);
    let directory = PrivateDir::create_after_sweep(&root, prefix, SIGNATURE)?;
    let path = directory.path().join(FILENAME);
    #[cfg(unix)]
    if nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).is_ok() {
        let fifo = path.clone();
        let writer = tokio::spawn(async move { feed(&fifo, &payload).await });
        return Ok(Some(Config {
            _directory: directory,
            path,
            writer: Some(writer),
        }));
    }
    write_private_file(&path, &payload)?;
    Ok(Some(Config {
        _directory: directory,
        path,
        #[cfg(unix)]
        writer: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_file_is_exclusive_and_plain_launch_reclaims_only_abandoned_configs() {
        let root = tempfile::tempdir().unwrap();
        let prefix = "fixture-mcp-";
        let live = PrivateDir::create(root.path(), prefix, SIGNATURE).unwrap();
        let path = live.path().join(FILENAME);
        write_private_file(&path, b"isolated").unwrap();
        assert_eq!(
            write_private_file(&path, b"replacement")
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(
                live.path().metadata().unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let stale = root.path().join("fixture-mcp-abandoned");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join(".owner"), SIGNATURE).unwrap();
        write_private_file(&stale.join(FILENAME), b"old").unwrap();
        let unrelated = root.path().join("fixture-mcp-unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join(".owner"), b"another application").unwrap();
        sweep(root.path(), prefix).unwrap();
        assert!(!stale.exists());
        assert!(unrelated.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"isolated");
        drop(live);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unconsumed_configuration_fails_open_and_releases_the_writer() {
        let mut options = SessionOptions::new(".");
        options.mcp_servers.insert(
            "fixture".into(),
            crate::McpServer::Stdio {
                command: "unused".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        );
        let cfg = prepare("kar-test-unread-mcp-", &options).unwrap().unwrap();
        let path = cfg.path().to_owned();
        let error = cfg
            .finish(std::time::Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(matches!(error, crate::Error::Timeout(_)));
        assert!(!path.parent().unwrap().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reader_waits_for_large_single_use_fifo_and_abandoned_reader_is_released() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        use std::{os::unix::fs::PermissionsExt, time::Duration};
        let root = tempfile::tempdir().unwrap();
        let data = vec![21u8; 1024 * 1024];
        for abandoned in [false, true] {
            let dir = root.path().join("fixture-mcp-owner");
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join(".owner"), SIGNATURE).unwrap();
            let path = dir.join(FILENAME);
            mkfifo(&path, Mode::from_bits_truncate(0o600)).unwrap();
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            let (entered, opened) = tokio::sync::oneshot::channel();
            let (replied, done) = tokio::sync::oneshot::channel();
            let fifo = path.clone();
            std::thread::spawn(move || {
                entered.send(()).unwrap();
                let _ = replied.send(std::fs::read(fifo));
            });
            opened.await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            if abandoned {
                sweep(root.path(), "fixture-mcp-").unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(5), feed(&path, &data))
                    .await
                    .unwrap()
                    .unwrap();
            }
            let received = tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if abandoned {
                assert!(received.is_empty());
            } else {
                assert_eq!(received, data);
                std::fs::remove_dir_all(&dir).unwrap();
            }
            assert!(!path.exists());
        }
    }
}
