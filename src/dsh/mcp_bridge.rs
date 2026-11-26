//! Per-session Unix-socket MCP bridge for DeepSeek Harness (`--patch`).
//!
//! DSH's `--patch` plugin loader only accepts credential-free stdio MCP server
//! definitions. For HTTP MCP servers (`McpServer::Http` / `McpServer::HostedHttp`)
//! and stdio MCP servers carrying secret `env` variables, Kar binds a session-scoped
//! `UnixListener` inside a `0700` `PrivateDir` and writes a credential-free stdio
//! launcher entry (`kar dsh-mcp-bridge <socket>` or `/usr/bin/nc -U <socket>`) into
//! `--patch`. Credentials never leave Kar's parent process memory or child env.

use crate::{Error, McpServer, Result, private_dir::PrivateDir};
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
};
use tokio_util::codec::{FramedRead, LinesCodec};

const MCP_SESSION_HEADER: &str = "mcp-session-id";
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Conservative ceiling below macOS `sockaddr_un.sun_path` (104 bytes including NUL).
const MAX_UNIX_SOCKET_PATH_LEN: usize = 96;

/// Session-scoped owner for active DSH MCP bridge listeners and any fallback `/tmp` socket directory.
pub(crate) struct McpBridgeSet {
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    _short_dir: Option<PrivateDir>,
}

impl Default for McpBridgeSet {
    fn default() -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            shutdown_tx,
            tasks: Vec::new(),
            _short_dir: None,
        }
    }
}

impl Drop for McpBridgeSet {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub(crate) fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || crate::options::instructions::reserved_server(name)
    {
        return Err(Error::Unsupported("invalid DSH MCP server name".into()));
    }
    Ok(())
}

fn validate_env_map(env: &BTreeMap<String, String>) -> Result<()> {
    for (k, v) in env {
        if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
            return Err(Error::Unsupported(
                "invalid environment variable in DSH stdio MCP server".into(),
            ));
        }
    }
    Ok(())
}

fn build_header_map(headers: &BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::Unsupported(format!("invalid HTTP MCP header name {name:?}")))?;
        let header_value = HeaderValue::from_str(value).map_err(|_| {
            Error::Unsupported(format!("invalid HTTP MCP header value for {name:?}"))
        })?;
        map.insert(header_name, header_value);
    }
    Ok(map)
}

