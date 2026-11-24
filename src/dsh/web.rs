use crate::{Error, Result, Runtime, SessionOptions};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpStream,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use uuid::Uuid;

pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub(super) struct Web {
    child: crate::process::OwnedProcess,
    client: Client,
    image_addon: Option<tempfile::NamedTempFile>,
    pub timeout: Duration,
    base: String,
    cookie: String,
    pub file: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    pub(super) patch: super::profile::Patch,
}
impl Web {
    pub(super) fn images(&self) -> super::images::ImageStore<'_> {
        super::images::ImageStore {
            client: &self.client,
            base: &self.base,
            cookie: &self.cookie,
            timeout: self.timeout,
            enabled: self.image_addon.is_some(),
        }
    }
    pub(super) async fn confirm(&mut self, receipt: super::profile::Receipt) -> Result<()> {
        receipt.confirm(self.child.wait(), self.timeout).await
    }

    pub(super) fn process_root(&self) -> Option<crate::resources::Root> {
        crate::resources::Root::new(self.child.id(), None)
    }

    pub async fn start(
        rt: &Runtime,
        opts: &SessionOptions,
        mut rows: Vec<Value>,
        version: Option<&str>,
    ) -> Result<Self> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(opts.handshake_timeout)
            .build()?;
        let mut cmd = opts.command(&rt.program)?;
        cmd.args(["--profile", "web"]);
        let image_addon = super::images::install(opts, &mut rows, &mut cmd)?;
        let patch = super::profile::Patch::prepare(rt, opts, version, &mut cmd, rows).await?;
        cmd.args(["--no-open", "--host", "127.0.0.1", "--port", "0"])
            .current_dir(&opts.cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped());
        let _startup = super::profile::session_lock(opts, "web").await?;
        let child = crate::process::OwnedProcess::spawn(cmd)?;
        opts.bind_bridge_process(crate::resources::Root::new(child.id(), None).as_ref());
        let mut web = Self {
            child,
            timeout: opts.handshake_timeout,
            client,
            base: String::new(),
            cookie: String::new(),
            file: Default::default(),
            patch,
            image_addon,
        };
        let out = web.child.inner().stdout.take().unwrap();
        let mut lines = BufReader::new(out).lines();
        let url = tokio::time::timeout(opts.handshake_timeout, async {
            while let Some(line) = lines.next_line().await? {
                if let Some(raw) = line.strip_prefix("dsh web: http://") {
                    let u = Url::parse(&format!("http://{raw}"))
                        .map_err(|_| Error::Protocol("invalid DSH launch URL".into()))?;
                    if u.host_str() != Some("127.0.0.1")
                        || u.port().is_none()
                        || !u.username().is_empty()
                        || u.password().is_some()
                        || u.path() != "/"
                        || u.fragment().is_some()
                    {
                        return Err(Error::Protocol("invalid DSH loopback endpoint".into()));
                    }
                    return Ok(u);
                }
            }
            Err(Error::Closed(
                "DSH exited before announcing its Web endpoint".into(),
            ))
        })
        .await
        .map_err(|_| Error::Timeout("DSH Web startup".into()))??;
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut lines.into_inner(), &mut tokio::io::sink()).await;
        });
        if let Some(path) = web.patch.receipt()
            && let Err(error) = web.confirm(path).await
        {
            let _ = web.stop().await;
            return Err(error);
        }
        web.base = url.origin().ascii_serialization();
        let token = url
            .query_pairs()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("DSH omitted its launch token".into()))?;
        let response = web
            .client
            .get(format!("{}/connect", web.base))
            .timeout(web.timeout)
            .query(&[("token", token.as_str()), ("interactions", "answer")])
            .send()
            .await
            .map_err(|e| Error::Http(e.without_url()))?;
        if response.status() != reqwest::StatusCode::FOUND {
            return Err(Error::Refused("DSH API authentication failed".into()));
        }
        web.cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.split(';').next())
            .filter(|s| s.contains('='))
            .ok_or_else(|| Error::Protocol("DSH omitted its API cookie".into()))?
            .to_owned();
        Ok(web)
    }
    pub async fn call(&self, method: &str, args: Value) -> Result<Value> {
        self.rpc_future(method, args, Some(self.timeout)).await
    }

    pub fn rpc_future(
        &self,
        method: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> futures_util::future::BoxFuture<'static, Result<Value>> {
        let id = Uuid::new_v4().to_string();
        let mut request = self.client.post(format!("{}/api/{method}", self.base))
            .header(header::COOKIE,&self.cookie)
            .json(&json!({"type":"client-request","rpcId":id,"method":method,"payload":{"args":args}}));
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let method = method.to_owned();
        Box::pin(async move {
            let res = request
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            if res["rpcId"] != id {
                return Err(Error::Protocol(
                    "DSH returned a different request id".into(),
                ));
            }
            let r = &res["result"];
            match r["ok"].as_bool() {
                Some(true) => Ok(r["value"].clone()),
                Some(false) => Err(Error::Refused(format!(
                    "DSH {method}: {}",
                    r["error"]["code"].as_str().unwrap_or("refused")
                ))),
                None => Err(Error::Protocol("invalid DSH RPC response".into())),
            }
        })
    }
    pub async fn request(&self, method: &str, request: Value) -> Result<Value> {
        self.call(method, json!({"request":request})).await
    }
    pub async fn socket(&self) -> Result<Socket> {
        let mut req = format!("{}/api/remote.mux", self.base.replacen("http:", "ws:", 1))
            .into_client_request()
            .map_err(|_| Error::Protocol("invalid DSH websocket URL".into()))?;
        req.headers_mut().insert(
            header::COOKIE,
            self.cookie
                .parse()
                .map_err(|_| Error::Protocol("invalid DSH cookie".into()))?,
        );
        Ok(tokio_tungstenite::connect_async(req)
            .await
            .map_err(|_| Error::Closed("DSH WebSocket handshake failed".into()))?
            .0)
    }
    pub async fn session_file(&self, id: &str) -> Result<Value> {
        // The first section comes directly from sessionPersistence.stat(). Its
        // opening fence is longer than every backtick run in the physical path.
        let mut response = self
            .client
            .get(format!("{}/api/session.markdown", self.base))
            .timeout(self.timeout)
            .header(header::COOKIE, &self.cookie)
            .query(&[("sessionId", id)])
            .send()
            .await?
            .error_for_status()?;
        let mut prefix = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            prefix.extend_from_slice(&chunk);
            if let Some(path) = source_header(&prefix) {
                let mut cache = self.file.lock().unwrap();
                *cache = path.map(str::to_owned);
                return Ok(serde_json::json!(*cache));
            }
            if prefix.len() > 128 * 1024 {
                return Err(Error::Protocol(
                    "DSH session export header is too long".into(),
                ));
            }
        }
        prefix.push(b'\n');
        let path = source_header(&prefix).flatten().map(str::to_owned);
        *self.file.lock().unwrap() = path.clone();
        Ok(serde_json::json!(path))
    }
    pub async fn stop(&mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        #[cfg(unix)]
        if let Ok(Ok(status)) =
            tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await
        {
            return Ok(status);
        }
        self.child.stop_with_status().await
    }
}
pub(super) async fn subscribe(
    ws: &mut Socket,
    stream: &str,
    endpoint: &str,
    args: Value,
) -> Result<()> {
    ws.send(Message::Text(
        json!({"type":"open","streamId":stream,"endpoint":endpoint,"payload":{"args":args}})
            .to_string()
            .into(),
    ))
    .await
    .map_err(|_| Error::Closed("DSH subscription failed".into()))
}
pub(super) async fn cancel(ws: &mut Socket, stream: &str) -> Result<()> {
    ws.send(Message::Text(
        json!({"type":"cancel","streamId":stream})
            .to_string()
            .into(),
    ))
    .await
    .map_err(|_| Error::Closed("DSH socket closed".into()))
}
pub(super) async fn read(ws: &mut Socket) -> Result<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
            Some(Ok(Message::Binary(b))) => return Ok(serde_json::from_slice(&b)?),
            Some(Ok(Message::Ping(b))) => ws
                .send(Message::Pong(b))
                .await
                .map_err(|_| Error::Closed("DSH socket closed".into()))?,
            Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
            _ => return Err(Error::Closed("DSH socket closed".into())),
        }
    }
}

