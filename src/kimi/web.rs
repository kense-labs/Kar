use crate::{Error, Result, Runtime, SessionOptions};
use crate::{installation::Probe, process::OwnedProcess};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::LazyLock,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::TcpStream,
    process::Command,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) struct Web {
    child: OwnedProcess,
    client: Client,
    origin: String,
    pub token: String,
}

pub(crate) fn segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

pub(super) async fn candidate(rt: &Runtime, opts: &SessionOptions) -> Result<bool> {
    let output = tokio::time::timeout(
        opts.handshake_timeout,
        opts.command(&rt.program)?
            .arg("--version")
            .current_dir(&opts.cwd)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| Error::Timeout("Kimi version probe".into()))??;
    let out = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    let v = String::from_utf8_lossy(&out)
        .split_whitespace()
        .find_map(|s| semver::Version::parse(s.trim_start_matches('v')).ok());
    Ok(output.status.success() && v.as_ref().is_some_and(super::kap_candidate))
}
impl Web {
    pub(super) fn process_root(&mut self) -> Option<crate::resources::Root> {
        crate::resources::Root::new(self.child.inner().id(), None)
    }

    pub async fn start(rt: &Runtime, opts: &SessionOptions) -> Result<Option<Self>> {
        let mut cmd = opts.command(&rt.program)?;
        for arg in &rt.args {
            let arg = arg
                .to_str()
                .ok_or_else(|| Error::Unsupported("non-UTF8 Kimi argument".into()))?;
            if matches!(
                arg.split('=').next(),
                Some(
                    "--json"
                        | "--host"
                        | "--port"
                        | "--open"
                        | "--no-open"
                        | "--socket"
                        | "--remote-control"
                        | "--rc"
                        | "--dangerous-bypass-auth"
                        | "--allow-remote-shutdown"
                        | "--debug-endpoints"
                        | "--managed-by"
                        | "--"
                )
            ) {
                return Err(Error::Unsupported(
                    "overriding a Kimi Web transport option".into(),
                ));
            }
        }
        cmd.args(["web", "--json", "--host", "127.0.0.1", "--port", "0"])
            .args(&rt.args)
            .current_dir(&opts.cwd);
        let home = crate::options::env_path(opts, "KIMI_CODE_HOME")
            .or_else(|| opts.session_home().map(|p| p.join(".kimi-code")))
            .ok_or_else(|| Error::Protocol("Kimi home is unavailable".into()))?;
        Self::launch(cmd, &opts.cwd.join(home), opts.handshake_timeout, false).await
    }

    pub(crate) async fn for_query(
        probe: &Probe,
        program: &Path,
        home: &Path,
        timeout: Duration,
    ) -> Result<Self> {
        let mut cmd = probe.command(program);
        cmd.args(["web", "--no-open", "--host", "127.0.0.1", "--port", "0"])
            .env("KIMI_CODE_MODEL_CATALOG_REFRESH_ON_START", "0")
            .env("KIMI_CODE_MODEL_CATALOG_REFRESH_INTERVAL_MS", "0");
        Self::launch(cmd, home, timeout, true)
            .await?
            .ok_or_else(|| Error::Protocol("Kimi Web omitted its query endpoint".into()))
    }

