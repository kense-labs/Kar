use crate::{Error, Result};
use futures_util::StreamExt;
use reqwest::{
    Client, Method, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    net::IpAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, Weak},
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot, watch},
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[derive(Clone)]
pub(super) struct Console {
    url: Url,
    token: HeaderValue,
    web_origin: Option<String>,
    client: Client,
}

impl Console {
    pub(super) fn new(url: &str, token: &str) -> Result<Self> {
        let url =
            Url::parse(url).map_err(|_| Error::Protocol("invalid Morph Console URL".into()))?;
        let loopback = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if url.scheme() != "http"
            || !loopback
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Protocol(
                "Morph publishes a loopback HTTP endpoint".into(),
            ));
        }
        if token.trim().is_empty() {
            return Err(Error::Protocol("missing Morph Console token".into()));
        }
        let mut token = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Protocol("invalid Morph Console token".into()))?;
        token.set_sensitive(true);
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            url,
            token,
            client,
            web_origin: None,
        })
    }
    fn route(&self, path: &[&str]) -> Url {
        let mut url = self.url.clone();
        url.path_segments_mut()
            .expect("HTTP URL")
            .pop_if_empty()
            .extend(path);
        url
    }
    pub(super) async fn call(
        &self,
        method: Method,
        path: &[&str],
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut request = self
            .client
            .request(method, self.route(path))
            .header(AUTHORIZATION, self.token.clone());
        if let Some(body) = body {
            request = request.json(body);
        }
        Self::decode(request.send().await?).await
    }
    pub(super) async fn read_task(&self, id: &str) -> Result<Value> {
        let response = self
            .client
            .get(self.route(&["tasks", id]))
            .header(AUTHORIZATION, self.token.clone())
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Value::Null);
        }
        Self::decode(response).await
    }
    async fn decode(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let raw = response.bytes().await?;
        let value = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&raw)
                .unwrap_or_else(|_| String::from_utf8_lossy(&raw).into_owned().into())
        };
        if !status.is_success() {
            return Err(Error::Refused(format!(
                "Morph HTTP {status}: {}",
                value["error"]
                    .as_str()
                    .or(value.as_str())
                    .unwrap_or("request rejected")
            )));
        }
        Ok(value)
    }
    pub(super) async fn snapshots(&self, task: String, tx: mpsc::Sender<(String, Value)>) {
        let mut url = self.route(&["stream", "ws"]);
        url.set_scheme("ws").expect("WebSocket scheme");
        url.query_pairs_mut().append_pair("task_id", &task);
        loop {
            let Ok(mut req) = url.as_str().into_client_request() else {
                return;
            };
            req.headers_mut().insert(AUTHORIZATION, self.token.clone());
            if let Ok((mut socket, _)) = tokio_tungstenite::connect_async(req).await {
                while let Some(Ok(message)) = socket.next().await {
                    if message.is_close() {
                        break;
                    }
                    if (message.is_text() || message.is_binary())
                        && let Ok(frame) = serde_json::from_slice::<Value>(&message.into_data())
                        && frame["task_id"].as_str() == Some(&task)
                        && tx.send((task.clone(), frame)).await.is_err()
                    {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

#[derive(Deserialize)]
struct Endpoint {
    #[serde(default)]
    web_url: Option<String>,
    url: String,
    token: String,
}

async fn discover(dir: &Path) -> Option<Console> {
    let bytes = tokio::fs::read(dir.join("console/runtime.json"))
        .await
        .ok()?;
    let endpoint: Endpoint = serde_json::from_slice(&bytes).ok()?;
    let mut console = Console::new(&endpoint.url, &endpoint.token).ok()?;
    console.web_origin = endpoint
        .web_url
        .and_then(|s| Url::parse(&s).ok())
        .map(|u| u.origin().ascii_serialization());
    let health = tokio::time::timeout(
        Duration::from_millis(700),
        console.call(Method::GET, &["health"], None),
    )
    .await
    .ok()?
    .ok()?;
    (health["mode"] == "console").then_some(console)
}

struct Owned {
    process_root: Option<crate::resources::Root>,
    console: Console,
    _stop: oneshot::Sender<()>,
    exit: watch::Receiver<Option<String>>,
}
type Pool = HashMap<PathBuf, Weak<Owned>>;
static CONSOLES: LazyLock<Mutex<Pool>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) struct Lease {
    pub console: Console,
    owned: Option<Arc<Owned>>,
}
impl Lease {
    pub(super) fn process_root(&self) -> Option<crate::resources::Root> {
        self.owned.as_ref()?.process_root.clone()
    }

    pub fn attached(console: Console) -> Self {
        Self {
            console,
            owned: None,
        }
    }
    fn owned(owner: Arc<Owned>) -> Self {
        Self {
            console: owner.console.clone(),
            owned: Some(owner),
        }
    }
    pub async fn acquire(program: &Path, dir: &Path, config: Option<&Path>) -> Result<Self> {
        tokio::fs::create_dir_all(dir).await?;
        let dir = tokio::fs::canonicalize(dir).await?;
        let mut pool = CONSOLES.lock().await;
        pool.retain(|_, owner| owner.strong_count() > 0);
        if let Some(owner) = pool.get(&dir).and_then(Weak::upgrade)
            && owner.exit.borrow().is_none()
        {
            return Ok(Self::owned(owner));
        }
        if let Some(console) = discover(&dir).await {
            return Ok(Self::attached(console));
        }
        let mut cmd = Command::new(program);
        if let Some(config) = config {
            cmd.arg("--config").arg(config);
        }
        cmd.args(["console", "serve", "--console-listen", "127.0.0.1:0"])
            .current_dir(&dir)
            .env("MISTER_MORPH_FILE_STATE_DIR", &dir)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("piped Morph stdout");
        let (ready_tx, ready) = oneshot::channel();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            let mut sender = Some(ready_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(origin) = line
                    .strip_prefix("console serve listening on ")
                    .and_then(|url| Url::parse(url.trim()).ok())
                    .map(|u| u.origin().ascii_serialization())
                    && let Some(sender) = sender.take()
                {
                    let _ = sender.send(origin);
                    // Drain any further output without retaining log lines.
                    let _ = tokio::io::copy(&mut lines.into_inner(), &mut tokio::io::sink()).await;
                    break;
                }
            }
        });
        let process_root =
            crate::resources::Root::new(child.id(), Some(crate::ResourceScope::SharedProcess));
        let (stop, stopped) = oneshot::channel();
        let (status, mut exit) = watch::channel(None);
        tokio::spawn(supervise(child, stopped, status));
        loop {
            if let Some(console) = discover(&dir).await {
                // A different process can win the Console's own lock while we start.
                if exit.borrow().is_some() {
                    return Ok(Self::attached(console));
                }
                // Two processes can race to publish runtime.json. Only attribute
                // memory when the child itself announced this Console endpoint.
                let process_root =
                    process_root
                        .zip(console.web_origin.clone())
                        .map(|(root, expected)| {
                            root.confirm_with(async move {
                                ready.await.is_ok_and(|origin| origin == expected)
                            })
                        });
                let owner = Arc::new(Owned {
                    process_root,
                    console,
                    _stop: stop,
                    exit,
                });
                pool.insert(dir, Arc::downgrade(&owner));
                return Ok(Self::owned(owner));
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(90)) => {}
                _ = exit.changed() => {
                    if let Some(console) = discover(&dir).await {return Ok(Self::attached(console));}
                    return Err(Error::Closed(format!("Morph Console exited before publishing its endpoint: {:?}",*exit.borrow())));
                }
            }
        }
    }
    pub fn exit(&self) -> Option<watch::Receiver<Option<String>>> {
        self.owned.as_ref().map(|o| o.exit.clone())
    }
    pub async fn release(self) {
        if let Some(owner) = self.owned {
            let _guard = CONSOLES.lock().await;
            let mut exit = owner.exit.clone();
            let last = Arc::strong_count(&owner) == 1;
            drop(owner);
            if last && exit.borrow().is_none() {
                let _ = exit.changed().await;
            }
        }
    }
}
async fn supervise(
    mut child: Child,
    stop: oneshot::Receiver<()>,
    status: watch::Sender<Option<String>>,
) {
    let result = tokio::select! {
        result = child.wait() => result,
        _ = stop => {
            if let Some(pid) = child.id() {
                #[cfg(unix)]
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
                #[cfg(windows)]
                let _ = Command::new("taskkill").args(["/PID",&pid.to_string(),"/T","/F"]).output().await;
            }
            let _ = child.start_kill();
            child.wait().await
        }
    };
    status.send_replace(Some(match result {
        Ok(status) => status.to_string(),
        Err(err) => err.to_string(),
    }));
}
