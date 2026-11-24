use kar::{
    Runtime, SessionOptions,
    agents::{Crew, Landed, SendMode},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, task::JoinSet};
use uuid::Uuid;

#[derive(Clone)]
struct Server {
    crew: Crew,
    cwd: PathBuf,
    runtimes: BTreeMap<String, Runtime>,
    limits: Limits,
}

#[derive(Clone, Copy)]
pub(super) struct Limits {
    sessions: usize,
    running: usize,
    depth: usize,
    max_depth: usize,
}
impl Limits {
    pub fn from_env(
        sessions: Option<usize>,
        running: usize,
        requested: Option<usize>,
    ) -> kar::Result<Self> {
        fn inherited(name: &str) -> kar::Result<Option<usize>> {
            std::env::var_os(name)
                .map(|s| {
                    s.to_str()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| kar::Error::Protocol(format!("invalid {name}")))
                })
                .transpose()
        }
        let inherited_limit = inherited("KAR_AGENT_MAX_DEPTH")?;
        let max_depth = match (requested, inherited_limit) {
            (Some(local), Some(parent)) => local.min(parent),
            (Some(local), None) => local,
            (None, Some(parent)) => parent,
            (None, None) => 1,
        };
        Ok(Self {
            sessions: sessions.unwrap_or(tokio::sync::Semaphore::MAX_PERMITS),
            running,
            depth: inherited("KAR_AGENT_DEPTH")?.unwrap_or(0),
            max_depth,
        })
    }
}
fn tools() -> Value {
    let string = json!({"type":"string"});
    let id = json!({"type":"string","format":"uuid"});
    let launch = json!({"runtime":{"type":"string","description":"Runtime ID returned by the runtimes tool."},"task":string,"name":string,"cwd":string,"model":string,"effort":string,"serviceTier":string,"resume":string});
    let specs = [
        (
            "runtimes",
            "List the runtimes offered by this server and their installation status.",
            json!({}),
            vec![],
        ),
        (
            "spawn",
            "Start a child agent and return its handle.",
            launch.clone(),
            vec!["runtime", "task"],
        ),
        (
            "run",
            "Start a child agent and wait for its first turn.",
            launch,
            vec!["runtime", "task"],
        ),
        ("list", "List agents and unread reports.", json!({}), vec![]),
        (
            "send",
            "Send a follow-up to a child agent.",
            json!({"id":id,"message":string,"mode":{"enum":["followup","queue","steer"]}}),
            vec!["id", "message"],
        ),
        (
            "wait",
            "Take unread reports from any selected agents.",
            json!({"ids":{"type":"array","items":id},"timeoutMs":{"type":"integer","minimum":0,"maximum":600000}}),
            vec![],
        ),
        (
            "interrupt",
            "Interrupt a child agent's current turn.",
            json!({"id":id}),
            vec!["id"],
        ),
        (
            "close",
            "Close a child agent and release its process.",
            json!({"id":id}),
            vec!["id"],
        ),
    ];
    json!({"tools":specs.map(|(name,description,properties,required)|json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}))})
}
fn argument<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .ok_or_else(|| format!("{key} must be a string"))
}
fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    args.get(key)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} must be a string"))
        })
        .transpose()
}

fn refused(code: &str, reason: impl Into<String>) -> Value {
    json!({"kind":"refused","code":code,"reason":reason.into()})
}

