use super::super::{ProviderAuthOptions, within};
use crate::providers::ProviderErrorKind as Kind;
use crate::{
    Error, InstallationSnapshot, Result, Runtime, installation::Probe, process::OwnedProcess,
};
use reqwest::{Client, Method, StatusCode, Url, redirect::Policy};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::Mutex,
    time::Duration,
};

pub(super) struct Pool {
    server: Mutex<Option<Arc<Server>>>,
    program: PathBuf,
    probe: Probe,
    timeout: Duration,
    idle: Duration,
}

#[derive(Default)]
struct Activity {
    users: usize,
    revision: u64,
    timer: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for Activity {
    fn drop(&mut self) {
        if let Some(t) = self.timer.take() {
            t.abort();
        }
    }
}

pub(super) struct Lease {
    pool: Arc<Pool>,
    pub server: Arc<Server>,
    handle: tokio::runtime::Handle,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.server.activity.lock().unwrap();
        state.users -= 1;
        if state.users != 0 {
            return;
        }
        let revision = state.revision;
        let pool = Arc::downgrade(&self.pool);
        let server = Arc::downgrade(&self.server);
        let idle = self.pool.idle;
        state.timer = Some(self.handle.spawn(async move {
            tokio::time::sleep(idle).await;
            let Some(pool) = pool.upgrade() else { return };
            let mut current = pool.server.lock().await;
            let still_idle = current.as_ref().is_some_and(|s| {
                let state = s.activity.lock().unwrap();
                std::ptr::eq(Arc::as_ptr(s), server.as_ptr())
                    && state.revision == revision
                    && state.users == 0
            });
            if still_idle && let Some(s) = current.take() {
                s.stop().await;
            }
        }));
    }
}
impl Pool {
    pub async fn new(rt: &Runtime, opt: &ProviderAuthOptions) -> Result<Arc<Self>> {
        if rt.id != "opencode" {
            return Err(Kind::UnsupportedInstallation
                .error("OpenCode provider credentials require the OpenCode runtime"));
        }
        let mut probe = Probe::new(&opt.installation)?;
        let snap = within(opt.timeout, "OpenCode installation", async {
            Ok(probe.discover(rt).await)
        })
        .await?;
        let InstallationSnapshot::Available {
            command: Some(program),
            version: Some(version),
            ..
        } = snap
        else {
            return Err(Kind::UnsupportedInstallation.error(
                "OpenCode provider credentials require an installed OpenCode 2 executable",
            ));
        };
        if crate::account::release(&version).is_none_or(|(major, _, _)| major != 2) {
            return Err(
                Kind::VersionUnsupported.error("OpenCode 2 is required for provider credentials")
            );
        }
        probe.cwd = crate::account::profile_home(&probe)
            .ok_or_else(|| Error::Protocol("Missing OpenCode home directory".into()))?;
        Ok(Arc::new(Self {
            server: Mutex::new(None),
            program,
            probe,
            timeout: opt.timeout,
            idle: opt.idle_timeout,
        }))
    }
    pub async fn lease(self: &Arc<Self>) -> Result<Lease> {
        within(self.timeout, "OpenCode provider service startup", async {
            let mut current = self.server.lock().await;
            if let Some(s) = current.as_ref()
                && s.exited().await?
            {
                *current = None;
            }
            if current.is_none() {
                *current = Some(Arc::new(Server::start(self).await?));
            }
            let server = current.as_ref().unwrap().clone();
            {
                let mut state = server.activity.lock().unwrap();
                if let Some(timer) = state.timer.take() {
                    timer.abort();
                }
                state.users += 1;
                state.revision = state.revision.wrapping_add(1);
            }
            Ok(Lease {
                pool: self.clone(),
                server,
                handle: tokio::runtime::Handle::current(),
            })
        })
        .await
    }
    pub async fn close(&self) {
        let mut current = self.server.lock().await;
        if let Some(s) = current.take() {
            s.stop().await;
        }
    }
}

pub(super) struct Server {
    activity: std::sync::Mutex<Activity>,
    child: Mutex<OwnedProcess>,
    origin: Url,
    password: String,
    client: Client,
}
impl Server {
    async fn start(pool: &Pool) -> Result<Self> {
        let server = Self::launch(&pool.probe, &pool.program, pool.timeout, None).await?;
        server.await_integrations().await?;
        Ok(server)
    }

