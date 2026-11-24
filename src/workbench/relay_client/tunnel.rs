//! Own a single managed cloudflared connector. Only its private configuration sees the token.
use super::super::Workbench;
use crate::{
    Error, Result,
    process::OwnedProcess,
    resources::tree::{ProcessId, identity},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex, Weak},
    time::Duration,
};

#[derive(Default)]
pub(in crate::workbench) struct Service {
    status: Mutex<Option<Value>>,
}
impl Service {
    fn publish(&self, state: &str, pid: Option<u32>, detail: Option<&str>) {
        *self.status.lock().unwrap() =
            Some(json!({"state":state,"connected":state=="active","pid":pid,"message":detail}));
    }
}
impl Workbench {
    pub(in crate::workbench) fn managed_tunnel_status(&self) -> Value {
        self.0
            .relay
            .status
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| json!({"state":"disabled","connected":false}))
    }
}

struct Connection {
    process: OwnedProcess,
    owner: ProcessId,
    port: u16,
    token_digest: String,
    ledger: PathBuf,
}
fn recover(ledger: &Path) -> Result<()> {
    match fs::read(ledger) {
        Ok(data) => {
            let id: ProcessId = serde_json::from_slice(&data)?;
            super::super::terminal_preview::stop_owned_process(id);
            fs::remove_file(ledger)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
impl Drop for Connection {
    fn drop(&mut self) {
        super::super::terminal_preview::stop_owned_process(self.owner);
        let _ = fs::remove_file(&self.ledger);
    }
}
async fn launch(app: &Workbench, token: &str, digest: String, ledger: &Path) -> Result<Connection> {
    let status = super::resolve(app).await?;
    let path = status["executable_path"]
        .as_str()
        .ok_or_else(|| Error::Refused("tunnel client is unavailable".into()))?;
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
        .local_addr()?
        .port();
    let mut cmd = tokio::process::Command::new(path);
    cmd.args([
        "tunnel",
        "--no-autoupdate",
        "--loglevel",
        "info",
        "--output",
        "default",
        "--metrics",
        &format!("127.0.0.1:{port}"),
        "run",
    ]);
    cmd.env_clear();
    for key in [
        "PATH",
        "HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTPS_PROXY",
        "NO_PROXY",
    ] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("TUNNEL_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let process = OwnedProcess::spawn(cmd)?;
    let owner = process.id().and_then(identity).ok_or_else(|| {
        Error::Closed("tunnel process exited before ownership could be recorded".into())
    })?;
    let root = ledger
        .parent()
        .ok_or_else(|| Error::Protocol("missing relay state path".into()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(root)?;
    tmp.write_all(&serde_json::to_vec(&owner)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(ledger).map_err(|e| e.error)?;
    fs::File::open(root)?.sync_all()?;
    Ok(Connection {
        process,
        owner,
        port,
        token_digest: digest,
        ledger: ledger.to_owned(),
    })
}

pub(in crate::workbench) fn start(
    app: &Workbench,
    mut changes: tokio::sync::watch::Receiver<()>,
    config: impl Fn(&Workbench) -> Option<String> + Send + 'static,
) {
    let weak = std::sync::Arc::downgrade(&app.0);
    let ledger = app.0.state.join("relay-process.json");
    tokio::spawn(async move {
        // A dying server may have left an owned connector. A birth check excludes PID reuse.
        if recover(&ledger).is_err() {
            if let Some(inner) = weak.upgrade() {
                inner.relay.publish(
                    "failed",
                    None,
                    Some("could not recover prior relay ownership"),
                );
            }
            return;
        }
        let client = match reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(client) => client,
            Err(_) => return,
        };
        let mut current: Option<Connection> = None;
        loop {
            let token = {
                let Some(inner) = Weak::upgrade(&weak) else {
                    return;
                };
                config(&Workbench(inner))
            };
            let digest = token.as_deref().map(super::super::access::digest);
            if current.as_mut().is_some_and(|c| {
                Some(&c.token_digest) != digest.as_ref() || c.process.exited().unwrap_or(true)
            }) {
                drop(current.take());
            }
            if let Some(inner) = weak.upgrade() {
                let app = Workbench(inner);
                if let (Some(token), Some(digest)) = (token, digest) {
                    if current.is_none() {
                        match launch(&app, &token, digest, &ledger).await {
                            Ok(connection) => current = Some(connection),
                            Err(_) => app.0.relay.publish(
                                "failed",
                                None,
                                Some("could not start the configured relay client"),
                            ),
                        }
                    }
                    if let Some(conn) = current.as_ref() {
                        let owner = conn.owner;
                        let port = conn.port;
                        let owns = tokio::task::spawn_blocking(move || {
                            super::super::terminal_preview::owns_local_port(owner, port)
                        })
                        .await
                        .unwrap_or(false);
                        let ready = owns
                            && client
                                .get(format!("http://127.0.0.1:{port}/ready"))
                                .send()
                                .await
                                .is_ok_and(|r| r.status().is_success());
                        app.0.relay.publish(
                            if ready { "active" } else { "starting" },
                            Some(owner.pid),
                            None,
                        );
                    }
                } else {
                    app.0.relay.publish("disabled", None, None);
                }
            } else {
                return;
            }
            let delay = if current.is_some() {
                Duration::from_secs(4)
            } else {
                Duration::from_secs(15)
            };
            tokio::select! {_=tokio::time::sleep(delay)=>(), result=changes.changed()=>{if result.is_err(){return;}}}
        }
    });
}
