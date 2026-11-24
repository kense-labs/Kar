//! Pull Pi's current root request from its own SDK process. The socket is private
//! to this open, so snapshots cannot become chat records or survive a resume.
use super::{ModelRequest, mask_body, masked};
use crate::{Error, Result, SessionOptions};
use serde::Deserialize;
use serde_json::Value;
use std::{fs, io, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{mpsc, oneshot, watch},
};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
const PLUGIN: Option<&[u8]> = Some(include_bytes!(concat!(
    env!("OUT_DIR"),
    "/pi-requests.node"
)));
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const PLUGIN: Option<&[u8]> = None;

fn unavailable() -> Error {
    Error::Unsupported(
        "Pi model_requests requires the npm Pi SDK and a Node host with native addon support"
            .into(),
    )
}
fn unsupported_api() -> Error {
    Error::Unsupported("Pi uses an API without verified model request capture".into())
}
fn format(api: &str) -> Result<&'static str> {
    match api {
        "anthropic-messages" => Ok("anthropic.messages"),
        "openai-completions" => Ok("openai.chat"),
        "openai-responses" => Ok("openai.responses"),
        _ => Err(unsupported_api()),
    }
}

type Query = (u8, oneshot::Sender<Vec<u8>>);
pub(crate) struct Reader {
    queue: mpsc::Sender<Query>,
    closed: watch::Sender<bool>,
    secrets: Vec<String>,
}
pub(crate) struct Owner {
    pub reader: Arc<Reader>,
    _files: tempfile::TempDir,
    dist: PathBuf,
    plugin: PathBuf,
    port: u16,
    token: String,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.reader.close();
    }
}

impl Owner {
    pub async fn new(launch: &crate::pi_sdk::Launch, options: &SessionOptions) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("kar-pi-sdk-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let files = builder.tempdir()?;
        let plugin = files.path().canonicalize()?.join("pi-requests.node");
        fs::write(&plugin, PLUGIN.ok_or_else(unavailable)?)?;
        launch.check_plugin(&plugin, options).await?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (queue, rx) = mpsc::channel(8);
        let (closed, mut stop) = watch::channel(false);
        let nonce = token.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = stop.changed() => {}
                _ = run(listener,nonce,rx) => {}
            }
        });
        Ok(Self {
            reader: Arc::new(Reader {
                queue,
                closed,
                secrets: crate::options::session_secrets(options),
            }),
            _files: files,
            dist: launch.dist.clone(),
            plugin,
            port,
            token,
        })
    }
    pub fn configure(&self, cmd: &mut Command) {
        cmd.arg("--extension")
            .arg(&self.plugin)
            .env("KAR_RS_PI_CAPTURE_PORT", self.port.to_string())
            .env("KAR_RS_PI_CAPTURE_TOKEN", &self.token)
            .env(
                "KAR_RS_PI_CAPTURE_SDK",
                self.dist.join("core/agent-session.js"),
            )
            .env("KAR_RS_PI_CAPTURE_PLUGIN", &self.plugin);
    }
}

async fn authenticated(listener: TcpListener, token: &str) -> io::Result<TcpStream> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut bytes = [0; 32];
        if tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut bytes))
            .await
            .is_ok_and(|r| r.is_ok())
            && bytes == token.as_bytes()
        {
            return Ok(stream);
        }
    }
}

async fn run(
    listener: TcpListener,
    token: String,
    mut queries: mpsc::Receiver<Query>,
) -> io::Result<()> {
    let mut stream = authenticated(listener, &token).await?;
    while let Some((kind, reply)) = queries.recv().await {
        // Finish a whole frame even if the caller abandons its read.
        stream.write_u8(kind).await?;
        let len = usize::try_from(stream.read_u64().await?).map_err(io::Error::other)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(len).map_err(io::Error::other)?;
        buf.resize(len, 0);
        stream.read_exact(&mut buf).await?;
        let _ = reply.send(buf);
    }
    Ok(())
}

impl Reader {
    pub fn close(&self) {
        self.closed.send_replace(true);
    }
    async fn query(&self, kind: u8) -> Result<Vec<u8>> {
        let mut stop = self.closed.subscribe();
        if *stop.borrow_and_update() {
            return Ok(b"null".to_vec());
        }
        let (tx, rx) = oneshot::channel();
        tokio::select! {
            biased;
            _ = stop.changed() => Ok(b"null".to_vec()),
            result = async {
                self.queue.send((kind,tx)).await.map_err(|_| unavailable())?;
                rx.await.map_err(|_| unavailable())
            } => result
        }
    }
    pub async fn verify(&self, model: &Value) -> Result<()> {
        let status = tokio::time::timeout(Duration::from_secs(2), self.query(b'V'))
            .await
            .map_err(|_| unavailable())??;
        if *self.closed.borrow() {
            return Err(Error::Closed(
                "Pi exited during request capture setup".into(),
            ));
        }
        match status.as_slice() {
            b"null" => format(model["api"].as_str().unwrap_or("")).map(|_| ()),
            b"!api" => Err(unsupported_api()),
            _ => Err(unavailable()),
        }
    }
    pub async fn read(&self) -> Result<Option<ModelRequest>> {
        let bytes = self.query(b'R').await?;
        if *self.closed.borrow() || bytes == b"null" {
            return Ok(None);
        }
        match bytes.as_slice() {
            b"!api" => return Err(unsupported_api()),
            b"!json" => {
                return Err(Error::Protocol(
                    "Pi's latest request did not expose a JSON body".into(),
                ));
            }
            _ => {}
        }
        #[derive(Deserialize)]
        struct Snapshot {
            at: i64,
            model: String,
            provider: String,
            api: String,
        }
        let split = bytes
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| Error::Protocol("Pi request is missing metadata".into()))?;
        let raw: Snapshot = serde_json::from_slice(&bytes[..split])?;
        let mut body: Value = serde_json::from_slice(&bytes[split + 1..])?;
        mask_body(&mut body, &self.secrets);
        let at = jiff::Timestamp::from_millisecond(raw.at)
            .map_err(|_| Error::Protocol("Invalid Pi request timestamp".into()))?;
        let request = ModelRequest {
            at: at.to_string(),
            format: format(&raw.api)?.into(),
            model: Some(masked(&raw.model, &self.secrets)),
            provider: Some(masked(&raw.provider, &self.secrets)),
            body: Some(body),
            withheld: vec![],
        };
        Ok((!*self.closed.borrow()).then_some(request))
    }
}