impl Server {
    async fn tool(&self, params: &Value) -> Result<Value, String> {
        let a = &params["arguments"];
        let id = || Uuid::parse_str(argument(a, "id")?).map_err(|e| e.to_string());
        match argument(params, "name")? {
            "runtimes" => {
                let mut rows = vec![];
                for r in self.runtimes.values() {
                    let installation = match r
                        .probe_installation(&kar::InstallationOptions {
                            cwd: Some(self.cwd.clone()),
                            ..Default::default()
                        })
                        .await
                    {
                        Ok(snapshot) => snapshot.summary(),
                        Err(error) => kar::Installation::Failed {
                            reason: error.to_string(),
                        },
                    };
                    let mut row = json!({"id":r.id, "installation":installation});
                    if let Some(reason) = r.protocol.child_environment_refusal() {
                        row["spawnable"] = false.into();
                        row["reason"] = reason.into();
                    }
                    rows.push(row);
                }
                Ok(json!({"runtimes":rows}))
            }
            "list" => Ok(
                json!({"agents":self.crew.list(),"unread":self.crew.unread().iter().map(|(id,turn)|json!({"id":id,"turn":turn})).collect::<Vec<_>>()}),
            ),
            name @ ("run" | "spawn") => {
                let runtime_id = argument(a, "runtime")?;
                let prompt = argument(a, "task")?.to_owned();
                if runtime_id.is_empty() || prompt.is_empty() {
                    return Err("runtime and task are required".into());
                }
                let label = optional_string(a, "name")?;
                let cwd = optional_string(a, "cwd")?
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.cwd.clone());
                let mut opts = SessionOptions::new(cwd);
                opts.model = optional_string(a, "model")?;
                opts.resume = optional_string(a, "resume")?;
                opts.settings.effort = optional_string(a, "effort")?;
                opts.settings.service_tier = optional_string(a, "serviceTier")?;
                let Some(runtime) = self.runtimes.get(runtime_id) else {
                    return Ok(refused(
                        "unknown_runtime",
                        format!("runtime {runtime_id} is not offered by this server"),
                    ));
                };
                if let Some(reason) = runtime.protocol.child_environment_refusal() {
                    return Ok(refused(
                        "open_failed",
                        format!("{runtime_id} cannot be a subagent: {reason}"),
                    ));
                }
                let limits = self.limits;
                if limits.depth >= limits.max_depth {
                    return Ok(refused(
                        "depth_limit",
                        format!("subagents may nest at most {} deep", limits.max_depth),
                    ));
                }
                if !opts.cwd.is_dir() {
                    return Ok(refused(
                        "open_failed",
                        format!("invalid working directory {}", opts.cwd.display()),
                    ));
                }
                opts.env.insert(
                    "KAR_AGENT_DEPTH".into(),
                    (limits.depth + 1).to_string().into(),
                );
                opts.env.insert(
                    "KAR_AGENT_MAX_DEPTH".into(),
                    limits.max_depth.to_string().into(),
                );
                opts.remove_env("KAR_WORKBENCH_TOKEN")
                    .map_err(|e| e.to_string())?;
                let result = if name == "spawn" {
                    self.crew
                        .spawn(runtime, opts, label, prompt)
                        .await
                        .map(|info| json!(info))
                } else {
                    self.crew
                        .run(runtime, opts, label, prompt)
                        .await
                        .map(|report| json!(report))
                };
                Ok(match result {
                    Ok(value) => value,
                    Err(error) => {
                        let code = match &error {
                            kar::Error::Refused(s) if s == "active agent limit reached" => {
                                "running_limit"
                            }
                            kar::Error::Refused(s) if s == "agent capacity reached" => {
                                "session_limit"
                            }
                            kar::Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                "not_installed"
                            }
                            _ => "open_failed",
                        };
                        refused(code, error.to_string())
                    }
                })
            }
            "send" => {
                let id = id()?;
                let msg = argument(a, "message")?.to_owned();
                let mode = a
                    .get("mode")
                    .map(|v| serde_json::from_value::<SendMode>(v.clone()))
                    .transpose()
                    .map_err(|_| "mode must be followup, steer or queue")?
                    .unwrap_or_default();
                match self.crew.send_mode(id, msg, mode).await {
                    Ok(receipt) => {
                        let mut value = json!({"id":id,"kind":"accepted","landed":receipt.landed});
                        if receipt.landed == Landed::Steered {
                            value["steered"] = true.into();
                        } else {
                            value["turn"] = receipt.turns_sent.into();
                        }
                        Ok(value)
                    }
                    Err(error) => {
                        let code = match &error {
                            kar::Error::Unsupported(_) => "unsupported",
                            kar::Error::Busy => "busy",
                            kar::Error::Closed(_) => "closed",
                            kar::Error::Refused(reason)
                                if reason == "active agent limit reached" =>
                            {
                                "running_limit"
                            }
                            kar::Error::Refused(reason) if reason == "no active turn" => {
                                "no_active_turn"
                            }
                            kar::Error::Refused(_) => "runtime_refused",
                            _ => "error",
                        };
                        Ok(
                            json!({"kind":"rejected","id":id,"code":code,"reason":error.to_string()}),
                        )
                    }
                }
            }
            "wait" => {
                let ids: Vec<Uuid> = a
                    .get("ids")
                    .map(|ids| serde_json::from_value(ids.clone()))
                    .transpose()
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default();
                let timeout = a["timeoutMs"].as_u64().unwrap_or(30000).min(600000);
                Ok(json!({"reports":self.crew.wait(&ids,Duration::from_millis(timeout)).await}))
            }
            "interrupt" => {
                self.crew
                    .interrupt(id()?)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({"interrupted":true}))
            }
            "close" => {
                self.crew.close(id()?).await.map_err(|e| e.to_string())?;
                Ok(json!({"closed":true}))
            }
            _ => Err("unknown tool".into()),
        }
    }
}
fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
async fn output(out: &mut tokio::io::Stdout, v: Value) -> kar::Result<()> {
    let mut bytes = serde_json::to_vec(&v)?;
    bytes.push(b'\n');
    out.write_all(&bytes).await?;
    out.flush().await?;
    Ok(())
}
pub(super) async fn serve(
    cwd: PathBuf,
    runtimes: Vec<Runtime>,
    limits: Limits,
    log_dir: Option<PathBuf>,
) -> kar::Result<()> {
    let mut crew = Crew::new(limits.sessions).with_running_limit(limits.running);
    if let Some(dir) = log_dir {
        crew = crew.with_log_directory(&dir)?;
    }
    let server = Server {
        limits,
        crew,
        cwd: std::fs::canonicalize(cwd)?,
        runtimes: runtimes.into_iter().map(|r| (r.id.clone(), r)).collect(),
    };
    let mut input = stdin_lines();
    let mut out = tokio::io::stdout();
    let mut initialized = false;
    let mut calls = JoinSet::new();
    let mut pending = HashMap::<String, tokio::task::AbortHandle>::new();
    let result:kar::Result<()>=async {
        loop {
            let line=tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                Some(result) = calls.join_next(), if !calls.is_empty() => {
                    if let Ok((id,reply))=result{
                        pending.remove(&id);
                        output(&mut out,reply).await?;
                    }
                    continue;
                }
                Some(line) = input.recv() => line?,
                else => break,
            };
            let Some(line)=line else{break};
            let request:Value=match serde_json::from_str(&line){
                Ok(value)=>value,
                Err(_)=>{output(&mut out,error(&Value::Null,-32700,"Invalid JSON")).await?;continue;}
            };
            let id=&request["id"];
            let method=request["method"].as_str().unwrap_or("");
            if method=="notifications/cancelled"{
                if let Some(handle)=pending.remove(&request["params"]["requestId"].to_string()){handle.abort();}
                continue;
            }
            if id.is_null(){continue;}
            let reply=match method{
                "initialize"=>{
                    initialized=true;
                    let versions=["2024-11-05","2025-03-26","2025-06-18","2025-11-25"];
                    let version=request.pointer("/params/protocolVersion").and_then(Value::as_str).filter(|v|versions.contains(v)).unwrap_or("2025-11-25");
                    ok(id,json!({"protocolVersion":version,"capabilities":{"tools":{}},"instructions":"Call runtimes for the offered runtime IDs and installation status. Pass the selected ID to run or spawn.","serverInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")}}))
                }
                "ping"=>ok(id,json!({})),
                _ if !initialized=>error(id,-32002,"Initialize the server first"),
                "tools/list"=>ok(id,tools()),
                "tools/call"=>{
                    let key=id.to_string();
                    if pending.contains_key(&key){output(&mut out,error(id,-32600,"Duplicate request ID")).await?;continue;}
                    let server=server.clone();
                    let handle=calls.spawn(async move{
                        let id=&request["id"];
                        let result=match server.tool(&request["params"]).await{
                            Ok(value) if matches!(value["kind"].as_str(), Some("refused" | "rejected")) =>
                                json!({"isError":true,"structuredContent":value,"content":[{"type":"text","text":value["reason"]}]}),
                            Ok(value)=>json!({"structuredContent":value,"content":[{"type":"text","text":value.to_string()}]}),
                            Err(message)=>json!({"isError":true,"content":[{"type":"text","text":message}]}),
                        };
                        (id.to_string(),ok(id,result))
                    });
                    pending.insert(key,handle);
                    continue;
                }
                _=>error(id,-32601,"Unknown method"),
            };
            output(&mut out,reply).await?;
        }
        Ok(())
    }.await;
    calls.abort_all();
    while calls.join_next().await.is_some() {}
    let mut result = result;
    for agent in server.crew.list() {
        if let Err(error) = server.crew.close(agent.id).await
            && result.is_ok()
        {
            result = Err(error);
        }
    }
    result
}

// A blocking read on stdin must not keep Tokio's blocking pool alive on Ctrl-C.
fn stdin_lines() -> tokio::sync::mpsc::Receiver<std::io::Result<Option<String>>> {
    use std::io::BufRead;
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            if tx.blocking_send(line.map(Some)).is_err() {
                return;
            }
        }
        let _ = tx.blocking_send(Ok(None));
    });
    rx
}