fn validate_http_url(url: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).map_err(|_| {
        Error::Unsupported("DSH HTTP MCP server requires a valid http:// or https:// URL".into())
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::Unsupported(
            "DSH HTTP MCP server requires an http:// or https:// URL".into(),
        ));
    }
    Ok(parsed)
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn resolve_bridge_launcher_with(
    socket: &Path,
    bridge_env: Option<&OsStr>,
    current_exe: Option<&Path>,
    path_env: Option<&OsStr>,
    nc_fallback: &Path,
) -> Result<(String, Vec<String>)> {
    let sock = socket.to_string_lossy().into_owned();
    let kar_bridge = |bin: &Path| {
        let resolved = bin.canonicalize().unwrap_or_else(|_| bin.to_path_buf());
        Ok((
            resolved.to_string_lossy().into_owned(),
            vec!["dsh-mcp-bridge".into(), sock.clone()],
        ))
    };

    // 1. Explicit KAR_WORKBENCH_BRIDGE_EXE / KAR_DSH_BRIDGE_EXE override.
    if let Some(raw) = bridge_env.filter(|s| !s.is_empty()) {
        let candidate = Path::new(raw);
        if is_executable_file(candidate) {
            return kar_bridge(candidate);
        }
    }

    // 2. Current process if it is already the `kar` CLI binary.
    if let Some(exe) = current_exe {
        let canonical = exe.canonicalize().ok();
        let probe = canonical.as_deref().unwrap_or(exe);
        if (exe.file_name() == Some(OsStr::new("kar"))
            || probe.file_name() == Some(OsStr::new("kar")))
            && is_executable_file(probe)
        {
            return kar_bridge(probe);
        }
    }

    // 3. System `/usr/bin/nc -U <socket>` on macOS/BSD when hosted inside a non-`kar`
    // binary (such as `cargo test` runners or embedded Rust applications): requires
    // zero external binary build and carries zero credentials in argv/env.
    if is_executable_file(nc_fallback) {
        return Ok((
            nc_fallback.to_string_lossy().into_owned(),
            vec!["-U".into(), sock],
        ));
    }

    // 4. Sibling `kar` binary in parent/grandparent directories (e.g. target/debug/kar).
    if let Some(exe) = current_exe {
        let canonical = exe.canonicalize().ok();
        let probe = canonical.as_deref().unwrap_or(exe);
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

    // 5. `kar` on PATH.
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

    Err(Error::Unsupported(
        "DSH MCP bridge launcher (`kar` or `/usr/bin/nc`) is unavailable".into(),
    ))
}

fn resolve_bridge_launcher(socket: &Path) -> Result<(String, Vec<String>)> {
    let exe = std::env::current_exe().ok();
    let bridge_override = std::env::var_os("KAR_DSH_BRIDGE_EXE")
        .or_else(|| std::env::var_os("KAR_WORKBENCH_BRIDGE_EXE"));
    resolve_bridge_launcher_with(
        socket,
        bridge_override.as_deref(),
        exe.as_deref(),
        std::env::var_os("PATH").as_deref(),
        Path::new("/usr/bin/nc"),
    )
}

fn stdio_patch_row(id: &str, name: &str, command: &str, args: &[String], cwd: &Path) -> Value {
    json!({
        "id": id,
        "name": "@deepseek-ai/dsh-mcp-client",
        "config": {
            "serverName": name,
            "transport": "stdio",
            "command": command,
            "args": args,
            "cwd": cwd,
            "failOnStartupError": true
        }
    })
}

impl McpBridgeSet {
    fn allocate_socket_path(&mut self, profile_dir: &Path, index: usize) -> Result<PathBuf> {
        let file_name = format!("m{index}.sock");
        let direct = profile_dir.join(&file_name);
        if direct.as_os_str().len() <= MAX_UNIX_SOCKET_PATH_LEN {
            return Ok(direct);
        }
        if self._short_dir.is_none() {
            self._short_dir = Some(PrivateDir::create(
                Path::new("/tmp"),
                "kar-dsh-mcp-",
                b"Kar DSH MCP v1\n",
            )?);
        }
        Ok(self
            ._short_dir
            .as_ref()
            .expect("short_dir created")
            .path()
            .join(file_name))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_server(
        &mut self,
        profile_dir: &Path,
        index: usize,
        id: &str,
        name: &str,
        server: &McpServer,
        cwd: &Path,
        session_env: &BTreeMap<OsString, OsString>,
        unset_env: &BTreeSet<String>,
    ) -> Result<Value> {
        validate_server_name(name)?;

        match server {
            McpServer::Stdio { command, args, env } if env.is_empty() => {
                Ok(stdio_patch_row(id, name, command, args, cwd))
            }
            McpServer::Stdio { command, args, env } => {
                if command.trim().is_empty() {
                    return Err(Error::Unsupported(
                        "DSH stdio MCP server command cannot be empty".into(),
                    ));
                }
                validate_env_map(env)?;
                #[cfg(not(unix))]
                {
                    let _ = (profile_dir, index, session_env, unset_env);
                    return Err(Error::Unsupported(
                        "DSH credentialed stdio MCP bridge requires Unix domain sockets".into(),
                    ));
                }
                #[cfg(unix)]
                {
                    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
                        Error::Unsupported("DSH MCP bridge requires an active Tokio runtime".into())
                    })?;
                    let socket_path = self.allocate_socket_path(profile_dir, index)?;
                    let (launcher_cmd, launcher_args) = resolve_bridge_launcher(&socket_path)?;
                    let listener = UnixListener::bind(&socket_path)?;
                    let target = Arc::new(StdioBridgeTarget {
                        command: command.clone(),
                        args: args.clone(),
                        cwd: cwd.to_path_buf(),
                        session_env: session_env.clone(),
                        unset_env: unset_env.clone(),
                        server_env: env.clone(),
                    });
                    let shutdown_rx = self.shutdown_tx.subscribe();
                    let task = handle.spawn(accept_loop(
                        listener,
                        shutdown_rx,
                        BridgeBackend::Stdio(target),
                    ));
                    self.tasks.push(task);
                    Ok(stdio_patch_row(
                        id,
                        name,
                        &launcher_cmd,
                        &launcher_args,
                        cwd,
                    ))
                }
            }
            McpServer::Http { url, headers } | McpServer::HostedHttp { url, headers } => {
                let parsed_url = validate_http_url(url)?;
                let header_map = build_header_map(headers)?;
                #[cfg(not(unix))]
                {
                    let _ = (
                        profile_dir,
                        index,
                        session_env,
                        unset_env,
                        parsed_url,
                        header_map,
                    );
                    return Err(Error::Unsupported(
                        "DSH HTTP MCP bridge requires Unix domain sockets".into(),
                    ));
                }
                #[cfg(unix)]
                {
                    let _ = (session_env, unset_env);
                    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
                        Error::Unsupported("DSH MCP bridge requires an active Tokio runtime".into())
                    })?;
                    let socket_path = self.allocate_socket_path(profile_dir, index)?;
                    let (launcher_cmd, launcher_args) = resolve_bridge_launcher(&socket_path)?;
                    let listener = UnixListener::bind(&socket_path)?;
                    let mut builder = Client::builder().redirect(reqwest::redirect::Policy::none());
                    if matches!(
                        parsed_url.host_str(),
                        Some("127.0.0.1" | "localhost" | "::1")
                    ) {
                        builder = builder.no_proxy();
                    }
                    let client = builder
                        .build()
                        .map_err(|e| Error::Protocol(format!("DSH HTTP MCP client failed: {e}")))?;
                    let target = Arc::new(HttpBridgeTarget {
                        client,
                        url: parsed_url.to_string(),
                        headers: header_map,
                    });
                    let shutdown_rx = self.shutdown_tx.subscribe();
                    let task = handle.spawn(accept_loop(
                        listener,
                        shutdown_rx,
                        BridgeBackend::Http(target),
                    ));
                    self.tasks.push(task);
                    Ok(stdio_patch_row(
                        id,
                        name,
                        &launcher_cmd,
                        &launcher_args,
                        cwd,
                    ))
                }
            }
        }
    }
}

