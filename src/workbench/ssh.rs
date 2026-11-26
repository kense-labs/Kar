//! Pure-Rust SSH remote workspace engine for macOS:
//! - Parses `~/.ssh/config` (including `Host` wildcards, `HostName`, `User`, `Port`, `IdentityFile`, `ProxyJump`)
//! - Manages OpenSSH `ControlMaster` sockets in a private `0700` directory
//! - Bootstraps or attaches to a remote `kar workbench` over a local `127.0.0.1` SSH `-L` port-forward tunnel

use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{Error, Result, process::OwnedProcess};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHostConfig {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
    pub forward_agent: Option<bool>,
}

fn glob_matches(pattern: &str, target: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = target.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi].eq_ignore_ascii_case(&t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            pi += 1;
            star_t = ti;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn host_patterns_match(patterns: &[&str], target: &str) -> bool {
    let mut matched = false;
    for pat in patterns {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_matches(neg, target) {
                return false;
            }
        } else if glob_matches(pat, target) {
            matched = true;
        }
    }
    matched
}

/// Parse OpenSSH config text into named concrete host aliases and resolve effective options for a host.
pub fn parse_ssh_config(text: &str) -> Vec<SshHostConfig> {
    #[derive(Default)]
    struct Block {
        patterns: Vec<String>,
        hostname: Option<String>,
        user: Option<String>,
        port: Option<u16>,
        identity_files: Vec<String>,
        proxy_jump: Option<String>,
        forward_agent: Option<bool>,
    }
    let mut blocks: Vec<Block> = Vec::new();
    let mut current = Block {
        patterns: vec!["*".into()],
        ..Default::default()
    };

    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, rest)) = line
            .split_once(|c: char| c.is_ascii_whitespace() || c == '=')
            .map(|(k, v)| (k.trim(), v.trim().trim_start_matches('=').trim()))
        else {
            continue;
        };
        let val = rest.trim_matches('"');
        if key.eq_ignore_ascii_case("Host") {
            blocks.push(current);
            current = Block {
                patterns: val.split_whitespace().map(str::to_owned).collect(),
                ..Default::default()
            };
        } else if key.eq_ignore_ascii_case("Match") {
            blocks.push(current);
            // Ignore conditional Match blocks in static listing.
            current = Block::default();
        } else if key.eq_ignore_ascii_case("HostName") && current.hostname.is_none() {
            current.hostname = Some(val.to_owned());
        } else if key.eq_ignore_ascii_case("User") && current.user.is_none() {
            current.user = Some(val.to_owned());
        } else if key.eq_ignore_ascii_case("Port") && current.port.is_none() {
            current.port = val.parse().ok();
        } else if key.eq_ignore_ascii_case("IdentityFile") {
            current.identity_files.push(val.to_owned());
        } else if key.eq_ignore_ascii_case("ProxyJump") && current.proxy_jump.is_none() {
            current.proxy_jump = Some(val.to_owned());
        } else if key.eq_ignore_ascii_case("ForwardAgent") && current.forward_agent.is_none() {
            current.forward_agent = Some(val.eq_ignore_ascii_case("yes"));
        }
    }
    blocks.push(current);

    let mut concrete_aliases = Vec::new();
    for b in &blocks {
        for pat in &b.patterns {
            if !pat.contains(['*', '?', '!']) && !concrete_aliases.contains(pat) {
                concrete_aliases.push(pat.clone());
            }
        }
    }

    concrete_aliases
        .into_iter()
        .map(|alias| {
            let mut out = SshHostConfig {
                alias: alias.clone(),
                ..Default::default()
            };
            for b in &blocks {
                let refs: Vec<&str> = b.patterns.iter().map(String::as_str).collect();
                if !refs.is_empty() && host_patterns_match(&refs, &alias) {
                    if out.hostname.is_none() {
                        out.hostname = b.hostname.clone();
                    }
                    if out.user.is_none() {
                        out.user = b.user.clone();
                    }
                    if out.port.is_none() {
                        out.port = b.port;
                    }
                    for idf in &b.identity_files {
                        if !out.identity_files.contains(idf) {
                            out.identity_files.push(idf.clone());
                        }
                    }
                    if out.proxy_jump.is_none() {
                        out.proxy_jump = b.proxy_jump.clone();
                    }
                    if out.forward_agent.is_none() {
                        out.forward_agent = b.forward_agent;
                    }
                }
            }
            out
        })
        .collect()
}