    async fn launch(
        mut cmd: Command,
        home: &Path,
        timeout: Duration,
        banner: bool,
    ) -> Result<Option<Self>> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::Protocol("Invalid Kimi Web startup timeout".into()))?;
        let _guard = tokio::time::timeout_at(deadline, super::startup::acquire(home.to_owned()))
            .await
            .map_err(|_| Error::Timeout("Kimi Web startup".into()))??;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = OwnedProcess::spawn(cmd)?;
        let mut web = Self {
            child,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(timeout)
                .build()?,
            origin: String::new(),
            token: String::new(),
        };
        let mut lines = BufReader::new(web.child.inner().stdout.take().unwrap()).lines();
        let stderr = web.child.inner().stderr.take().unwrap();
        let diagnostics = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut prefix = Vec::new();
            let _ = (&mut reader).take(64 * 1024).read_to_end(&mut prefix).await;
            let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
            let s = String::from_utf8_lossy(&prefix).to_ascii_lowercase();
            s.contains("--json")
                && [
                    "unknown option",
                    "unrecognized option",
                    "unexpected argument",
                    "unknown flag",
                ]
                .iter()
                .any(|e| s.contains(e))
        });
        let ready = tokio::time::timeout_at(deadline, async {
            while let Some(line) = lines.next_line().await? {
                if banner {
                    static URL: LazyLock<regex::Regex> = LazyLock::new(|| {
                        regex::Regex::new(r"(http://127\.0\.0\.1:[0-9]+)/?#token=([^\s\x1b]+)")
                            .unwrap()
                    });
                    if let Some(c) = URL.captures(&line) {
                        return Ok(Some(json!({"origin":&c[1],"token":&c[2]})));
                    }
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if v.get("origin").is_some() {
                    return Ok::<_, std::io::Error>(Some(v));
                }
            }
            Ok(None)
        })
        .await
        .map_err(|_| Error::Timeout("Kimi Web startup".into()))??;
        let Some(ready) = ready else {
            if tokio::time::timeout(Duration::from_secs(1), diagnostics)
                .await
                .is_ok_and(|r| r.unwrap_or(false))
            {
                return Ok(None);
            }
            return Err(Error::Closed(
                "Kimi Web exited before announcing its endpoint".into(),
            ));
        };
        let origin = ready["origin"].as_str().unwrap_or_default();
        let port = origin
            .strip_prefix("http://127.0.0.1:")
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p != 0);
        let Some(port) = port else {
            return Err(Error::Protocol("invalid Kimi Web loopback endpoint".into()));
        };
        if !banner
            && (ready["ws_url"] != format!("ws://127.0.0.1:{port}/api/v1/ws")
                || ready["kimi_agent_protocol_version"]
                    .as_str()
                    .is_none_or(str::is_empty))
        {
            return Err(Error::Protocol(
                "invalid Kimi Web endpoint announcement".into(),
            ));
        }
        web.origin = origin.into();
        web.token = ready["token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("missing Kimi Web token".into()))?
            .into();
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Ok(Some(web))
    }
    pub(crate) async fn workspace(
        &self,
        cwd: &Path,
        dirs: Option<&[PathBuf]>,
    ) -> Result<(String, bool)> {
        let catalog = self.request(Method::GET, "/workspaces", None).await?;
        let rows = catalog["items"]
            .as_array()
            .or_else(|| catalog["workspaces"].as_array())
            .ok_or_else(|| Error::Protocol("Invalid Kimi workspace catalog".into()))?;
        let ids = rows
            .iter()
            .map(|r| {
                r["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::Protocol("Missing Kimi workspace id".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut body = json!({"root":cwd});
        if let Some(dirs) = dirs {
            body["additional_dirs"] = json!(dirs);
        }
        let result = self
            .request(Method::POST, "/workspaces", Some(body))
            .await?;
        let id = result["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("Missing Kimi workspace id".into()))?;
        Ok((id.into(), !ids.contains(&id)))
    }
    pub async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        self.session_request(method, path, body, None).await
    }
    pub(super) async fn session_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        opening: Option<(&SessionOptions, &[String])>,
    ) -> Result<Value> {
        let is_post = method == Method::POST;
        let url = Url::parse(&format!("{}/api/v1{path}", self.origin))
            .map_err(|_| Error::Protocol("invalid Kimi API path".into()))?;
        let mut req = self.client.request(method, url).bearer_auth(&self.token);
        if let Some(v) = body {
            req = req.json(&v);
        }
        let response = req.send().await?;
        let status = response.status();
        let v: Value = response.json().await?;
        if !status.is_success() || v["code"] != 0 {
            if let Some((opts, secrets)) = opening
                && let Some(id) = opts.resume.as_deref()
            {
                let code = v["code"].as_i64();
                let method = if is_post {
                    "POST /sessions/{id}:resume"
                } else {
                    "GET /sessions/{id}"
                };
                let cause = || crate::NativeCause {
                    method: method.into(),
                    native: v.clone(),
                };
                let message = v["msg"].as_str().unwrap_or("Kimi refused session resume");
                if code == Some(40401) || status.as_u16() == 404 {
                    return Err(crate::OpenError::SessionNotFound {
                        session_id: id.into(),
                        message: message.into(),
                        cause: cause(),
                    }
                    .redacted(secrets));
                }
                if is_post && (matches!(code, Some(40931 | 40901)) || status.as_u16() == 409) {
                    return Err(crate::OpenError::SessionUnavailable {
                        session_id: id.into(),
                        message: message.into(),
                        reason: crate::UnavailableReason::InUse,
                        cause: cause(),
                    }
                    .redacted(secrets));
                }
            }
            if is_post
                && let Some((opts, secrets)) = opening
                && let Some(message) = v["msg"].as_str()
            {
                let lower = message.to_ascii_lowercase();
                if opts.settings.effort.is_some()
                    && ["thinking", "effort", "reasoning"]
                        .iter()
                        .any(|k| lower.contains(k))
                {
                    return Err(Error::Unsupported(
                        "Kimi rejected the requested thought level".into(),
                    ));
                }
                if let Some(wanted) = &opts.model
                    && lower
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|s| s == "model")
                {
                    return Err(crate::OpenError::model_confirmation(
                        format!("Kimi rejected model {wanted:?}: {message}"),
                        wanted,
                        None,
                    )
                    .redacted(secrets));
                }
            }
            if is_post && path.ends_with(":cancel") {
                let message = v["msg"].as_str().unwrap_or("").to_ascii_lowercase();
                if v["code"] == 40902
                    || ["no active turn", "no running turn"]
                        .iter()
                        .any(|s| message.contains(s))
                    || message == "not running"
                    || (message.contains("turn") && message.contains("not running"))
                {
                    return Err(Error::Refused("no active turn".into()));
                }
            }
            return Err(Error::Refused(format!(
                "Kimi API returned HTTP {} and code {:?}",
                status.as_u16(),
                v["code"].as_i64()
            )));
        }
        Ok(v["data"].clone())
    }
    pub async fn socket(&self) -> Result<Socket> {
        let url = self.origin.replacen("http:", "ws:", 1) + "/api/v1/ws";
        let mut req = url
            .into_client_request()
            .map_err(|_| Error::Protocol("invalid Kimi socket URL".into()))?;
        req.headers_mut().insert(
            "sec-websocket-protocol",
            format!("kimi-code.bearer.{}", self.token)
                .parse()
                .map_err(|_| Error::Protocol("invalid Kimi socket credential".into()))?,
        );
        Ok(tokio_tungstenite::connect_async(req)
            .await
            .map_err(|_| Error::Closed("Kimi WebSocket handshake failed".into()))?
            .0)
    }
    pub async fn stop(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let graceful = tokio::time::timeout(Duration::from_secs(2), async {
            self.request(Method::POST, "/shutdown", None).await?;
            self.child.wait().await.map_err(Error::from)
        })
        .await;
        if let Ok(Ok(status)) = graceful {
            return Ok(status);
        }
        self.child.stop_with_status().await
    }
}
pub(super) async fn send(ws: &mut Socket, v: &Value) -> Result<()> {
    ws.send(Message::Text(v.to_string().into()))
        .await
        .map_err(|_| Error::Closed("Kimi WebSocket write failed".into()))
}
pub(super) async fn read(ws: &mut Socket) -> Result<Value> {
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| Error::Closed("Kimi WebSocket closed".into()))?
            .map_err(|_| Error::Closed("Kimi WebSocket read failed".into()))?;
        let v: Value = match msg {
            Message::Text(t) => serde_json::from_str::<Value>(&t)?,
            Message::Binary(b) => serde_json::from_slice(&b)?,
            Message::Close(_) => return Err(Error::Closed("Kimi WebSocket closed".into())),
            Message::Ping(b) => {
                ws.send(Message::Pong(b))
                    .await
                    .map_err(|_| Error::Closed("Kimi WebSocket closed".into()))?;
                continue;
            }
            _ => continue,
        };
        if v["type"] == "ping" {
            send(ws, &json!({"type":"pong"})).await?;
            continue;
        }
        return Ok(v);
    }
}