#[derive(Clone)]
enum BridgeBackend {
    Http(Arc<HttpBridgeTarget>),
    Stdio(Arc<StdioBridgeTarget>),
}

struct HttpBridgeTarget {
    client: Client,
    url: String,
    headers: HeaderMap,
}

struct StdioBridgeTarget {
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    session_env: BTreeMap<OsString, OsString>,
    unset_env: BTreeSet<String>,
    server_env: BTreeMap<String, String>,
}

#[cfg(unix)]
fn verify_same_user(stream: &UnixStream) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|cred| cred.uid() == nix::unistd::geteuid().as_raw())
}

#[cfg(unix)]
async fn accept_loop(
    listener: UnixListener,
    mut shutdown_rx: watch::Receiver<bool>,
    backend: BridgeBackend,
) {
    let limit = Arc::new(Semaphore::new(16));
    loop {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                if !verify_same_user(&stream) {
                    continue;
                }
                let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else {
                    continue;
                };
                let conn_backend = backend.clone();
                let mut conn_shutdown = shutdown_rx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    match conn_backend {
                        BridgeBackend::Http(target) => {
                            serve_http_connection(stream, target, &mut conn_shutdown).await;
                        }
                        BridgeBackend::Stdio(target) => {
                            serve_stdio_connection(stream, target, &mut conn_shutdown).await;
                        }
                    }
                });
            }
        }
    }
}

