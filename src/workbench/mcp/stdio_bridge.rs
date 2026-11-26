//! The provider's stdio launcher gets a socket pathname. Only descendants of the
//! selected native process can use it. The HTTP credential stays in the workbench.
use super::{SESSION, STATEFUL, VERSION, registry::Grant};
use crate::{McpServer, WorkbenchRegistration, private_dir::PrivateDir, resources::tree};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::{ffi::OsStr, io, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{Semaphore, watch},
};
use tokio_util::codec::{FramedRead, LinesCodec};

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
}

fn resolve_bridge_launcher_with(
    socket: &Path,
    bridge_env: Option<&OsStr>,
    current_exe: Option<&Path>,
    path_env: Option<&OsStr>,
    nc_fallback: &Path,
) -> crate::Result<(String, Vec<String>)> {
    let sock = socket.to_string_lossy().into_owned();
    let kar_bridge = |bin: &Path| {
        let resolved = bin.canonicalize().unwrap_or_else(|_| bin.to_path_buf());
        Ok((
            resolved.to_string_lossy().into_owned(),
            vec!["workbench-mcp-bridge".into(), sock.clone()],
        ))
    };

    // 1. Explicit KAR_WORKBENCH_BRIDGE_EXE override.
    if let Some(raw) = bridge_env.filter(|s| !s.is_empty()) {
        let candidate = Path::new(raw);
        if is_executable_file(candidate) {
            return kar_bridge(candidate);
        }
    }

    // 2. current_exe() if named "kar", or 3. sibling `kar` binary in parent / grandparent
    // (e.g. target/debug/deps/test_bin -> target/debug/kar).
    if let Some(exe) = current_exe {
        let canonical = exe.canonicalize().ok();
        let probe = canonical.as_deref().unwrap_or(exe);
        if (exe.file_name() == Some(OsStr::new("kar"))
            || probe.file_name() == Some(OsStr::new("kar")))
            && is_executable_file(probe)
        {
            return kar_bridge(probe);
        }
        for ancestor in [
            probe.parent(),
            probe.parent().and_then(Path::parent),
            exe.parent(),
            exe.parent().and_then(Path::parent),
        ]
        .into_iter()
        .flatten()
        {
            let sibling = ancestor.join("kar");
            if is_executable_file(&sibling) {
                return kar_bridge(&sibling);
            }
        }
    }

    // 4. `kar` on PATH.
    if let Some(paths) = path_env {
        for dir in std::env::split_paths(paths) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join("kar");
            if is_executable_file(&candidate) {
                return kar_bridge(&candidate);
            }
        }
    }

    // 5. macOS /usr/bin/nc -U <socket> fallback (Host::serve speaks raw newline-delimited JSON-RPC).
    if is_executable_file(nc_fallback) {
        return Ok((
            nc_fallback.to_string_lossy().into_owned(),
            vec!["-U".into(), sock],
        ));
    }

    Err(crate::Error::Unsupported(
        "Kar CLI bridge is unavailable".into(),
    ))
}

fn resolve_bridge_launcher(socket: &Path) -> crate::Result<(String, Vec<String>)> {
    let exe = std::env::current_exe().ok();
    resolve_bridge_launcher_with(
        socket,
        std::env::var_os("KAR_WORKBENCH_BRIDGE_EXE").as_deref(),
        exe.as_deref(),
        std::env::var_os("PATH").as_deref(),
        Path::new("/usr/bin/nc"),
    )
}

/// Async stdio-to-Unix-socket bridge usable directly by embedded hosts.
#[allow(dead_code)]
pub(crate) async fn bridge_stdio(socket: &Path) -> crate::Result<()> {
    let stream = UnixStream::connect(socket).await?;
    let (mut read_sock, mut write_sock) = stream.into_split();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut write_sock).await;
        let _ = write_sock.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    tokio::io::copy(&mut read_sock, &mut stdout).await?;
    stdout.flush().await?;
    Ok(())
}

pub(super) fn listen(
    addr: SocketAddr,
    token: String,
    grant: &Arc<Grant>,
    registration: &WorkbenchRegistration,
) -> crate::Result<McpServer> {
    let dir = PrivateDir::create(Path::new("/tmp"), "kar-mcp-bridge-", b"kar-stdio-mcp-v1")?;
    let path = dir.path().join("rpc");
    let (command, args) = resolve_bridge_launcher(&path)?;
    let listener = UnixListener::bind(&path)?;
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| io::Error::other("MCP client unavailable"))?;
    let host = Arc::new(Host {
        http,
        url: format!("http://{addr}/mcp"),
        token,
        grant: grant.clone(),
        root: registration.root.subscribe(),
    });
    tokio::spawn(async move {
        let _directory = dir;
        let limit = Arc::new(Semaphore::new(16));
        let expired = host.grant.clone().expired();
        tokio::pin!(expired);
        loop {
            tokio::select! {
                biased;
                _ = &mut expired => break,
                peer = listener.accept() => {
                    let Ok((stream, _)) = peer else {break};
                    let Ok(slot) = limit.clone().try_acquire_owned() else { continue };
                    let host = host.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        tokio::select! {
                            _ = host.grant.clone().expired() => {},
                            _ = host.serve(stream) => {},
                        }
                    });
                }
            }
        }
    });
    Ok(McpServer::Stdio {
        command,
        args,
        env: Default::default(),
    })
}

