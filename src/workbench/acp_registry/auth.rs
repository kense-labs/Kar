//! Owner-only, process-scoped ACP authentication. Interactive output stays in memory.
mod flow;
use super::*;
use crate::account::AccountWire;
pub(in crate::workbench) use flow::Jobs;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/acp-registry/{runtime}/auth/methods",
            get(methods),
        )
        .route(
            "/projects/{project}/acp-registry/{runtime}/auth/login",
            post(login),
        )
        .route(
            "/projects/{project}/acp-registry/{runtime}/auth/logout",
            post(logout),
        )
        .route(
            "/projects/{project}/acp-registry/{runtime}/auth/credentials",
            delete(forget),
        )
        .route(
            "/projects/{project}/acp-registry/{runtime}/auth/flows/{flow}",
            get(flow::status).post(flow::respond).delete(flow::cancel),
        )
}
type Creds = BTreeMap<String, String>;
fn credentials(store: &ChatStore, runtime: &str) -> PathBuf {
    root(store)
        .join("credentials")
        .join(format!("{:x}", Sha256::digest(runtime.as_bytes())))
}
pub(super) fn bind(store: &ChatStore, runtime: &str, opts: &mut SessionOptions) -> Result<()> {
    let file = match File::open(credentials(store, runtime)) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let vars: Creds = serde_json::from_reader(file)?;
    apply(opts, &vars)
}
fn valid_env(key: &str) -> bool {
    key.len() <= 128
        && key
            .bytes()
            .next()
            .is_some_and(|v| v.is_ascii_alphabetic() || v == b'_')
        && key.bytes().all(|v| v == b'_' || v.is_ascii_alphanumeric())
        && !key.to_ascii_uppercase().starts_with("KAR_")
}
fn apply(opts: &mut SessionOptions, vars: &Creds) -> Result<()> {
    if vars.len() > 16
        || vars.iter().any(|(k, v)| {
            !valid_env(k)
                || v.len() > 8192
                || v.contains('\0')
                || opts.unset_env.iter().any(|n| n.eq_ignore_ascii_case(k))
        })
    {
        return Err(bad("invalid ACP credential environment"));
    }
    for (k, v) in vars {
        opts.env.insert(k.into(), v.into());
    }
    Ok(())
}
fn context(app: &Workbench, project: Uuid, id: &str) -> Result<(Runtime, SessionOptions)> {
    if !dynamic(id) {
        return Err(bad("select a prepared ACP instance"));
    }
    let p = app.project(project)?;
    app.workspace(&p)?;
    let mut opts = SessionOptions::new(p.root);
    opts.remove_env("KAR_WORKBENCH_TOKEN")?;
    let rt = app.0.chats.resolve_runtime(id, &mut opts)?;
    Ok((rt, opts))
}
async fn initialize(rt: &Runtime, opts: &SessionOptions) -> Result<(AccountWire, Value)> {
    let mut cmd = opts.command(&rt.program)?;
    cmd.args(&rt.args);
    let mut wire = AccountWire::open(cmd)?;
    let mut caps = AccountWire::capabilities();
    caps["auth"] = json!({"terminal":true});
    caps["elicitation"] = json!({"url":{}});
    let hello = wire.initialize(caps).await?;
    Ok((wire, hello))
}
fn native_methods(hello: &Value) -> impl Iterator<Item = &Value> {
    hello["authMethods"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m["id"]
                .as_str()
                .is_some_and(|id| !id.is_empty() && id.trim() == id && id.len() <= 128)
                && matches!(
                    m["type"].as_str(),
                    None | Some("agent" | "terminal" | "env_var")
                )
        })
        .take(32)
}
pub(crate) fn headless<'a>(hello: &'a Value, preferred: Option<&str>) -> Result<Option<&'a str>> {
    if let Some(id) = preferred {
        let m = native_methods(hello)
            .find(|m| m["id"] == id)
            .ok_or_else(|| bad("configured ACP authentication method is no longer advertised"))?;
        if !matches!(m["type"].as_str(), None | Some("agent")) {
            return Err(bad(
                "complete this ACP method in the owner sign-in interface first",
            ));
        }
        Ok(m["id"].as_str())
    } else {
        Ok(native_methods(hello)
            .find(|m| matches!(m["type"].as_str(), None | Some("agent")))
            .and_then(|m| m["id"].as_str()))
    }
}
fn names(method: &Value) -> Vec<&str> {
    method["vars"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["name"].as_str())
        .filter(|k| valid_env(k))
        .take(16)
        .collect()
}
fn summary(m: &Value) -> Value {
    let kind = m["type"].as_str().unwrap_or("agent");
    let short = |k: &str, n| {
        m[k].as_str()
            .unwrap_or("")
            .chars()
            .take(n)
            .collect::<String>()
    };
    json!({"id":m["id"],"name":short("name",160),"description":short("description",1024),"type":if kind=="env_var"{"credentials"}else{kind},"environment":names(m),"link":flow::web(&m["link"])})
}
async fn methods(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, String)>) -> Reply {
    let (rt, opts) = context(&app, p, &id)?;
    let _pin = pin_process(&rt)?;
    let (_, hello) = tokio::time::timeout(opts.handshake_timeout, initialize(&rt, &opts))
        .await
        .map_err(|_| Error::Timeout("ACP auth methods".into()))??;
    Ok(Json(
        json!({"methods":native_methods(&hello).map(summary).collect::<Vec<_>>(),"default_method_id":opts.acp_auth_method,"logout":hello.pointer("/agentCapabilities/auth/logout").is_some_and(|v|v.is_object()||v==true)}),
    ))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    method_id: Option<String>,
    #[serde(default)]
    credentials: Creds,
}
async fn login(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    Json(input): Json<Login>,
) -> Reply {
    start(&app, p, id, Some(input))
}
async fn logout(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, String)>) -> Reply {
    start(&app, p, id, None)
}
fn start(app: &Workbench, p: Uuid, id: String, login: Option<Login>) -> Reply {
    let (rt, opts) = context(app, p, &id)?;
    let path = credentials(&app.0.chats, &id);
    let program = rt.program.clone();
    app.0.acp_auth.start(p, id, &program, move |io| async move {
        authenticate(rt, opts, path, login, io).await
    })
}
async fn forget(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, String)>) -> Reply {
    let (rt, _) = context(&app, p, &id)?;
    let removed = app.0.acp_auth.with_idle(&rt.program, || {
        match fs::remove_file(credentials(&app.0.chats, &id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    })?;
    Ok(Json(json!({"removed":removed})))
}
async fn authenticate(
    rt: Runtime,
    mut opts: SessionOptions,
    path: PathBuf,
    login: Option<Login>,
    mut io: flow::Interaction,
) -> Result<()> {
    let _pin = pin_process(&rt)?;
    let (mut wire, hello) = initialize(&rt, &opts).await?;
    let Some(input) = login else {
        if !hello
            .pointer("/agentCapabilities/auth/logout")
            .is_some_and(|v| v.is_object() || v == true)
        {
            return Err(Error::Unsupported(
                "ACP agent does not advertise logout".into(),
            ));
        }
        io.rpc(&mut wire, 2, "logout", json!({})).await?;
        wire.finish().await;
        return Ok(());
    };
    let wanted = input
        .method_id
        .as_deref()
        .or(opts.acp_auth_method.as_deref());
    let method = native_methods(&hello)
        .find(|m| wanted.map_or(true, |id| m["id"] == id))
        .ok_or_else(|| bad("sign-in method is not advertised"))?;
    let credential_method = method["type"] == "env_var";
    if !input.credentials.is_empty() {
        let allowed = names(method);
        if !credential_method
            || input
                .credentials
                .keys()
                .any(|k| !allowed.contains(&k.as_str()))
        {
            return Err(bad(
                "credentials must match the agent's advertised environment fields",
            ));
        }
        apply(&mut opts, &input.credentials)?;
        wire.close().await;
        (wire, _) = initialize(&rt, &opts).await?;
    }
    match method["type"].as_str() {
        Some("terminal") => {
            wire.close().await;
            io.terminal(&rt, &opts, method).await?;
            (wire, _) = initialize(&rt, &opts).await?;
        }
        Some("env_var") => {}
        _ => {
            io.rpc(
                &mut wire,
                2,
                "authenticate",
                json!({"methodId":method["id"]}),
            )
            .await?;
        }
    }
    io.set(json!({"status":"verifying"}));
    let opened = io
        .rpc(
            &mut wire,
            3,
            "session/new",
            json!({"cwd":opts.cwd,"mcpServers":[]}),
        )
        .await?;
    let session = opened["sessionId"]
        .as_str()
        .filter(|i| !i.is_empty())
        .ok_or_else(|| bad("ACP sign-in verification returned no session ID"))?;
    let caps = &hello["agentCapabilities"]["sessionCapabilities"];
    if let Some(op) = ["delete", "close"]
        .into_iter()
        .find(|k| caps[k].is_object() || caps[k] == true)
    {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            wire.rpc(4, &format!("session/{op}"), json!({"sessionId":session})),
        )
        .await;
    }
    wire.finish().await;
    if !input.credentials.is_empty() {
        let mut saved: Creds = match File::open(&path) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Creds::new(),
            Err(e) => return Err(e.into()),
        };
        saved.extend(input.credentials);
        record(&path, &saved)?;
    }
    Ok(())
}
