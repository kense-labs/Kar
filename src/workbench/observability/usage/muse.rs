//! A read-only MSP handshake discovers Muse's actual session directory.
use super::*;
use crate::process::OwnedProcess;
use std::{io, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
#[derive(Default)]
pub(super) struct Discovery(tokio::sync::Mutex<Option<(Instant, Option<PathBuf>)>>);
impl Discovery {
    pub async fn home(&self) -> Option<PathBuf> {
        if let Some(home) = std::env::var_os("MUSE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            return Some(home);
        }
        let mut cache = self.0.lock().await;
        if let Some((at, dir)) = &*cache
            && (dir.is_some() || at.elapsed() < Duration::from_secs(30))
        {
            return dir.clone();
        }
        let home = tokio::time::timeout(Duration::from_secs(5), probe())
            .await
            .ok()
            .and_then(|r| r.ok());
        *cache = Some((Instant::now(), home.clone()));
        home
    }
}
async fn probe() -> io::Result<PathBuf> {
    let mut cmd = Command::new(std::env::var_os("KAR_MUSE_BIN").unwrap_or_else(|| "muse".into()));
    cmd.args([
        "serve",
        "--disable-shell",
        "--disable-write",
        "--no-session-log",
    ])
    .env("MUSE_NO_AUTO_UPDATE", "1")
    .env_remove("KAR_WORKBENCH_TOKEN")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().eq_ignore_ascii_case("META_API_KEY") {
            cmd.env_remove(key);
        }
    }
    let mut owned = OwnedProcess::spawn(cmd)?;
    let mut stdin = owned
        .inner()
        .stdin
        .take()
        .ok_or(io::ErrorKind::BrokenPipe)?;
    let stdout = owned
        .inner()
        .stdout
        .take()
        .ok_or(io::ErrorKind::BrokenPipe)?;
    let mut reader = BufReader::new(stdout.take(1 << 20));
    let request = json!({"id":1,"jsonrpc":"2.0","method":"initialize","params":{"clientInfo":{"name":"kar","title":"Kar","version":env!("CARGO_PKG_VERSION")},"capabilities":{"requestedCapabilities":[]}}});
    stdin.write_all(format!("{request}\n").as_bytes()).await?;
    stdin.flush().await?;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let v: Value = serde_json::from_slice(&line)?;
        if v["jsonrpc"] != "2.0" || v["id"] != 1 {
            continue;
        }
        let r = &v["result"];
        if r["schema"]["version"] != 1 || !r["schema"]["fingerprint"].is_string() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let home = r["museHome"]
            .as_str()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or(io::ErrorKind::InvalidData)?;
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialized\"}\n")
            .await?;
        stdin.shutdown().await?;
        drop(stdin);
        if tokio::time::timeout(Duration::from_millis(200), owned.inner().wait())
            .await
            .is_err()
        {
            owned.stop().await;
        }
        return Ok(home);
    }
}
