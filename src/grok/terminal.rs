//! The ACP terminal reverse requests run on the host, independently of Grok's stdin.
use crate::process::OwnedProcess;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Create {
    session_id: String,
    command: String,
    args: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: Vec<Variable>,
    output_byte_limit: Option<u64>,
}
#[derive(Deserialize)]
struct Variable {
    name: String,
    value: String,
}

struct Output {
    data: VecDeque<u8>,
    pending: [Vec<u8>; 2],
    limit: usize,
    truncated: bool,
}
impl Output {
    fn chunk(&mut self, stream: usize, bytes: &[u8]) {
        let pending = &mut self.pending[stream];
        pending.extend_from_slice(bytes);
        let end = if bytes.is_empty() {
            pending.len()
        } else {
            // Find the final unfinished UTF-8 scalar, even after an invalid byte.
            let start = pending.len().saturating_sub(4);
            (start..pending.len())
                .rev()
                .find(|&i| pending[i] & 0xc0 != 0x80)
                .filter(|&i| {
                    std::str::from_utf8(&pending[i..]).is_err_and(|e| e.error_len().is_none())
                })
                .unwrap_or(pending.len())
        };
        let text = String::from_utf8_lossy(&pending[..end]);
        self.data.extend(text.bytes());
        pending.drain(..end);
        if self.data.len() > self.limit {
            self.truncated = true;
            self.data.drain(..self.data.len() - self.limit);
            while self.data.front().is_some_and(|b| b & 0xc0 == 0x80) {
                self.data.pop_front();
            }
        }
    }
}
struct Stop {
    hard: bool,
    reply: oneshot::Sender<Result<bool, String>>,
}
#[derive(Clone)]
struct Handle {
    signal: mpsc::UnboundedSender<Stop>,
    exit: watch::Receiver<Option<Value>>,
    output: Arc<Mutex<Output>>,
}
impl Handle {
    async fn wait(&self) -> Result<Value, String> {
        let mut exit = self.exit.clone();
        let value = exit
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "terminal owner closed")?;
        Ok(value.clone().expect("waited for exit"))
    }
    async fn stop(&self, hard: bool) -> Result<bool, String> {
        if self.exit.borrow().is_some() {
            return Ok(false);
        }
        let (reply, result) = oneshot::channel();
        if self.signal.send(Stop { hard, reply }).is_err() {
            self.wait().await?;
            return Ok(false);
        }
        let sent = result.await.unwrap_or_else(|_| {
            if self.exit.borrow().is_some() {
                Ok(false)
            } else {
                Err("terminal owner closed".into())
            }
        })?;
        if hard {
            self.wait().await?;
        }
        Ok(sent)
    }
    fn output(&self) -> Value {
        let out = self.output.lock().unwrap();
        let data = out.data.iter().copied().collect::<Vec<_>>();
        let mut result = json!({"output":String::from_utf8(data).expect("decoded UTF-8"),"truncated":out.truncated});
        if let Some(exit) = self.exit.borrow().as_ref() {
            result["exitStatus"] = exit.clone();
        }
        result
    }
}

fn exit_status(status: Option<ExitStatus>) -> Value {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status
            .and_then(|s| s.signal())
            .and_then(|s| nix::sys::signal::Signal::try_from(s).ok())
            .map(|s| s.to_string())
    };
    #[cfg(not(unix))]
    let signal: Option<String> = None;
    json!({"exitCode":status.and_then(|s|s.code()),"signal":signal})
}