struct Host {
    http: Client,
    url: String,
    token: String,
    grant: Arc<Grant>,
    root: watch::Receiver<Option<tree::ProcessId>>,
}
impl Host {
    async fn serve(&self, stream: UnixStream) -> Option<()> {
        let cred = stream.peer_cred().ok()?;
        if cred.uid() != nix::unistd::geteuid().as_raw() {
            return None;
        }
        let pid = u32::try_from(cred.pid()?).ok()?;
        let peer = tree::identity(pid)?;
        let mut root = self.root.clone();
        let root = tokio::time::timeout(Duration::from_secs(10), root.wait_for(Option::is_some))
            .await
            .ok()?
            .ok()?
            .as_ref()
            .copied()?;
        let authorized =
            tokio::task::spawn_blocking(move || tree::descendants(root).contains(&peer))
                .await
                .ok()?;
        if !authorized {
            return None;
        }
        let (read, mut write) = stream.into_split();
        let mut lines = FramedRead::new(read, LinesCodec::new_with_max_length(1024 * 1024));
        let mut changes = self.grant.changes.subscribe();
        let mut session: Option<String> = None;
        loop {
            let frame = tokio::select! {
                line=lines.next()=>match line {Some(Ok(s))=>s,_=>break},
                res=changes.changed(), if session.is_some()=>{
                    if res.is_err(){break;}
                    write.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n").await.ok()?;
                    continue;
                }
            };
            let msg: Value = serde_json::from_str(&frame).ok()?;
            let mut req = self
                .http
                .post(&self.url)
                .bearer_auth(&self.token)
                .header("accept", "application/json, text/event-stream")
                .header("kar-tool-exposure", "deferred")
                .json(&msg);
            if let Some(id) = &session {
                req = req.header(SESSION, id).header(VERSION, STATEFUL);
            }
            let response = req.send().await.ok()?;
            let code = response.status();
            if let Some(id) = response.headers().get(SESSION) {
                session = Some(id.to_str().ok()?.to_owned());
            }
            if code == reqwest::StatusCode::ACCEPTED {
                continue;
            }
            let value=response.json::<Value>().await.unwrap_or_else(|_|json!({
                "jsonrpc":"2.0","id":msg["id"],"error":{"code":-32000,"message":"Kar workbench request failed"}
            }));
            let mut bytes = serde_json::to_vec(&value).ok()?;
            bytes.push(b'\n');
            write.write_all(&bytes).await.ok()?;
            if code == reqwest::StatusCode::UNAUTHORIZED {
                break;
            }
        }
        if let Some(id) = session {
            let _ = self
                .http
                .delete(&self.url)
                .bearer_auth(&self.token)
                .header(SESSION, id)
                .header(VERSION, STATEFUL)
                .timeout(Duration::from_secs(2))
                .send()
                .await;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn make_exec(path: &Path) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn bridge_resolution_cascades_env_sibling_path_and_nc_fallback() -> crate::Result<()> {
        let tmp = tempfile::tempdir()?;
        let sock = tmp.path().join("rpc");
        let absent_nc = tmp.path().join("no-nc");

        // 1. Explicit KAR_WORKBENCH_BRIDGE_EXE wins even when current_exe is an embedded host.
        let custom = tmp.path().join("custom/my-bridge");
        make_exec(&custom);
        let embedded = tmp.path().join("host/embedded-app");
        make_exec(&embedded);
        let (cmd, args) = resolve_bridge_launcher_with(
            &sock,
            Some(custom.as_os_str()),
            Some(&embedded),
            None,
            &absent_nc,
        )?;
        assert_eq!(Path::new(&cmd), custom.canonicalize()?);
        assert_eq!(args[0], "workbench-mcp-bridge");

        // 2. Test binary in target/debug/deps/test_bin finds sibling target/debug/kar!
        let debug_kar = tmp.path().join("target/debug/kar");
        let test_bin = tmp.path().join("target/debug/deps/workbench_test-1234");
        make_exec(&debug_kar);
        make_exec(&test_bin);
        let (cmd, args) =
            resolve_bridge_launcher_with(&sock, None, Some(&test_bin), None, &absent_nc)?;
        assert_eq!(Path::new(&cmd), debug_kar.canonicalize()?);
        assert_eq!(args[0], "workbench-mcp-bridge");

        // 3. `kar` on PATH when current_exe has no sibling `kar`.
        let path_dir = tmp.path().join("usr-bin");
        let path_kar = path_dir.join("kar");
        make_exec(&path_kar);
        let (cmd, args) = resolve_bridge_launcher_with(
            &sock,
            None,
            Some(&embedded),
            Some(path_dir.as_os_str()),
            &absent_nc,
        )?;
        assert_eq!(Path::new(&cmd), path_kar.canonicalize()?);
        assert_eq!(args[0], "workbench-mcp-bridge");

        // 4. `/usr/bin/nc -U <socket>` fallback when no `kar` binary exists anywhere.
        let mock_nc = tmp.path().join("nc");
        make_exec(&mock_nc);
        let (cmd, args) =
            resolve_bridge_launcher_with(&sock, None, Some(&embedded), None, &mock_nc)?;
        assert_eq!(Path::new(&cmd), mock_nc);
        assert_eq!(
            args,
            vec!["-U".to_owned(), sock.to_string_lossy().into_owned()]
        );
        Ok(())
    }
}
