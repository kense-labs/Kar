//! One-shot local pipe to the selected Pi process. Only the endpoint appears in launch settings.
use std::{io, path::Path, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, task::JoinHandle};
use zeroize::Zeroizing;

pub(super) struct Transfer {
    writer: JoinHandle<()>,
    #[cfg(unix)]
    owner: Option<tokio::sync::oneshot::Sender<u32>>,
}

impl Drop for Transfer {
    fn drop(&mut self) {
        self.writer.abort();
    }
}

impl Transfer {
    pub(super) fn spawned(&mut self, pid: u32) {
        #[cfg(unix)]
        if let Some(owner) = self.owner.take() {
            let _ = owner.send(pid);
        }
        #[cfg(not(unix))]
        let _ = pid;
    }
}

#[cfg(unix)]
pub(super) fn prepare(
    cmd: &mut Command,
    directory: &Path,
    content: Vec<u8>,
    timeout: Duration,
) -> io::Result<Transfer> {
    let path = directory.join("config.sock");
    let listener = tokio::net::UnixListener::bind(&path)?;
    cmd.env("KAR_PI_MCP_PIPE", &path);
    let (owner, expected) = tokio::sync::oneshot::channel::<u32>();
    let content = Zeroizing::new(content);
    let writer = tokio::spawn(async move {
        let _ = tokio::time::timeout(timeout, async {
            let Ok(pid) = expected.await else {
                return Ok::<_, io::Error>(());
            };
            loop {
                let (mut client, _) = listener.accept().await?;
                if client
                    .peer_cred()?
                    .pid()
                    .and_then(|id| u32::try_from(id).ok())
                    == Some(pid)
                {
                    // This exact SDK host connected. No further client can obtain the payload.
                    drop(listener);
                    std::fs::remove_file(&path)?;
                    return client.write_all(&content).await;
                }
            }
        })
        .await;
    });
    Ok(Transfer {
        writer,
        owner: Some(owner),
    })
}

#[cfg(windows)]
pub(super) fn prepare(
    cmd: &mut Command,
    _: &Path,
    content: Vec<u8>,
    timeout: Duration,
) -> io::Result<Transfer> {
    use tokio::{io::AsyncReadExt, net::windows::named_pipe::ServerOptions};
    let name = format!(r"\\.\pipe\kar-pi-mcp-{}", uuid::Uuid::new_v4());
    let mut pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .create(&name)?;
    let key = uuid::Uuid::new_v4().simple().to_string();
    cmd.env("KAR_PI_MCP_PIPE", name).env("KAR_PI_MCP_KEY", &key);
    let content = Zeroizing::new(content);
    let writer = tokio::spawn(async move {
        let _ = tokio::time::timeout(timeout, async {
            loop {
                pipe.connect().await?;
                let mut proof = [0; 32];
                if matches!(
                    tokio::time::timeout(Duration::from_secs(1), pipe.read_exact(&mut proof)).await,
                    Ok(Ok(_))
                ) && proof == key.as_bytes()
                {
                    break;
                }
                pipe.disconnect()?;
            }
            pipe.write_u64_le(content.len() as u64).await?;
            pipe.write_all(&content).await?;
            // Windows discards unread bytes on disconnect. Wait until the client has consumed them.
            pipe.read_u8().await?;
            Ok::<_, io::Error>(())
        })
        .await;
    });
    Ok(Transfer { writer })
}

#[cfg(not(any(unix, windows)))]
pub(super) fn prepare(_: &mut Command, _: &Path, _: Vec<u8>, _: Duration) -> io::Result<Transfer> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Pi private configuration channel unavailable",
    ))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn only_the_selected_pid_can_read_the_one_time_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("config.sock");
        let payload = b"isolated private settings";
        let start = |owner| {
            let mut cmd = Command::new("unused");
            let mut transfer = prepare(
                &mut cmd,
                dir.path(),
                payload.to_vec(),
                Duration::from_secs(3),
            )
            .unwrap();
            transfer.spawned(owner);
            transfer
        };
        let other = start(u32::MAX);
        let mut wrong = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let mut bytes = Vec::new();
        assert_eq!(wrong.read_to_end(&mut bytes).await.unwrap(), 0);
        drop(other);
        std::fs::remove_file(&socket).unwrap();
        let _right = start(std::process::id());
        let mut allowed = tokio::net::UnixStream::connect(&socket).await.unwrap();
        allowed.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, payload);
        assert!(tokio::net::UnixStream::connect(socket).await.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