pub fn load_user_ssh_hosts() -> Vec<SshHostConfig> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let config_path = home.join(".ssh/config");
    let Ok(main_text) = fs::read_to_string(&config_path) else {
        return Vec::new();
    };
    let mut combined = String::new();
    for line in main_text.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if let Some(rest) = trimmed
            .strip_prefix("Include ")
            .or_else(|| trimmed.strip_prefix("include "))
        {
            let pat = rest.trim().trim_matches('"');
            let expanded = if let Some(rel) = pat.strip_prefix("~/") {
                home.join(rel)
            } else if Path::new(pat).is_absolute() {
                PathBuf::from(pat)
            } else {
                home.join(".ssh").join(pat)
            };
            if expanded.is_file()
                && let Ok(inc) = fs::read_to_string(&expanded)
            {
                combined.push_str(&inc);
                combined.push('\n');
            } else if let Some(parent) = expanded.parent()
                && let Some(glob_name) = expanded.file_name().and_then(|n| n.to_str())
                && let Ok(entries) = fs::read_dir(parent)
            {
                let mut files: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok().map(|d| d.path()))
                    .filter(|p| {
                        p.is_file()
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| glob_matches(glob_name, n))
                    })
                    .collect();
                files.sort();
                for f in files {
                    if let Ok(inc) = fs::read_to_string(&f) {
                        combined.push_str(&inc);
                        combined.push('\n');
                    }
                }
            }
        } else {
            combined.push_str(line);
            combined.push('\n');
        }
    }
    parse_ssh_config(&combined)
}

pub fn validate_ssh_destination(dest: &str) -> Result<()> {
    let trimmed = dest.trim();
    if trimmed.is_empty()
        || trimmed.len() > 255
        || trimmed.starts_with('-')
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(Error::Refused("invalid SSH destination".into()));
    }
    Ok(())
}

