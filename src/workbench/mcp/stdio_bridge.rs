//! The provider's stdio launcher gets a socket pathname. Only descendants of the
//! selected native process can use it. The HTTP credential stays in the workbench.
use super::{SESSION, STATEFUL, VERSION, registry::Grant};
use crate::{McpServer, WorkbenchRegistration, private_dir::PrivateDir, resources::tree};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::{io, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{Semaphore, watch},
};
use tokio_util::codec::{FramedRead, LinesCodec};

pub(super) fn listen(
    addr: SocketAddr,
    token: String,
    grant: &Arc<Grant>,
    registration: &WorkbenchRegistration,
) -> crate::Result<McpServer> {
    // The CLI normal entry point can act as the stdio child. Embedded hosts can
    // still use negotiated HTTP without executing their own application as Kar.
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.file_name().is_none_or(|n| n != "kar") {
        return Err(crate::Error::Unsupported(
            "Kar CLI bridge is unavailable".into(),
        ));
    }
    let dir = PrivateDir::create(Path::new("/tmp"), "kar-mcp-bridge-", b"kar-stdio-mcp-v1")?;
    let path = dir.path().join("rpc");
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
        command: exe.to_string_lossy().into(),
        args: vec!["workbench-mcp-bridge".into(), path.to_string_lossy().into()],
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