/// Outer None means that the header has not fully arrived.
fn source_header(bytes: &[u8]) -> Option<Option<&str>> {
    let nl = bytes.iter().position(|b| *b == b'\n')?;
    if !bytes[..nl].starts_with(b"# ") {
        return Some(None);
    }
    let body = &bytes[nl + 1..];
    let label = b"\nSession file:\n\n";
    if body.len() < label.len() && label.starts_with(body) {
        return None;
    }
    let Some(fenced) = body.strip_prefix(label.as_slice()) else {
        return Some(None);
    };
    let nl = fenced.iter().position(|b| *b == b'\n')?;
    let opening = &fenced[..nl];
    let Some(ticks) = opening.strip_suffix(b"text") else {
        return Some(None);
    };
    if ticks.len() < 3 || ticks.iter().any(|b| *b != b'`') {
        return Some(None);
    }
    let rest = &fenced[nl + 1..];
    let end = rest
        .split_inclusive(|b| *b == b'\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line))
        })
        .find_map(|(start, line)| (line.strip_suffix(b"\n") == Some(ticks)).then_some(start))?;
    let path = std::str::from_utf8(rest[..end].strip_suffix(b"\n")?).ok()?;
    Some(std::path::Path::new(path).is_absolute().then_some(path))
}

#[cfg(test)]
mod tests {
    use super::source_header;

    #[test]
    fn exported_path_requires_the_complete_header_and_matching_fence() {
        let fence = "`".repeat(5);
        let path = std::env::temp_dir().join("line\nwith-````");
        let path = path.to_str().unwrap();
        let header = format!("# title\n\nSession file:\n\n{fence}text\n{path}\n{fence}\n");
        for end in 0..header.len() {
            assert_eq!(source_header(&header.as_bytes()[..end]), None);
        }
        assert_eq!(source_header(header.as_bytes()), Some(Some(path)));
        let from_message = format!("# title\n\n## User\n\n{header}");
        assert_eq!(source_header(from_message.as_bytes()), Some(None));
        assert_eq!(
            source_header(b"# title\n\nSession file:\n\n~~~\n"),
            Some(None)
        );
    }
}