async fn write_json_line(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    value: &Value,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

async fn relay_sse_body(
    response: reqwest::Response,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Option<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut data_lines: Vec<String> = Vec::new();

    let flush_event = |data_lines: &mut Vec<String>| -> Option<Value> {
        if data_lines.is_empty() {
            return None;
        }
        let payload = data_lines.join("\n");
        data_lines.clear();
        let trimmed = payload.trim();
        if trimmed.is_empty() {
            return None;
        }
        serde_json::from_str::<Value>(trimmed).ok()
    };

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.ok()?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(newline) = buffer.find('\n') {
            let mut line = buffer[..newline].to_owned();
            buffer.drain(..=newline);
            if line.ends_with('\r') {
                line.pop();
            }
            if line.is_empty() {
                if let Some(json_msg) = flush_event(&mut data_lines) {
                    write_json_line(writer, &json_msg).await.ok()?;
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
            }
        }
    }
    if !buffer.trim().is_empty()
        && let Some(rest) = buffer.trim_end_matches('\r').strip_prefix("data:")
    {
        data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
    }
    if let Some(json_msg) = flush_event(&mut data_lines) {
        write_json_line(writer, &json_msg).await.ok()?;
    }
    Some(())
}

async fn serve_http_connection(
    stream: UnixStream,
    target: Arc<HttpBridgeTarget>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = FramedRead::new(read_half, LinesCodec::new_with_max_length(MAX_FRAME_BYTES));
    let mut session_id: Option<String> = None;

    loop {
        let line = tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
            next = lines.next() => match next {
                Some(Ok(line)) => line,
                _ => break,
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let request_id = msg.get("id").cloned();

        let mut req = target
            .client
            .post(&target.url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(id) = &session_id {
            req = req.header(MCP_SESSION_HEADER, id);
        }
        for (k, v) in &target.headers {
            req = req.header(k, v);
        }

        let response = match req.json(&msg).send().await {
            Ok(resp) => resp,
            Err(_) => {
                if let Some(id) = request_id {
                    let err_reply = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": "DSH HTTP MCP bridge upstream request failed"
                        }
                    });
                    if write_json_line(&mut write_half, &err_reply).await.is_err() {
                        break;
                    }
                }
                continue;
            }
        };

        let status = response.status();
        if let Some(raw_sid) = response.headers().get(MCP_SESSION_HEADER)
            && let Ok(sid_str) = raw_sid.to_str()
            && !sid_str.is_empty()
        {
            session_id = Some(sid_str.to_owned());
        }

        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            continue;
        }

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));

        if is_sse && status.is_success() {
            if relay_sse_body(response, &mut write_half).await.is_none() {
                break;
            }
            continue;
        }

        let parsed = response.json::<Value>().await;
        match parsed {
            Ok(body) => {
                if let Value::Array(batch) = body {
                    let mut failed = false;
                    for item in batch {
                        if write_json_line(&mut write_half, &item).await.is_err() {
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                } else if write_json_line(&mut write_half, &body).await.is_err() {
                    break;
                }
            }
            Err(_) => {
                if let Some(id) = request_id {
                    let err_reply = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": format!("DSH HTTP MCP upstream returned HTTP {}", status.as_u16())
                        }
                    });
                    if write_json_line(&mut write_half, &err_reply).await.is_err() {
                        break;
                    }
                }
            }
        }

        if status == StatusCode::UNAUTHORIZED {
            break;
        }
    }

    if let Some(sid) = session_id {
        let mut del = target
            .client
            .delete(&target.url)
            .header(MCP_SESSION_HEADER, sid)
            .timeout(Duration::from_secs(2));
        for (k, v) in &target.headers {
            del = del.header(k, v);
        }
        let _ = del.send().await;
    }
}

