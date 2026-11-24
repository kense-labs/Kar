use super::*;
use crate::terminal::Terminal;
use base64::{Engine, engine::general_purpose::STANDARD};
use portable_pty::{CommandBuilder, PtySize};
use std::future::Future;

struct Job {
    project: Uuid,
    client: Option<String>,
    runtime: String,
    program: PathBuf,
    born: Instant,
    state: watch::Receiver<Value>,
    answer: mpsc::Sender<Answer>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Default)]
pub(in crate::workbench) struct Jobs(Mutex<HashMap<Uuid, Job>>);
impl Jobs {
    pub(super) fn start<F, Fut>(
        &self,
        p: Uuid,
        id: String,
        program: &Path,
        client: Option<String>,
        run: F,
    ) -> Reply
    where
        F: FnOnce(Interaction) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        let mut jobs = self.0.lock().unwrap();
        jobs.retain(|_, j| j.born.elapsed() < Duration::from_secs(1800));
        if jobs.len() >= 32
            && let Some(old) = jobs
                .iter()
                .filter(|(_, j)| j.task.is_finished())
                .min_by_key(|(_, j)| j.born)
                .map(|(k, _)| *k)
        {
            jobs.remove(&old);
        }
        if jobs.len() >= 32
            || jobs
                .values()
                .any(|j| j.program == program && !j.task.is_finished())
        {
            return Err(Error::Busy.into());
        }
        let flow = Uuid::new_v4();
        let (tx, state) = watch::channel(json!({"status":"starting"}));
        let (answer, rx) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            let io = Interaction {
                state: tx.clone(),
                rx,
            };
            let status = match tokio::time::timeout(Duration::from_secs(600), run(io)).await {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => json!({"status":"failed","reason":e.to_string()}),
                Err(_) => json!({"status":"failed","reason":"ACP authentication timed out"}),
            };
            tx.send_replace(status);
        });
        jobs.insert(
            flow,
            Job {
                project: p,
                client,
                runtime: id,
                program: program.into(),
                born: Instant::now(),
                state,
                answer,
                task,
            },
        );
        Ok(Json(json!({"flow":flow})))
    }
    pub(super) fn with_idle<T>(&self, program: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let jobs = self.0.lock().unwrap();
        if jobs
            .values()
            .any(|j| j.program == program && !j.task.is_finished())
        {
            return Err(Error::Busy);
        }
        f()
    }
}
type Key = (Uuid, String, Uuid);
/// The client generates an independent random secret for its window and never places it in a URL.
pub(super) fn client(headers: &HeaderMap) -> Result<Option<String>> {
    let Some(v) = headers.get("kar-auth-client") else {
        return Ok(None);
    };
    let v = v.to_str().map_err(|_| bad("invalid sign-in client key"))?;
    if !(32..=128).contains(&v.len())
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(bad("invalid sign-in client key"));
    }
    Ok(Some(crate::workbench::access::digest(v)))
}
fn job<'a>(jobs: &'a HashMap<Uuid, Job>, key: &Key, headers: &HeaderMap) -> Result<&'a Job> {
    let client = client(headers)?;
    jobs.get(&key.2)
        .filter(|j| {
            j.project == key.0
                && j.runtime == key.1
                && j.client.as_ref().is_none_or(|c| Some(c) == client.as_ref())
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown ACP sign-in flow").into())
}
pub(super) async fn status(
    State(app): State<Workbench>,
    Param(key): Param<Key>,
    headers: HeaderMap,
) -> Reply {
    let jobs = app.0.acp_auth.0.lock().unwrap();
    let current = job(&jobs, &key, &headers)?.state.borrow().clone();
    Ok(Json(current))
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Answer {
    Browser {
        interaction: Uuid,
        accept: bool,
    },
    Terminal {
        interaction: Uuid,
        data_base64: Option<String>,
        size: Option<[u16; 2]>,
    },
}
impl Answer {
    fn id(&self) -> Uuid {
        match self {
            Self::Browser { interaction, .. } | Self::Terminal { interaction, .. } => *interaction,
        }
    }
}
pub(super) async fn respond(
    State(app): State<Workbench>,
    Param(key): Param<Key>,
    headers: HeaderMap,
    Json(a): Json<Answer>,
) -> Reply {
    let jobs = app.0.acp_auth.0.lock().unwrap();
    let current = job(&jobs, &key, &headers)?;
    let state = current.state.borrow();
    let kind = match &a {
        Answer::Browser { .. } => "browser",
        Answer::Terminal {
            data_base64, size, ..
        } => {
            if data_base64
                .as_ref()
                .is_some_and(|s| s.len() > 8192 || STANDARD.decode(s).is_err())
                || size.is_some_and(|d| d.iter().any(|v| !(1..=1000).contains(v)))
            {
                return Err(bad("invalid terminal response").into());
            }
            "terminal"
        }
    };
    if state["status"] != kind || state["interaction"] != a.id().to_string() {
        return Err(bad("ACP interaction has changed").into());
    }
    current.answer.try_send(a).map_err(|_| Error::Busy)?;
    Ok(Json(json!({"accepted":true})))
}
pub(super) async fn cancel(
    State(app): State<Workbench>,
    Param(key): Param<Key>,
    headers: HeaderMap,
) -> Reply {
    let mut jobs = app.0.acp_auth.0.lock().unwrap();
    job(&jobs, &key, &headers)?;
    jobs.remove(&key.2);
    Ok(Json(json!({"removed":true})))
}

pub(super) fn web(v: &Value) -> Option<String> {
    let s = v.as_str().filter(|s| s.len() <= 4096)?;
    let url = url::Url::parse(s).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}
pub(super) struct Interaction {
    state: watch::Sender<Value>,
    rx: mpsc::Receiver<Answer>,
}
impl Interaction {
    pub(super) fn set(&self, state: Value) {
        self.state.send_replace(state);
    }
    pub(super) async fn rpc(
        &mut self,
        wire: &mut AccountWire,
        id: u64,
        method: &'static str,
        params: Value,
    ) -> Result<Value> {
        wire.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let mut f = wire.next().await?;
            if f["jsonrpc"] != "2.0" {
                continue;
            }
            if f["id"] == id && f.get("method").is_none() {
                return models::result(&mut f, method);
            }
            let Some(request) = f["method"].as_str().filter(|_| !f["id"].is_null()) else {
                continue;
            };
            let mut response = json!({"jsonrpc":"2.0","id":f["id"]});
            if matches!(request, "elicitation/create" | "session/elicitation") {
                let p = &f["params"];
                let mut accept = false;
                if p["mode"] == "url"
                    && p["elicitationId"]
                        .as_str()
                        .is_some_and(|v| !v.trim().is_empty() && v.len() <= 128)
                    && let Some(url) = web(&p["url"])
                {
                    let interaction = Uuid::new_v4();
                    self.set(json!({"status":"browser","interaction":interaction,"url":url,"message":p["message"].as_str().unwrap_or("").chars().take(1024).collect::<String>()}));
                    while let Some(a) = self.rx.recv().await {
                        if let Answer::Browser {
                            interaction: key,
                            accept: allowed,
                        } = a
                            && interaction == key
                        {
                            accept = allowed;
                            break;
                        }
                    }
                    self.set(json!({"status":"verifying"}));
                }
                let answer = json!({"action":if accept{"accept"}else{"decline"}});
                response["result"] = if request == "session/elicitation" {
                    json!({"action":answer})
                } else {
                    answer
                };
            } else {
                response["error"] = json!({"code":-32601,"message":"Unavailable during sign-in"});
            }
            wire.send(response).await?;
        }
    }
    pub(super) async fn terminal(
        &mut self,
        rt: &Runtime,
        opts: &SessionOptions,
        m: &Value,
    ) -> Result<()> {
        let mut cmd = CommandBuilder::new(&rt.program);
        cmd.args(&rt.args);
        cmd.cwd(&opts.cwd);
        for (name, value) in &opts.env {
            cmd.env(name, value);
        }
        if let Some(args) = m["args"].as_array() {
            if args.len() > 64 {
                return Err(bad("too many ACP sign-in arguments"));
            }
            for arg in args {
                cmd.arg(
                    arg.as_str()
                        .filter(|s| s.len() <= 4096 && !s.contains('\0'))
                        .ok_or_else(|| bad("invalid ACP sign-in argument"))?,
                );
            }
        }
        if let Some(env) = m["env"].as_object() {
            for (k, v) in env {
                if valid_env(k) {
                    cmd.env(
                        k,
                        v.as_str()
                            .filter(|s| s.len() <= 8192 && !s.contains('\0'))
                            .ok_or_else(|| bad("invalid ACP sign-in environment"))?,
                    );
                }
            }
        }
        for key in &opts.unset_env {
            cmd.env_remove(key);
        }
        cmd.env_remove("KAR_WORKBENCH_TOKEN");
        let mut terminal = Terminal::open(cmd, PtySize::default())?;
        let mut output = Vec::new();
        let mut offset = 0u64;
        let interaction = Uuid::new_v4();
        loop {
            self.set(json!({"status":"terminal","interaction":interaction,"output_base64":STANDARD.encode(&output),"end":offset}));
            if let Some(ok) = *terminal.exit.borrow() {
                return if ok {
                    Ok(())
                } else {
                    Err(bad("ACP sign-in command exited unsuccessfully"))
                };
            }
            tokio::select! {
                row=terminal.output.recv()=>if let Some(chunk)=row{
                    let bytes=chunk?;offset=offset.saturating_add(bytes.len() as u64);output.extend(bytes);
                    if output.len()>16384{output.drain(..output.len()-16384);}
                },
                ended=terminal.exit.changed()=>{ended.map_err(|_|bad("ACP sign-in terminal stopped"))?;},
                a=self.rx.recv()=>match a{
                    Some(Answer::Terminal{interaction:key,data_base64,size}) if key==interaction=>{
                        if let Some([cols,rows])=size{terminal.resize(PtySize{cols,rows,..Default::default()})?;}
                        if let Some(data)=data_base64{terminal.write_bytes(STANDARD.decode(data).map_err(|_|bad("invalid terminal input"))?)?;}
                    }
                    None=>return Err(bad("ACP sign-in was cancelled")),
                    _=>{}
                }
            }
        }
    }
}
