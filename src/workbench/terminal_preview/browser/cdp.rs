//! A private DevTools connection to Kar's own sandboxed Chromium profile.
use crate::{Error, Result, process::OwnedProcess};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{net::TcpStream, process::Command};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

fn executable() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KAR_PREVIEW_BROWSER") {
        let path = PathBuf::from(path);
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err(Error::Unsupported(
            "KAR_PREVIEW_BROWSER must name an installed browser executable".into(),
        ));
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
    .ok_or_else(|| {
        Error::Unsupported("set KAR_PREVIEW_BROWSER to an installed Chromium executable".into())
    })
}

pub(super) struct Connection {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    sequence: u64,
    pub(super) observe_console: bool,
    pub(super) console:Vec<Value>,
}
impl Connection {
    async fn connect(url: &str) -> Result<Self> {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|_| Error::Closed("Chromium debugging connection failed".into()))?;
        Ok(Self {
            socket,
            sequence: 0,
            observe_console:false,console:vec![],
        })
    }
    pub(super) async fn request(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        let mut req = json!({"id":id,"method":method,"params":params});
        if let Some(session) = session {
            req["sessionId"] = session.into();
        }
        let call = async {
            self.socket
                .send(Message::Text(req.to_string().into()))
                .await
                .map_err(|_| Error::Closed("Chromium connection closed".into()))?;
            while let Some(frame) = self.socket.next().await {
                match frame.map_err(|_| Error::Closed("Chromium connection failed".into()))? {
                    Message::Text(text) => {
                        let mut reply: Value = serde_json::from_str(&text)?;
                        if self.observe_console && reply["method"]=="Runtime.consoleAPICalled" && self.console.len()<20{
                            let text=reply["params"]["args"].as_array().into_iter().flatten().filter_map(|a|a.get("value").or_else(||a.get("description"))).map(|v|v.as_str().map(str::to_owned).unwrap_or_else(||v.to_string())).collect::<Vec<_>>().join(" ");
                            self.console.push(json!({"level":reply["params"]["type"],"text":text.chars().take(500).collect::<String>()}));
                        }
                        if reply["id"] != id {
                            continue;
                        }
                        if reply.get("error").is_some() {
                            return Err(Error::Refused(format!("Chromium refused {method}")));
                        }
                        if !reply["result"].is_object() {
                            return Err(Error::Protocol(format!(
                                "Chromium omitted {method} result"
                            )));
                        }
                        return Ok(reply["result"].take());
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            Err(Error::Closed("Chromium exited".into()))
        };
        tokio::time::timeout(Duration::from_secs(12), call)
            .await
            .map_err(|_| Error::Timeout(format!("Chromium {method}")))?
    }
}

pub(super) struct Browser {
    pub conn: Connection,
    pub network: super::network::Network,
    pub process: OwnedProcess,
}
impl Browser {
    pub(super) async fn open(dir: &Path) -> Result<Self> {
        let exe = executable()?;
        let network = super::network::Network::listen().await?;
        tokio::fs::create_dir_all(dir).await?;
        let marker = dir.join("DevToolsActivePort");
        match tokio::fs::remove_file(&marker).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut cmd = Command::new(exe);
        let mut data = std::ffi::OsString::from("--user-data-dir=");
        data.push(dir);
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{}", network.port))
            .args([
                "--proxy-bypass-list=<-loopback>",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--disable-quic",
            ])
            .arg(data)
            .args([
                "--headless",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-sync",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-breakpad",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "about:blank",
            ])
            .env_remove("KAR_WORKBENCH_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = OwnedProcess::spawn(cmd)?;
        let start = async {
            loop {
                if process.inner().try_wait()?.is_some() {
                    return Err(Error::Closed(
                        "the preview browser exited during startup".into(),
                    ));
                }
                if let Ok(data) = tokio::fs::read_to_string(&marker).await {
                    let mut lines = data.lines();
                    if let (Some(port), Some(path)) = (
                        lines.next().and_then(|p| p.parse::<u16>().ok()),
                        lines.next(),
                    ) && let Some(key) = path.strip_prefix("/devtools/browser/")
                        && uuid::Uuid::parse_str(key).is_ok()
                    {
                        let url = format!("ws://127.0.0.1:{port}{path}");
                        if let Ok(conn) = Connection::connect(&url).await {
                            return Ok(conn);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        };
        let conn = tokio::time::timeout(Duration::from_secs(15), start)
            .await
            .map_err(|_| Error::Timeout("launching Chromium".into()))??;
        Ok(Self {
            conn,
            process,
            network,
        })
    }
}