async fn serve_stdio_connection(
    stream: UnixStream,
    target: Arc<StdioBridgeTarget>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let mut cmd = tokio::process::Command::new(&target.command);
    cmd.args(&target.args)
        .current_dir(&target.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for removed in &target.unset_env {
        cmd.env_remove(removed);
    }
    cmd.envs(&target.session_env);
    cmd.envs(&target.server_env);

    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    let Some(mut child_stdin) = child.stdin.take() else {
        return;
    };
    let Some(mut child_stdout) = child.stdout.take() else {
        return;
    };

    let (mut read_sock, mut write_sock) = stream.into_split();
    let to_child = async move {
        let _ = tokio::io::copy(&mut read_sock, &mut child_stdin).await;
        let _ = child_stdin.shutdown().await;
    };
    let from_child = async move {
        let _ = tokio::io::copy(&mut child_stdout, &mut write_sock).await;
        let _ = write_sock.shutdown().await;
    };

    tokio::select! {
        biased;
        changed = shutdown_rx.changed() => {
            let _ = changed;
        }
        _ = async {
            tokio::join!(to_child, from_child);
        } => {}
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Relay stdio JSON-RPC traffic to a session-scoped DSH MCP Unix domain socket.
///
/// Invoked by `kar dsh-mcp-bridge <socket>` or directly from embedded Rust hosts.
pub async fn run_dsh_mcp_bridge(socket: &Path) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = socket;
        Err(Error::Unsupported(
            "dsh-mcp-bridge requires Unix domain sockets".into(),
        ))
    }
    #[cfg(unix)]
    {
        let stream = UnixStream::connect(socket).await?;
        let (mut read_sock, mut write_sock) = stream.into_split();
        let writer = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let _ = tokio::io::copy(&mut stdin, &mut write_sock).await;
            let _ = write_sock.shutdown().await;
        });
        let mut stdout = tokio::io::stdout();
        tokio::io::copy(&mut read_sock, &mut stdout).await?;
        stdout.flush().await?;
        writer.abort();
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap as AxumHeaders, StatusCode as AxumStatus},
        response::IntoResponse,
        routing::post,
    };
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn make_exec(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn launcher_resolution_supports_env_kar_binary_and_nc_fallback() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let sock = tmp.path().join("m0.sock");
        let absent_nc = tmp.path().join("no-nc");
        let mock_nc = tmp.path().join("nc");
        make_exec(&mock_nc);

        // 1. Explicit env override takes top priority.
        let override_bin = tmp.path().join("bin/custom-kar");
        make_exec(&override_bin);
        let host_bin = tmp.path().join("deps/test-runner");
        make_exec(&host_bin);
        let (cmd, args) = resolve_bridge_launcher_with(
            &sock,
            Some(override_bin.as_os_str()),
            Some(&host_bin),
            None,
            &mock_nc,
        )?;
        assert_eq!(Path::new(&cmd), override_bin.canonicalize()?);
        assert_eq!(args, vec!["dsh-mcp-bridge", sock.to_str().unwrap()]);

        // 2. When current_exe is `kar`, `kar dsh-mcp-bridge <sock>` wins.
        let kar_bin = tmp.path().join("bin/kar");
        make_exec(&kar_bin);
        let (cmd, args) =
            resolve_bridge_launcher_with(&sock, None, Some(&kar_bin), None, &mock_nc)?;
        assert_eq!(Path::new(&cmd), kar_bin.canonicalize()?);
        assert_eq!(args, vec!["dsh-mcp-bridge", sock.to_str().unwrap()]);

        // 3. When current_exe is a test binary or embedded host and `/usr/bin/nc` exists,
        // `nc -U <sock>` is chosen without needing a separate `kar` binary build.
        let (cmd, args) =
            resolve_bridge_launcher_with(&sock, None, Some(&host_bin), None, &mock_nc)?;
        assert_eq!(Path::new(&cmd), mock_nc);
        assert_eq!(args, vec!["-U", sock.to_str().unwrap()]);

        // 4. When `nc` is absent, sibling `kar` or `PATH` `kar` is used.
        let sibling_kar = tmp.path().join("kar");
        make_exec(&sibling_kar);
        let (cmd, args) =
            resolve_bridge_launcher_with(&sock, None, Some(&host_bin), None, &absent_nc)?;
        assert_eq!(Path::new(&cmd), sibling_kar.canonicalize()?);
        assert_eq!(args[0], "dsh-mcp-bridge");

        Ok(())
    }

    #[tokio::test]
    async fn http_mcp_bridge_forwards_secret_headers_and_sse_while_keeping_patch_clean()
    -> Result<()> {
        #[derive(Clone, Default)]
        struct Seen {
            auth: Arc<std::sync::Mutex<Vec<String>>>,
            sessions: Arc<std::sync::Mutex<Vec<Option<String>>>>,
        }

        async fn mcp_endpoint(
            State(seen): State<Seen>,
            headers: AxumHeaders,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            seen.auth.lock().unwrap().push(auth);
            let sid = headers
                .get(MCP_SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            seen.sessions.lock().unwrap().push(sid);

            match body["method"].as_str() {
                Some("initialize") => (
                    AxumStatus::OK,
                    [(MCP_SESSION_HEADER, "sess-dsh-42")],
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": { "protocolVersion": "2025-03-26" }
                    })),
                )
                    .into_response(),
                Some("notifications/initialized") => AxumStatus::ACCEPTED.into_response(),
                Some("tools/list") => {
                    let sse = format!(
                        "event: message\ndata: {}\n\n",
                        json!({
                            "jsonrpc": "2.0",
                            "id": body["id"],
                            "result": { "tools": [{ "name": "lookup" }] }
                        })
                    );
                    (
                        AxumStatus::OK,
                        [(reqwest::header::CONTENT_TYPE, "text/event-stream")],
                        sse,
                    )
                        .into_response()
                }
                _ => AxumStatus::BAD_REQUEST.into_response(),
            }
        }

        let seen = Seen::default();
        let app = Router::new()
            .route("/mcp", post(mcp_endpoint))
            .with_state(seen.clone());
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = tcp.local_addr()?;
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(tcp, app).await;
        });

        let dir = PrivateDir::create(&std::env::temp_dir(), "kar-dsh-test-", b"test\n")?;
        let secret = "Bearer super-secret-dsh-token-999";
        let server = McpServer::Http {
            url: format!("http://{addr}/mcp"),
            headers: BTreeMap::from([("Authorization".into(), secret.into())]),
        };

        let mut bridges = McpBridgeSet::default();
        let row = bridges.register_server(
            dir.path(),
            0,
            "kar-mcp-1",
            "remote_docs",
            &server,
            dir.path(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )?;

        let serialized_patch = serde_json::to_string(&row)?;
        assert!(!serialized_patch.contains("super-secret-dsh-token-999"));
        assert_eq!(row["config"]["transport"], "stdio");
        assert_eq!(row["config"]["serverName"], "remote_docs");

        // Connect directly to the bridged socket (or spawn the resolved launcher!) and verify round-trip.
        let sock_arg = row["config"]["args"]
            .as_array()
            .and_then(|a| a.last())
            .and_then(Value::as_str)
            .unwrap();
        let stream = UnixStream::connect(sock_arg).await?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half).lines();

        write_half
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
            .await?;
        let init_reply: Value = serde_json::from_str(&reader.next_line().await?.unwrap())?;
        assert_eq!(init_reply["id"], 1);
        assert_eq!(init_reply["result"]["protocolVersion"], "2025-03-26");

        write_half
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await?;
        write_half
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await?;
        let tools_reply: Value = serde_json::from_str(&reader.next_line().await?.unwrap())?;
        assert_eq!(tools_reply["id"], 2);
        assert_eq!(tools_reply["result"]["tools"][0]["name"], "lookup");

        {
            let auths = seen.auth.lock().unwrap().clone();
            assert_eq!(auths, vec![secret, secret, secret]);
            let sids = seen.sessions.lock().unwrap().clone();
            assert_eq!(
                sids,
                vec![None, Some("sess-dsh-42".into()), Some("sess-dsh-42".into())]
            );
        }

        drop(bridges);
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn credentialed_stdio_mcp_bridge_injects_env_without_exposing_it_in_patch() -> Result<()>
    {
        let dir = PrivateDir::create(&std::env::temp_dir(), "kar-dsh-stdio-", b"stdio\n")?;
        let secret_val = "sk-dsh-stdio-secret-777";
        let server = McpServer::Stdio {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "read line; printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"secret\":\"%s\"}\\n' \"$MCP_API_KEY\"".into(),
            ],
            env: BTreeMap::from([("MCP_API_KEY".into(), secret_val.into())]),
        };

        let mut bridges = McpBridgeSet::default();
        let row = bridges.register_server(
            dir.path(),
            0,
            "kar-mcp-stdio",
            "local_tool",
            &server,
            dir.path(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )?;

        let serialized_patch = serde_json::to_string(&row)?;
        assert!(!serialized_patch.contains(secret_val));
        assert!(row["config"].get("env").is_none());

        // Spawn the exact launcher command & args written into `--patch`!
        let launcher_cmd = row["config"]["command"].as_str().unwrap();
        let launcher_args: Vec<String> = serde_json::from_value(row["config"]["args"].clone())?;
        let mut child = tokio::process::Command::new(launcher_cmd)
            .args(&launcher_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
            .await?;
        stdin.flush().await?;

        let mut lines = BufReader::new(stdout).lines();
        let reply_line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .map_err(|_| Error::Timeout("stdio bridge reply".into()))??
            .unwrap();
        let reply: Value = serde_json::from_str(&reply_line)?;
        assert_eq!(reply["secret"], secret_val);

        drop(stdin);
        let _ = child.start_kill();
        drop(bridges);
        Ok(())
    }
}