fn launch(
    create: &Create,
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    agent: Option<u32>,
) -> Result<Handle, Fault> {
    if create.command.is_empty() || create.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
        return Err(Fault::invalid(
            "terminal requires a command and an absolute working directory",
        ));
    }
    let mut cmd = if let Some(args) = &create.args {
        let mut cmd = Command::new(&create.command);
        cmd.args(args);
        cmd
    } else {
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new(crate::process::find_sh());
            c.arg("-c");
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd.exe");
            c.args(["/d", "/s", "/c"]);
            c
        };
        #[cfg(windows)]
        cmd.raw_arg(&create.command);
        #[cfg(not(windows))]
        cmd.arg(&create.command);
        cmd
    };
    cmd.current_dir(create.cwd.as_deref().unwrap_or(cwd))
        .env_clear()
        .envs(env);
    for v in &create.env {
        if v.name.is_empty() || v.name.contains(['=', '\0']) {
            return Err(Fault::invalid("invalid terminal environment variable"));
        }
        cmd.env(&v.name, &v.value);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedProcess::spawn(cmd).map_err(Fault::internal)?;
    #[cfg(target_os = "linux")]
    if let Some((agent, pid)) = agent.zip(child.id())
        && let Ok(score) = std::fs::read_to_string(format!("/proc/{agent}/oom_score_adj"))
    {
        let _ = std::fs::write(format!("/proc/{pid}/oom_score_adj"), score);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = agent;
    let mut stdout = child.inner().stdout.take().expect("piped stdout");
    let mut stderr = child.inner().stderr.take().expect("piped stderr");
    let output = Arc::new(Mutex::new(Output {
        data: VecDeque::new(),
        pending: Default::default(),
        limit: create.output_byte_limit.unwrap_or(4 << 20).min(16 << 20) as usize,
        truncated: false,
    }));
    let (signal, mut control) = mpsc::unbounded_channel::<Stop>();
    let (done, exit) = watch::channel(None);
    let handle = Handle {
        signal,
        exit,
        output: output.clone(),
    };
    tokio::spawn(async move {
        let mut out = [0; 8192];
        let mut err = [0; 8192];
        let mut out_open = true;
        let mut err_open = true;
        let mut deadline = None;
        let status = loop {
            tokio::select! {
                n = stdout.read(&mut out), if out_open => {
                    let n = n.unwrap_or(0); out_open = n > 0;
                    output.lock().unwrap().chunk(0, &out[..n]);
                }
                n = stderr.read(&mut err), if err_open => {
                    let n = n.unwrap_or(0); err_open = n > 0;
                    output.lock().unwrap().chunk(1, &err[..n]);
                }
                status = child.inner().wait(), if !out_open && !err_open => break status.ok(),
                _ = async { if let Some(at) = deadline { tokio::time::sleep_until(at).await; } else { std::future::pending::<()>().await; } } => {
                    // A detached grandchild may retain the pipe after the group is gone.
                    out_open = false; err_open = false; deadline = None;
                }
                command = control.recv() => {
                    let Some(stop) = command else {
                        child.stop().await;
                        break None;
                    };
                    let sent = child.signal_terminal(stop.hard).map_err(|e| e.to_string());
                    if stop.hard && sent.is_ok() {
                        deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                    }
                    let _ = stop.reply.send(sent);
                }
            }
        };
        let mut output = output.lock().unwrap();
        output.chunk(0, &[]);
        output.chunk(1, &[]);
        done.send_replace(Some(exit_status(status)));
    });
    Ok(handle)
}

struct Entry {
    session: String,
    turn: Option<String>,
    handle: Handle,
}
struct Fault {
    code: i32,
    message: String,
}
impl Fault {
    fn invalid(message: impl ToString) -> Self {
        Self {
            code: -32602,
            message: message.to_string(),
        }
    }
    fn internal(message: impl ToString) -> Self {
        Self {
            code: -32603,
            message: message.to_string(),
        }
    }
}
type Reply = Result<Value, Fault>;
pub(crate) struct Terminals {
    cwd: PathBuf,
    env: BTreeMap<OsString, OsString>,
    agent: Option<u32>,
    entries: HashMap<Uuid, Entry>,
    replies: JoinSet<Value>,
    aborts: HashMap<String, Vec<u64>>,
    ending: JoinSet<()>,
}
pub(crate) fn method(name: &str) -> bool {
    matches!(
        name,
        "terminal/create"
            | "terminal/output"
            | "terminal/kill"
            | "terminal/release"
            | "terminal/wait_for_exit"
    )
}
fn response(id: Value, reply: Reply) -> Value {
    match reply {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(fault) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":fault.code,"message":fault.message}})
        }
    }
}
impl Terminals {
    pub(crate) fn new(opts: &crate::SessionOptions, agent: Option<u32>) -> Self {
        Self {
            cwd: opts.cwd.canonicalize().unwrap_or_else(|_| opts.cwd.clone()),
            env: opts.installation_options().effective_env(),
            agent,
            entries: HashMap::new(),
            replies: JoinSet::new(),
            aborts: HashMap::new(),
            ending: JoinSet::new(),
        }
    }
    pub(crate) fn request(
        &mut self,
        method: &str,
        id: Value,
        params: &Value,
        turn: Option<String>,
    ) -> Option<Value> {
        let result = self.handle(method, &id, params, turn);
        match result {
            Ok(None) => None,
            Ok(Some(value)) => Some(response(id, Ok(value))),
            Err(e) => Some(response(id, Err(e))),
        }
    }
    fn handle(
        &mut self,
        method: &str,
        id: &Value,
        p: &Value,
        turn: Option<String>,
    ) -> Result<Option<Value>, Fault> {
        if method == "terminal/create" {
            let create: Create = serde_json::from_value(p.clone()).map_err(Fault::invalid)?;
            let handle = launch(&create, &self.cwd, &self.env, self.agent)?;
            let terminal = Uuid::new_v4();
            self.entries.insert(
                terminal,
                Entry {
                    session: create.session_id,
                    turn,
                    handle,
                },
            );
            return Ok(Some(json!({"terminalId":terminal})));
        }
        let terminal = p["terminalId"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
            .ok_or_else(|| Fault::invalid("invalid terminal ID"))?;
        let entry = self
            .entries
            .get(&terminal)
            .filter(|e| p["sessionId"] == e.session)
            .ok_or_else(|| Fault::invalid("unknown terminal for this session"))?;
        if method == "terminal/output" {
            return Ok(Some(entry.handle.output()));
        }
        let handle = entry.handle.clone();
        let release = method == "terminal/release";
        let wait = method == "terminal/wait_for_exit";
        if release {
            self.entries.remove(&terminal);
        }
        let id = id.clone();
        self.replies.spawn(async move {
            let result = if wait {
                handle.wait().await
            } else {
                handle.stop(release).await.map(|_| json!({}))
            };
            response(id, result.map_err(Fault::internal))
        });
        Ok(None)
    }
    pub(crate) fn pending(&self) -> bool {
        !self.replies.is_empty()
    }
    pub(crate) async fn reply(&mut self) -> Option<Value> {
        self.replies.join_next().await?.ok()
    }
    pub(crate) fn end(
        &self,
        turn: &str,
    ) -> impl Future<Output = crate::ProcessCleanupResult> + Send + 'static {
        let targets: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.turn.as_deref() == Some(turn))
            .map(|e| e.handle.clone())
            .collect();
        async move {
            let outcomes =
                futures_util::future::join_all(targets.iter().map(|h| h.stop(true))).await;
            let mut signalled = 0;
            let mut errors = Vec::new();
            for result in outcomes {
                match result {
                    Ok(true) => signalled += 1,
                    Ok(false) => {}
                    Err(e) => errors.push(e),
                }
            }
            if errors.is_empty() {
                crate::ProcessCleanupResult::Completed {
                    signalled,
                    foreign: 0,
                }
            } else {
                crate::ProcessCleanupResult::Failed {
                    reason: errors.join(", "),
                    signalled,
                    foreign: 0,
                }
            }
        }
    }
    pub(crate) fn aborted(&mut self, turn: &str, id: u64) {
        self.aborts.entry(turn.to_owned()).or_default().push(id);
    }
    pub(crate) fn ended(
        &mut self,
        turn: &str,
        outcome: &crate::Outcome,
        session: Option<String>,
        journal: crate::stream::Journal,
    ) {
        let Some(ids) = self.aborts.remove(turn) else {
            return;
        };
        let abort = *outcome == crate::Outcome::Aborted;
        let stop = self.end(turn);
        let turn = turn.to_owned();
        while self.ending.try_join_next().is_some() {}
        self.ending.spawn(async move {
            let result = if abort {
                stop.await
            } else {
                crate::ProcessCleanupResult::Completed {
                    signalled: 0,
                    foreign: 0,
                }
            };
            for id in ids {
                crate::TurnProcessCleanup {
                    turn_id: turn.clone(),
                    request_id: id,
                    result: result.clone(),
                }
                .publish(&journal, session.clone());
            }
        });
    }
    pub(crate) async fn close(
        &mut self,
        session: Option<String>,
        journal: &crate::stream::Journal,
    ) {
        for (turn, ids) in self.aborts.drain() {
            for id in ids {
                crate::TurnProcessCleanup {
                    turn_id: turn.clone(),
                    request_id: id,
                    result: crate::resources::turn::failed(
                        "Grok closed before its turn end was confirmed",
                    ),
                }
                .publish(journal, session.clone());
            }
        }
        while self.ending.join_next().await.is_some() {}
        let handles: Vec<_> = self
            .entries
            .drain()
            .map(|(_, entry)| entry.handle)
            .collect();
        futures_util::future::join_all(handles.iter().map(|h| h.stop(true))).await;
        while self.replies.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_scalar_boundaries_and_separate_stream_decoders() {
        let mut out = Output {
            data: Default::default(),
            pending: Default::default(),
            truncated: false,
            limit: 5,
        };
        let bytes = "猫".as_bytes();
        out.chunk(0, &bytes[..2]);
        out.chunk(1, b"err");
        out.chunk(0, &bytes[2..]);
        assert_eq!(
            out.data.iter().copied().collect::<Vec<_>>(),
            "rr猫".as_bytes()
        );
        out.chunk(1, b"xyz");
        assert_eq!(out.data.iter().copied().collect::<Vec<_>>(), b"xyz");
        out.chunk(0, &[0xf0]);
        out.chunk(0, &[]);
        assert_eq!(
            out.data.iter().copied().collect::<Vec<_>>(),
            "yz�".as_bytes()
        );
        assert!(out.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_ownership_wait_output_and_abort_receipts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut host = Terminals::new(&crate::SessionOptions::new(tmp.path()), None);
        let journal = crate::stream::Journal::new(None).unwrap();
        let request = |host: &mut Terminals, session: &str, turn: Option<&str>| {
            let p = json!({"sessionId":session, "command":"exec sleep 20"});
            host.request("terminal/create", 1.into(), &p, turn.map(str::to_owned))
                .unwrap()["result"]["terminalId"]
                .clone()
        };
        let older = request(&mut host, "root", Some("older"));
        let child = request(&mut host, "child", None);
        let current = request(&mut host, "root", Some("current"));
        let reference = json!({"sessionId":"root","terminalId":current});
        assert!(
            host.request("terminal/wait_for_exit", 9.into(), &reference, None)
                .is_none()
        );
        let wrong = json!({"sessionId":"child","terminalId":current});
        assert_eq!(
            host.request("terminal/kill", 10.into(), &wrong, None)
                .unwrap()["error"]["code"],
            -32602
        );
        host.aborted("current", 12);
        host.aborted("current", 13);
        host.ended(
            "current",
            &crate::Outcome::Aborted,
            Some("root".into()),
            journal.clone(),
        );
        let waited = tokio::time::timeout(Duration::from_secs(3), host.reply())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waited["id"], 9);
        assert_eq!(waited["result"]["signal"], "SIGKILL");
        let output = host
            .request("terminal/output", 10.into(), &reference, None)
            .unwrap();
        assert_eq!(output["result"]["exitStatus"], waited["result"]);
        host.aborted("older", 14);
        host.ended(
            "older",
            &crate::Outcome::Completed,
            Some("root".into()),
            journal.clone(),
        );
        for (session, id) in [("root", older), ("child", child)] {
            let query = json!({"sessionId":session,"terminalId":id});
            assert!(
                host.request("terminal/output", 11.into(), &query, None)
                    .unwrap()["result"]
                    .get("exitStatus")
                    .is_none()
            );
        }
        let missing =
            json!({"sessionId":"root","command":"/missing/kar-fixture-executable","args":[]});
        assert_eq!(
            host.request("terminal/create", 15.into(), &missing, None)
                .unwrap()["error"]["code"],
            -32603
        );
        let literal = json!({"sessionId":"root","command":"printf","args":["%s","$HOME 猫"],"outputByteLimit":4});
        let id = host
            .request("terminal/create", 16.into(), &literal, None)
            .unwrap()["result"]["terminalId"]
            .clone();
        let query = json!({"sessionId":"root","terminalId":id});
        host.request("terminal/wait_for_exit", 17.into(), &query, None);
        assert_eq!(host.reply().await.unwrap()["result"]["exitCode"], 0);
        let result = host
            .request("terminal/output", 18.into(), &query, None)
            .unwrap();
        assert_eq!(result["result"]["output"], " 猫");
        assert_eq!(result["result"]["truncated"], true);
        host.request("terminal/release", 19.into(), &query, None);
        assert_eq!(host.reply().await.unwrap()["result"], json!({}));
        assert_eq!(
            host.request("terminal/output", 20.into(), &query, None)
                .unwrap()["error"]["code"],
            -32602
        );
        host.aborted("uncertain", 21);
        host.close(Some("root".into()), &journal).await;
        let records = journal.records();
        for (id, count) in [(12, 1), (13, 1), (14, 0)] {
            let record = records
                .iter()
                .find(|r| r.native["requestId"] == id)
                .unwrap();
            assert_eq!(record.native["result"]["signalled"], count);
        }
        assert!(
            records
                .iter()
                .any(|r| r.native["requestId"] == 21 && r.native["result"]["kind"] == "failed")
        );
    }
}