pub fn validate_remote_path(path: &str) -> Result<()> {
    if path.trim().is_empty()
        || path.len() > 4096
        || path.chars().any(|c| c == '\0' || c == '\n' || c == '\r')
    {
        return Err(Error::Refused("invalid remote workspace path".into()));
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshSessionSpec {
    pub id: Uuid,
    pub destination: String,
    pub port: Option<u16>,
    pub remote_root: String,
    pub remote_kar_bin: Option<String>,
    pub label: Option<String>,
}

struct ActiveTunnel {
    spec: SshSessionSpec,
    local_port: u16,
    remote_port: u16,
    token: String,
    _remote_proc: OwnedProcess,
    _forward_proc: OwnedProcess,
}

#[derive(Default)]
pub struct SshService {
    tunnels: Mutex<BTreeMap<Uuid, ActiveTunnel>>,
}

impl SshService {
    fn socket_dir(state_dir: &Path) -> Result<PathBuf> {
        // Keep socket path short for macOS sun_path (104 bytes max).
        let hash = Sha256::digest(state_dir.as_os_str().as_encoded_bytes());
        let mut digest = String::with_capacity(12);
        for b in &hash[..6] {
            digest.push_str(&format!("{b:02x}"));
        }
        let dir = PathBuf::from("/tmp").join(format!("kar-ssh-{digest}"));
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(dir)
    }

    fn base_ssh_command(state_dir: &Path, destination: &str, port: Option<u16>) -> Result<Command> {
        validate_ssh_destination(destination)?;
        let sock_dir = Self::socket_dir(state_dir)?;
        let mut cmd = Command::new("/usr/bin/ssh");
        cmd.env_remove("KAR_WORKBENCH_TOKEN")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPersist=600")
            .arg("-o")
            .arg(format!("ControlPath={}/%C", sock_dir.display()));
        if let Some(p) = port {
            cmd.arg("-p").arg(p.to_string());
        }
        Ok(cmd)
    }

    pub async fn probe(
        state_dir: &Path,
        destination: &str,
        port: Option<u16>,
        remote_kar_bin: Option<&str>,
    ) -> Result<Value> {
        let mut cmd = Self::base_ssh_command(state_dir, destination, port)?;
        let kar_bin = remote_kar_bin.unwrap_or("kar");
        let script = format!(
            "uname -sm && ({bin} --version 2>/dev/null || $HOME/.cargo/bin/kar --version 2>/dev/null || $HOME/.local/bin/kar --version 2>/dev/null || echo KAR_MISSING)",
            bin = shell_quote(kar_bin)
        );
        cmd.arg("--")
            .arg(destination)
            .arg(script)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
            .await
            .map_err(|_| Error::Timeout("probing SSH host".into()))??;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(Error::Refused(if err.is_empty() {
                "SSH connection failed".into()
            } else {
                err
            }));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        let platform = lines.next().unwrap_or("unknown").trim().to_owned();
        let kar_line = lines.next().unwrap_or("KAR_MISSING").trim().to_owned();
        let kar_installed = kar_line != "KAR_MISSING" && !kar_line.is_empty();
        Ok(json!({
            "reachable": true,
            "destination": destination,
            "platform": platform,
            "kar_installed": kar_installed,
            "kar_version": kar_installed.then_some(kar_line),
        }))
    }

    pub async fn connect(&self, state_dir: &Path, spec: SshSessionSpec) -> Result<Value> {
        validate_ssh_destination(&spec.destination)?;
        validate_remote_path(&spec.remote_root)?;
        let kar_bin = spec.remote_kar_bin.as_deref().unwrap_or("kar");

        // Reserve an ephemeral loopback port on macOS.
        let local_port = {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            listener.local_addr()?.port()
        };

        let mut remote_cmd = Self::base_ssh_command(state_dir, &spec.destination, spec.port)?;
        let launch_script = format!(
            "exec {bin} workbench --root {root} --port 0",
            bin = shell_quote(kar_bin),
            root = shell_quote(&spec.remote_root),
        );
        remote_cmd
            .arg("--")
            .arg(&spec.destination)
            .arg(launch_script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut remote_proc = OwnedProcess::spawn(remote_cmd)?;
        let stdout = remote_proc
            .inner()
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("missing SSH stdout".into()))?;
        let mut reader = BufReader::new(stdout).lines();
        let first_line = tokio::time::timeout(Duration::from_secs(20), reader.next_line())
            .await
            .map_err(|_| Error::Timeout("waiting for remote Kar workbench startup".into()))??
            .ok_or_else(|| {
                Error::Closed("remote Kar workbench exited before printing startup JSON".into())
            })?;
        let startup: Value = serde_json::from_str(&first_line)
            .map_err(|_| Error::Protocol("invalid remote Kar workbench startup JSON".into()))?;
        let remote_url = startup["url"]
            .as_str()
            .ok_or_else(|| Error::Protocol("missing remote url".into()))?;
        let token = startup["token"]
            .as_str()
            .ok_or_else(|| Error::Protocol("missing remote token".into()))?
            .to_owned();
        let parsed = url::Url::parse(remote_url)
            .map_err(|_| Error::Protocol("invalid remote loopback URL".into()))?;
        let remote_port = parsed
            .port()
            .ok_or_else(|| Error::Protocol("missing remote loopback port".into()))?;

        // Keep draining stdout so remote process never blocks on pipe backpressure.
        tokio::spawn(async move { while let Ok(Some(_)) = reader.next_line().await {} });

        let mut fwd_cmd = Self::base_ssh_command(state_dir, &spec.destination, spec.port)?;
        fwd_cmd
            .arg("-N")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-L")
            .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"))
            .arg("--")
            .arg(&spec.destination)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let forward_proc = OwnedProcess::spawn(fwd_cmd)?;

        // Wait briefly until local tunnel port accepts TCP connections.
        let local_url = format!("http://127.0.0.1:{local_port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .no_proxy()
            .build()?;
        let mut ready = false;
        for _ in 0..25 {
            if let Ok(r) = client
                .get(format!("{local_url}/projects"))
                .bearer_auth(&token)
                .send()
                .await
                && r.status().is_success()
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        if !ready {
            return Err(Error::Timeout(
                "SSH port-forward tunnel did not reach remote Kar workbench".into(),
            ));
        }

        let summary = json!({
            "id": spec.id,
            "destination": spec.destination,
            "remote_root": spec.remote_root,
            "local_url": local_url,
            "local_port": local_port,
            "remote_port": remote_port,
            "token": token,
            "status": "connected"
        });
        self.tunnels.lock().unwrap().insert(
            spec.id,
            ActiveTunnel {
                spec,
                local_port,
                remote_port,
                token,
                _remote_proc: remote_proc,
                _forward_proc: forward_proc,
            },
        );
        Ok(summary)
    }

    pub fn list(&self) -> Vec<Value> {
        self.tunnels
            .lock()
            .unwrap()
            .values()
            .map(|t| {
                json!({
                    "id": t.spec.id,
                    "destination": t.spec.destination,
                    "port": t.spec.port,
                    "remote_root": t.spec.remote_root,
                    "label": t.spec.label,
                    "local_url": format!("http://127.0.0.1:{}", t.local_port),
                    "local_port": t.local_port,
                    "remote_port": t.remote_port,
                    "status": "connected",
                })
            })
            .collect()
    }

    pub fn disconnect(&self, id: Uuid) -> bool {
        let _ = &self.tunnels.lock().unwrap().get(&id).map(|t| &t.token);
        self.tunnels.lock().unwrap().remove(&id).is_some()
    }
}

static GLOBAL_SSH: std::sync::OnceLock<Arc<SshService>> = std::sync::OnceLock::new();

pub(in crate::workbench) fn service() -> Arc<SshService> {
    GLOBAL_SSH
        .get_or_init(|| Arc::new(SshService::default()))
        .clone()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeReq {
    destination: String,
    port: Option<u16>,
    remote_kar_bin: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectReq {
    id: Option<Uuid>,
    destination: String,
    port: Option<u16>,
    remote_root: String,
    remote_kar_bin: Option<String>,
    label: Option<String>,
}

async fn list_hosts() -> std::result::Result<Json<Value>, ApiError> {
    blocking(|| Ok(json!({"hosts": load_user_ssh_hosts()}))).await
}

async fn list_sessions() -> Json<Value> {
    Json(json!({"sessions": service().list()}))
}

async fn probe_host(
    State(app): State<Workbench>,
    Json(req): Json<ProbeReq>,
) -> std::result::Result<Json<Value>, ApiError> {
    Ok(Json(
        SshService::probe(
            &app.0.state,
            &req.destination,
            req.port,
            req.remote_kar_bin.as_deref(),
        )
        .await?,
    ))
}

async fn connect_host(
    State(app): State<Workbench>,
    Json(req): Json<ConnectReq>,
) -> std::result::Result<Json<Value>, ApiError> {
    let spec = SshSessionSpec {
        id: req.id.unwrap_or_else(Uuid::new_v4),
        destination: req.destination,
        port: req.port,
        remote_root: req.remote_root,
        remote_kar_bin: req.remote_kar_bin,
        label: req.label,
    };
    Ok(Json(service().connect(&app.0.state, spec).await?))
}

async fn disconnect_host(
    AxumPath(id): AxumPath<Uuid>,
) -> std::result::Result<Json<Value>, ApiError> {
    if !service().disconnect(id) {
        return Err(super::missing().into());
    }
    Ok(Json(json!({"disconnected": true, "id": id})))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/environments/ssh/hosts", get(list_hosts))
        .route("/environments/ssh/probe", post(probe_host))
        .route("/environments/ssh", get(list_sessions).post(connect_host))
        .route("/environments/ssh/{id}", delete(disconnect_host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_config_parser_handles_wildcards_negation_and_precedence() {
        let cfg = r#"
# Global defaults
Host dev-* !dev-blocked
    User developer
    Port 2222
    ForwardAgent yes

Host dev-box
    HostName 10.20.30.40
    IdentityFile ~/.ssh/id_ed25519

Host prod-box
    HostName prod.example.com
    User admin

Host *
    User fallback
    Port 22
"#;
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts.len(), 2);
        let dev = hosts.iter().find(|h| h.alias == "dev-box").unwrap();
        assert_eq!(dev.hostname.as_deref(), Some("10.20.30.40"));
        // First matching option wins in OpenSSH config!
        assert_eq!(dev.user.as_deref(), Some("developer"));
        assert_eq!(dev.port, Some(2222));
        assert_eq!(dev.forward_agent, Some(true));
        assert_eq!(dev.identity_files, vec!["~/.ssh/id_ed25519"]);

        let prod = hosts.iter().find(|h| h.alias == "prod-box").unwrap();
        assert_eq!(prod.user.as_deref(), Some("admin"));
        assert_eq!(prod.port, Some(22));
    }

    #[test]
    fn ssh_destination_validation_rejects_option_injection() {
        assert!(validate_ssh_destination("-oProxyCommand=evil").is_err());
        assert!(validate_ssh_destination("user@host with space").is_err());
        assert!(validate_ssh_destination("leon@mac-mini.local").is_ok());
        assert!(validate_remote_path("/Users/leon/project").is_ok());
        assert!(validate_remote_path("bad\npath").is_err());
    }
}