    pub(super) async fn launch(
        probe: &Probe,
        program: &Path,
        timeout: Duration,
        workspace: Option<&Path>,
    ) -> Result<Self> {
        let password = uuid::Uuid::new_v4().simple().to_string();
        let mut cmd = probe.command(program);
        cmd.args(["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"])
            .env("OPENCODE_PASSWORD", &password)
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .env_remove("OPENCODE_SERVER_USERNAME")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        cmd.env("BROWSER", "/usr/bin/true");
        let mut child = OwnedProcess::spawn(cmd)?;
        // OpenCode's --stdio mode closes the service on EOF, including a killed host.
        // Keep stdin in the Child while we own it.
        let mut reader = BufReader::new(child.inner().stdout.take().expect("piped stdout"));
        let mut line = Vec::new();
        let endpoint = loop {
            line.clear();
            let n = (&mut reader)
                .take(65537)
                .read_until(b'\n', &mut line)
                .await?;
            if n == 0 || n > 65536 {
                return Err(
                    Kind::StartupFailed.error("OpenCode did not announce its private service")
                );
            }
            let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if let Some(url) = frame["url"].as_str() {
                break Url::parse(url).ok();
            }
        };
        let origin = endpoint
            .filter(|u| {
                u.scheme() == "http"
                    && u.host_str() == Some("127.0.0.1")
                    && u.port().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.path() == "/"
                    && u.query().is_none()
                    && u.fragment().is_none()
            })
            .ok_or_else(|| Kind::StartupFailed.error("Unexpected OpenCode service address"))?;
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
        });
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(cwd) = workspace {
            let directory = cwd
                .to_str()
                .ok_or_else(|| Error::Protocol("Invalid OpenCode workspace path".into()))?;
            let encoded = percent_encoding::utf8_percent_encode(
                directory,
                percent_encoding::NON_ALPHANUMERIC,
            );
            headers.insert(
                "x-opencode-directory",
                encoded
                    .to_string()
                    .parse()
                    .map_err(|_| Error::Protocol("Invalid OpenCode workspace header".into()))?,
            );
        }
        Ok(Self {
            activity: Default::default(),
            child: Mutex::new(child),
            origin,
            password,
            client: Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .default_headers(headers)
                .timeout(timeout)
                .build()?,
        })
    }

    async fn await_integrations(&self) -> Result<()> {
        loop {
            let plugins = self.call(Method::GET, &["plugin"], None).await?;
            let integrations = self.call(Method::GET, &["integration"], None).await?;
            if [plugins, integrations].iter().all(|(s, v)| {
                *s == StatusCode::OK && v["data"].as_array().is_some_and(|l| !l.is_empty())
            }) {
                return Ok(());
            }
            if self.exited().await? {
                return Err(
                    Kind::StartupFailed.error("OpenCode provider service exited during startup")
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn exited(&self) -> Result<bool> {
        Ok(self.child.lock().await.exited()?)
    }
    pub(super) async fn stop(&self) {
        self.child.lock().await.stop().await;
    }
    pub async fn call(
        &self,
        method: Method,
        parts: &[&str],
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        self.call_query(method, parts, &[], body).await
    }

    pub(super) async fn call_query(
        &self,
        method: Method,
        parts: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut url = self.origin.clone();
        {
            let mut path = url.path_segments_mut().expect("HTTP origin");
            path.clear().push("api");
            for p in parts {
                if p.is_empty() || matches!(*p, "." | "..") {
                    return Err(Error::Protocol("Invalid OpenCode API identifier".into()));
                }
                path.push(p);
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut req = self
            .client
            .request(method, url)
            .basic_auth("opencode", Some(&self.password));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let transport = |e: reqwest::Error| {
            if e.is_timeout() {
                Kind::TimedOut.error("OpenCode provider request")
            } else {
                Kind::ServerFailed.error("OpenCode provider service stopped answering")
            }
        };
        let mut res = req.send().await.map_err(transport)?;
        let status = res.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(Kind::AuthenticationFailed
                .http("OpenCode rejected the private service password", status));
        }
        // Error documents can include keys or OAuth codes. Never return their text.
        if !status.is_success() {
            return Ok((status, Value::Null));
        }
        let mut bytes = Vec::new();
        while let Some(block) = res.chunk().await.map_err(transport)? {
            if bytes.len().saturating_add(block.len()) > 8 * 1024 * 1024 {
                return Err(Error::Protocol(
                    "OpenCode provider response is too large".into(),
                ));
            }
            bytes.extend_from_slice(&block);
        }
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::Protocol("Invalid OpenCode provider response".into()))?
        };
        Ok((status, v))
    }
    pub async fn request(
        &self,
        method: Method,
        parts: &[&str],
        body: Option<Value>,
    ) -> Result<Value> {
        let (s, v) = self.call(method, parts, body).await?;
        if !s.is_success() {
            return Err(Kind::Rejected.http("OpenCode refused the provider request", s));
        }
        Ok(v)
    }
}

/// provider.list finishes plugin activation, even if it returns an empty list.
pub(crate) async fn workspace_skills(
    probe: &Probe,
    program: &Path,
    timeout: Duration,
) -> Result<(Value, bool)> {
    let server = Server::launch(probe, program, timeout, Some(&probe.cwd)).await?;
    let read = async {
        async fn registry(server: &Server, route: &str) -> Result<Vec<Value>> {
            let mut response = server.request(Method::GET, &[route], None).await?;
            match response["data"].take() {
                Value::Array(rows) if rows.iter().all(Value::is_object) => Ok(rows),
                _ => Err(Error::Protocol(
                    "Invalid OpenCode workspace registry".into(),
                )),
            }
        }
        registry(&server, "provider").await?;
        let mut partial = false;
        for plugin in registry(&server, "plugin").await? {
            match plugin["state"]["status"].as_str() {
                Some("active") => (),
                Some("failed") => partial = true,
                _ => return Err(Error::Protocol("Unknown OpenCode plugin state".into())),
            }
        }
        Ok((Value::Array(registry(&server, "skill").await?), partial))
    }
    .await;
    server.stop().await;
    read
}
