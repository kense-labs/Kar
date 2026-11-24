use super::{AuthStatus, Reader, profile, projection};
use crate::process::{OUTPUT_LIMIT, OwnedProcess};
use crate::{Error, Result, installation::Probe};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
};

pub(super) const BUDDY_ARGS: &[&str] = &[
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
];
pub(super) const FACTORY_ARGS: &[&str] = &["exec", "--output-format", "acp"];

pub(super) fn command(reader: Reader, probe: &Probe, program: &Path) -> Command {
    let mut cmd = probe.command(program);
    if reader == Reader::Claude {
        crate::options::claude_environment(&mut cmd, &Default::default());
    }
    cmd
}

pub(super) async fn query(reader: Reader, probe: &Probe, program: &Path) -> Result<AuthStatus> {
    use Reader::*;
    let keys: &[&str] = match reader {
        Devin => &["WINDSURF_API_KEY"],
        Codebuddy => &["CODEBUDDY_API_KEY"],
        Cursor => &["CURSOR_AUTH_TOKEN", "CURSOR_API_KEY"],
        _ => &[],
    };
    let configured: Vec<_> = keys
        .iter()
        .filter(|k| probe.get(k).is_some_and(|s| !s.is_empty()))
        .map(|key| format!("environment: {key}"))
        .collect();
    if !configured.is_empty() {
        return Ok(reader.configured(
            "Native account status cannot validate these environment credentials",
            configured,
        ));
    }
    if reader == Antigravity && !profile::default_gemini_directory(probe) {
        return Ok(reader.unknown("agy models cannot select the requested GEMINI_HOME"));
    }
    let args: &[&str] = match reader {
        Codex => &["login", "status"],
        Cursor => &["status", "--format", "json"],
        Claude => &["auth", "status", "--json"],
        Devin => &["auth", "status"],
        Qoder => &["status", "--output", "json"],
        Antigravity | Grok => &["models"],
        Kimi => &["acp"],
        Factory => FACTORY_ARGS,
        Codebuddy => BUDDY_ARGS,
        Qwen => unreachable!(),
    };
    let frames = matches!(reader, Kimi | Factory | Codebuddy);
    let mut cmd = command(reader, probe, program);
    cmd.args(args);
    if frames {
        let mut wire = Wire::open(cmd)?;
        let status = wire.status(reader).await;
        wire.close().await;
        return status;
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let result = OwnedProcess::spawn(cmd)?.output().await?;
    let exit = result.status.code();
    let out = String::from_utf8_lossy(&result.stdout);
    let err = String::from_utf8_lossy(&result.stderr);
    let mut status = projection::text(reader, exit, &out, &err);
    profile::enrich(reader, probe, &out, &mut status).await;
    Ok(status)
}

pub(crate) struct Wire {
    child: OwnedProcess,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}
impl Wire {
    pub(crate) fn open(mut cmd: Command) -> Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = OwnedProcess::spawn(cmd)?;
        Ok(Self {
            stdin: child.inner().stdin.take().expect("piped stdin"),
            stdout: BufReader::new(child.inner().stdout.take().expect("piped stdout")),
            child,
        })
    }
    /// Let a native inventory process finish writing its profile on stdin EOF.
    pub(crate) async fn finish(self) {
        let Self {
            mut child,
            stdin,
            mut stdout,
        } = self;
        drop(stdin);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(2500), async {
            let mut sink = tokio::io::sink();
            tokio::join!(child.wait(), tokio::io::copy(&mut stdout, &mut sink))
        })
        .await;
    }
    pub(crate) async fn close(&mut self) {
        self.child.stop().await;
    }

    pub(crate) async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }
    pub(crate) async fn next(&mut self) -> Result<Value> {
        let mut buf = vec![];
        loop {
            buf.clear();
            let n = (&mut self.stdout)
                .take(OUTPUT_LIMIT + 1)
                .read_until(b'\n', &mut buf)
                .await?;
            if n == 0 || n as u64 > OUTPUT_LIMIT {
                return Err(Error::Protocol("Incomplete private account frame".into()));
            }
            if let Ok(frame) = serde_json::from_slice::<Value>(&buf)
                && frame.is_object()
            {
                return Ok(frame);
            }
        }
    }
    pub(crate) async fn pi(&mut self, command: &'static str) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        self.send(json!({"type":command,"id":id})).await?;
        loop {
            let mut frame = self.next().await?;
            if frame["type"] == "extension_ui_request"
                && let Some(request) = frame["id"].as_str()
            {
                self.send(json!({"type":"extension_ui_response","id":request,"cancelled":true}))
                    .await?;
            } else if frame["type"] == "response" && frame["id"] == id {
                if frame["command"] == command
                    && frame["success"] == true
                    && frame["data"].is_object()
                {
                    return Ok(frame["data"].take());
                }
                return Err(Error::Protocol(
                    "Pi did not return the requested catalog".into(),
                ));
            }
        }
    }
    pub(crate) async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.request(id, method, params, true).await
    }
    pub(crate) async fn initialize_codex(&mut self) -> Result<()> {
        let hello=self.codex(1,"initialize",json!({"clientInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
        if !hello["result"].is_object() {
            return Err(Error::Protocol(
                "Codex app-server initialization failed".into(),
            ));
        }
        self.send(json!({"method":"initialized","params":{}})).await
    }
    pub(crate) async fn codex(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.request(id, method, params, false).await
    }
    async fn request(&mut self, id: u64, method: &str, params: Value, acp: bool) -> Result<Value> {
        let mut packet = json!({"id":id,"method":method,"params":params});
        if acp {
            packet["jsonrpc"] = "2.0".into();
        }
        self.send(packet).await?;
        loop {
            let f = self.next().await?;
            if acp && f["jsonrpc"] != "2.0" {
                continue;
            }
            if f.get("method").is_some() {
                if f["method"].is_string() && !f["id"].is_null() {
                    self.send(json!({"jsonrpc":"2.0","id":f["id"],"error":{"code":-32601,"message":"Not available during a native query"}})).await?;
                }
            } else if f["id"].as_u64() == Some(id) {
                return Ok(f);
            }
        }
    }
    pub(super) async fn control(&mut self, request: Value) -> Result<Value> {
        let response = self.control_reply(request).await?;
        match response["subtype"].as_str() {
            Some("success") => Ok(response["response"].clone()),
            Some("error") => Err(Error::Refused(
                "CodeBuddy refused the account operation".into(),
            )),
            _ => Err(Error::Protocol("Invalid CodeBuddy control response".into())),
        }
    }
    pub(crate) async fn control_reply(&mut self, request: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        self.send(json!({"type":"control_request","request_id":id,"request":request}))
            .await?;
        loop {
            let f = self.next().await?;
            let response = &f["response"];
            if f["type"] == "control_response" && response["request_id"] == id {
                return Ok(response.clone());
            }
        }
    }
    pub(crate) async fn initialize(&mut self, capabilities: Value) -> Result<Value> {
        let mut f = self.rpc(1,"initialize",json!({"protocolVersion":1,"clientCapabilities":capabilities,"clientInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")}})).await?;
        if f["error"].is_null() && f["result"]["protocolVersion"] == 1 {
            Ok(f["result"].take())
        } else {
            Err(Error::Protocol("ACP initialization failed".into()))
        }
    }
    pub(crate) fn capabilities() -> Value {
        json!({"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false})
    }
    async fn status(&mut self, reader: Reader) -> Result<AuthStatus> {
        if reader == Reader::Codebuddy {
            return Ok(projection::codebuddy(
                &self.control(json!({"subtype":"initialize"})).await?,
            ));
        }
        let init = self.initialize(Self::capabilities()).await?;
        if reader == Reader::Kimi {
            let offered = init["authMethods"]
                .as_array()
                .is_some_and(|a| a.iter().any(|m| m["id"] == "login"));
            if !offered {
                return Ok(reader.unknown("The runtime did not offer the login check"));
            }
            let response = self
                .rpc(2, "authenticate", json!({"methodId":"login"}))
                .await?;
            return Ok(if response["error"]["code"] == -32000 {
                reader.logged_out()
            } else if response["error"].is_null() && response.get("result").is_some() {
                reader.logged_in(Default::default())
            } else {
                reader.unknown("The native authentication check failed")
            });
        }
        if init.pointer("/agentCapabilities/auth/status").is_none() {
            return Ok(reader.unknown("This Droid build does not declare auth/status"));
        }
        let response = self.rpc(2, "auth/status", json!({})).await?;
        Ok(if response["error"].is_null() {
            projection::factory(&response["result"])
        } else {
            reader.unknown("Droid refused auth/status")
        })
    }
}
