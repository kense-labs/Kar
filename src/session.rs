mod image_input;
mod prompt_run;
mod qwen;
pub use prompt_run::{PromptInterrupt, PromptRun, PromptRunOptions};
pub(crate) mod reachability;

use crate::options::mcp_capabilities::McpSupport;
use crate::{
    Cursor, Error, Outcome, Protocol, Record, Result, Runtime, SessionOptions, Subscription,
    model_request::RequestReader,
    stream::Journal,
    transport::{Actor, Command, Identity},
};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnReport {
    /// Last native admission receipt observed for this input. A handled input needs no model turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_input_receipt: Option<crate::InputReceipt>,
    pub outcome: Outcome,
    pub text: String,
    #[serde(
        default,
        deserialize_with = "crate::stream::present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::TurnFailure>,
}

/// A dispatched input. Dropping this handle stops waiting, not the agent.
pub struct Turn {
    input_id: Uuid,
    admission: Option<crate::ControlReceipt>,
    receiver: oneshot::Receiver<Result<TurnReport>>,
}
impl Turn {
    /// UUID supplied by the caller or allocated when the prompt was submitted.
    pub fn input_id(&self) -> Uuid {
        self.input_id
    }

    /// The public request and decision for this prompt or queue admission.
    pub fn admission(&self) -> Option<&crate::ControlReceipt> {
        self.admission.as_ref()
    }

    pub async fn wait(self) -> Result<Outcome> {
        Ok(self.report().await?.outcome)
    }
    pub async fn report(self) -> Result<TurnReport> {
        self.receiver
            .await
            .map_err(|_| Error::Closed("turn owner stopped".into()))?
    }
}

/// A completed public prompt decision. Unconfirmed transport failures stay errors.
pub enum PromptDecision {
    Accepted(Turn),
    Rejected {
        receipt: crate::ControlReceipt,
        error: Error,
    },
}

pub enum Delivery {
    Started(Turn),
    Steered,
    Queued { input_id: Uuid, turn: Turn },
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Capabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_memory: Option<crate::NativeMemory>,
    pub protocol: Protocol,
    pub image_input: bool,
    pub resume: bool,
    pub fork: bool,
    pub steering: bool,
    /// The adapter accepts session MCP configuration, subject to native installation requirements.
    pub session_mcp: bool,
    /// Known native transports. None means unverified, an empty slice means unsupported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_transports: Option<&'static [crate::McpTransport]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_startup: Option<crate::McpStartup>,
    pub native_session_file: bool,
    pub model_requests: bool,
    pub host_queue: bool,
    /// Inputs accepted by the native runtime survive a process restart.
    pub durable_native_queue: bool,
    pub output_schema: bool,
    pub stop_task: bool,
    pub goals: bool,
    pub manual_compaction: bool,
}

#[derive(Clone)]
pub struct Session {
    mcp: McpSupport,
    native_memory: Option<crate::NativeMemory>,
    cwd: std::path::PathBuf,
    commands: mpsc::Sender<Command>,
    journal: Journal,
    preflight_secrets: Arc<Vec<String>>,
    id: String,
    protocol: Protocol,
    info: Value,
    pub(crate) image_input: bool,
    native_resume: bool,
    cursor_parameterized: bool,
    acp_steering: bool,
    text_limit: Option<usize>,
    request_capture: Option<RequestReader>,
    pub(crate) process_root: Option<crate::resources::Root>,
    factory_dir: Option<std::path::PathBuf>,
    pub(crate) dsh_file: Option<Arc<Mutex<Option<String>>>>,
    supports_output_schema: bool,
    native_fork: bool,
    manual_compaction: bool,
    goals: bool,
    timeout: Duration,
}
impl Session {
    pub(crate) fn remote(
        protocol: Protocol,
        options: SessionOptions,
        commands: mpsc::Sender<Command>,
        journal: Journal,
        id: String,
        info: Value,
    ) -> Self {
        let preflight_secrets = Arc::new(crate::options::session_secrets(&options));
        Self {
            mcp: McpSupport::new(protocol, "", None),
            native_memory: None,
            cwd: options.cwd,
            commands,
            journal,
            preflight_secrets,
            id,
            info,
            protocol,
            image_input: matches!(protocol, Protocol::Cybion | Protocol::KimiKap),
            request_capture: None,
            process_root: None,
            factory_dir: None,
            dsh_file: None,
            text_limit: None,
            native_resume: true,
            cursor_parameterized: false,
            acp_steering: false,
            supports_output_schema: false,
            native_fork: protocol == Protocol::Dsh,
            manual_compaction: matches!(protocol, Protocol::KimiKap | Protocol::Dsh),
            goals: protocol == Protocol::Dsh && options.settings.goals,
            timeout: options.handshake_timeout,
        }
    }
    pub(crate) async fn open(
        runtime: &Runtime,
        options: SessionOptions,
        fork: Option<(&str, Option<&str>)>,
        child: Option<Uuid>,
    ) -> Result<Self> {
        Self::open_with_replay(runtime, options, fork, child, false, None).await
    }
    pub(crate) async fn open_with_replay(
        runtime: &Runtime,
        options: SessionOptions,
        fork: Option<(&str, Option<&str>)>,
        child: Option<Uuid>,
        replay: bool,
        version: Option<&str>,
    ) -> Result<Self> {
        let journal = runtime.session_journal(options.retained_bytes)?;
        crate::options::launch_args::validate(runtime, &options, fork.is_some())?;
        options.settings.validate(runtime.protocol, &runtime.id)?;
        if let Some(reason) =
            crate::options::availability::credential_refusal(runtime.protocol, &runtime.id)
            && options
                .mcp_servers
                .values()
                .any(crate::McpServer::has_credentials)
        {
            return Err(Error::Unsupported(reason.into()));
        }
        let cwd = std::fs::canonicalize(&options.cwd)?;
        if runtime.protocol == Protocol::Acp
            && runtime.id == "kimi"
            && (options.resume.is_some() || fork.is_some())
            && !options.settings.additional_directories.is_empty()
        {
            return Err(Error::Unsupported(
                "Kimi accepts additional directories only at creation".into(),
            ));
        }
        let dirs = options.settings.directories(&cwd)?;
        if !cwd.is_dir() {
            return Err(Error::Protocol("cwd must be a directory".into()));
        }
        let cwd_str = cwd
            .to_str()
            .ok_or_else(|| Error::Unsupported("wire protocol requires a UTF-8 cwd".into()))?;
        let pi_mcp_entries = if runtime.protocol == Protocol::Pi && !options.mcp_servers.is_empty()
        {
            Some(crate::pi_mcp::entries(&options)?)
        } else {
            None
        };
        let pi_sdk = if runtime.protocol == Protocol::Pi
            && (options.settings.model_requests.is_some() || pi_mcp_entries.is_some())
        {
            Some(crate::pi_sdk::Launch::resolve(runtime, &options, version).await?)
        } else {
            None
        };
        let pi = if let Some(sdk) = &pi_sdk
            && options.settings.model_requests.is_some()
        {
            Some(crate::model_request::pi::Owner::new(sdk, &options).await?)
        } else {
            None
        };
        let mut process = pi_sdk.as_ref().map_or_else(
            || options.command(&runtime.program),
            |s| s.command(&options),
        )?;
        let mut pi_mcp = None;
        let grok_stdio = runtime.protocol == Protocol::Acp
            && runtime.id == "grok"
            && runtime.args.last().is_some_and(|a| a == "stdio");
        if grok_stdio {
            process
                .args(&runtime.args[..runtime.args.len() - 1])
                .args(&options.launch_args)
                .arg("stdio");
        } else if runtime.protocol != Protocol::Qoder {
            process.args(&runtime.args);
        }
        if runtime.protocol == Protocol::Acp
            && runtime.id == "antigravity"
            && let Some(directory) = crate::options::env_path(&options, "GEMINI_HOME")
        {
            process.arg("--gemini-dir").arg(directory);
        }
        process
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if runtime.protocol == Protocol::Qwen {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut id = options
            .resume
            .clone()
            .unwrap_or_else(|| child.unwrap_or_else(Uuid::new_v4).to_string());
        for name in options.mcp_servers.keys() {
            if name.is_empty() || crate::options::instructions::reserved_server(name) {
                return Err(Error::Unsupported(format!(
                    "invalid native MCP server name {name:?}"
                )));
            }
        }
        let grok = runtime.protocol == Protocol::Acp && runtime.id == "grok";
        let grok_mcp = if grok {
            crate::grok::mcp::Startup::for_options(&options, &journal)?
        } else {
            None
        };
        let cursor_acp = runtime.protocol == Protocol::Acp && runtime.id == "cursor";
        let parameterized =
            cursor_acp && crate::cursor_models::use_parameters(runtime, &options).await?;
        if grok
            && options.resume.is_some()
            && options.settings.append_system_prompt.is_some()
            && options.settings.system_prompt.is_none()
        {
            return Err(Error::Unsupported(
                "Grok cannot reapply append-only instructions on resume".into(),
            ));
        }
        let mut prompt_files =
            crate::options::instructions::launch(&mut process, runtime, &options, &cwd)?;
        let mut opencode_agents = vec![];
        let mut extended_children = false;
        let mut opencode_v1 = false;
        let mut installed_version = version.map(str::to_owned);
        let _startup = if runtime.protocol == Protocol::Acp && runtime.id == "opencode" {
            let lock = crate::options::opencode::startup(&options, &cwd).await?;
            let release = crate::options::opencode::release(runtime, &options).await?;
            installed_version = Some(release.to_string());
            opencode_v1 = release.major == 1;
            extended_children = release.major == 2 && release >= semver::Version::new(2, 3, 0);
            if options.settings.system_prompt.is_some()
                || options.settings.append_system_prompt.is_some()
            {
                let prepared = crate::options::opencode::prepare(
                    runtime,
                    &options,
                    fork.map(|f| f.0),
                    &release,
                )
                .await?;
                process.env("OPENCODE_CONFIG_CONTENT", prepared.content);
                opencode_agents = prepared.agents;
                prompt_files.extend(prepared.file);
            }
            Some(lock)
        } else {
            None
        };
        let mut secrets = crate::options::session_secrets(&options);
        if runtime.protocol == Protocol::Qoder {
            secrets.extend(crate::qoder::credentials(&options));
        }
        secrets.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        let capture =
            if runtime.protocol == Protocol::Claude && options.settings.model_requests.is_some() {
                Some(
                    crate::model_request::Owner::new(runtime, &options, &cwd, &id, secrets.clone())
                        .await?,
                )
            } else {
                None
            };
        let reader = capture.as_ref().map(|owner| owner.reader.clone());
        let mut private_mcp = match runtime.protocol {
            Protocol::Claude => crate::options::mcp_file::prepare("kar-claude-mcp-", &options)?,
            Protocol::Qoder => crate::options::mcp_file::prepare("kar-qoder-mcp-", &options)?,
            _ => None,
        };
        let mut qoder = None;
        let mut memory = None;
        match runtime.protocol {
            Protocol::Codex if options.settings.goals => {
                process.args(["-c", "features.goals=true"]);
            }
            Protocol::Claude => {
                crate::options::claude_environment(&mut process, &options.env);
                if let Some(schema) = &options.settings.output_schema {
                    process
                        .arg("--json-schema")
                        .arg(serde_json::to_string(schema)?);
                }
                if let Some(effort) = &options.settings.effort {
                    process.arg("--effort").arg(effort);
                }
                crate::options::user_configuration::claude(
                    &mut process,
                    &options,
                    &cwd,
                    false,
                    reader.as_ref().map(|r| r.setting.as_str()),
                )?;
                if options.resume.is_none()
                    && let Some(name) = &options.settings.name
                {
                    process.arg("--name").arg(name);
                }
                if !dirs.is_empty() {
                    process.arg("--add-dir").args(&dirs);
                }
                if let Some(config) = &private_mcp {
                    process.arg("--mcp-config").arg(config.path());
                }
                Uuid::parse_str(&id).map_err(|_| {
                    Error::Protocol("Claude resume requires a native session UUID".into())
                })?;
                process.args([
                    "--print",
                    "--verbose",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--replay-user-messages",
                    "--include-partial-messages",
                    "--permission-prompt-tool",
                    "stdio",
                ]);
                if let Some((source, through)) = fork {
                    Uuid::parse_str(source).map_err(|_| {
                        Error::Protocol("Claude fork requires a native source UUID".into())
                    })?;
                    process
                        .arg("--resume")
                        .arg(source)
                        .arg("--fork-session")
                        .arg("--session-id")
                        .arg(&id);
                    if let Some(anchor) = through {
                        Uuid::parse_str(anchor).map_err(|_| {
                            Error::Protocol("Claude anchor must be a native message UUID".into())
                        })?;
                        process.arg("--resume-session-at").arg(anchor);
                    }
                } else {
                    process
                        .arg(if options.resume.is_some() {
                            "--resume"
                        } else {
                            "--session-id"
                        })
                        .arg(&id);
                }
                if let Some(model) = &options.model {
                    process.arg("--model").arg(model);
                }
            }
            Protocol::Qoder => {
                qoder = Some(
                    crate::qoder::prepare(
                        runtime,
                        &options,
                        &cwd,
                        &id,
                        &mut process,
                        &mut prompt_files,
                        private_mcp.as_ref().map(|f| f.path()),
                    )
                    .await?,
                );
            }
            Protocol::Qwen => {
                let (setup, release) =
                    crate::options::qwen_memory::prepare(runtime, &options, version).await?;
                installed_version = release;
                if let Some(dir) = &setup.directory {
                    process.env(
                        "QWEN_CODE_SYSTEM_DEFAULTS_PATH",
                        dir.path().join("system-defaults.json"),
                    );
                }
                memory = Some(setup);
                Uuid::parse_str(&id)
                    .map_err(|_| Error::Protocol("Qwen requires a native session UUID".into()))?;
                process.args([
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--channel=SDK",
                    "--include-partial-messages",
                    "--approval-mode",
                    "default",
                ]);
                process
                    .arg(if options.resume.is_some() {
                        "--resume"
                    } else {
                        "--session-id"
                    })
                    .arg(&id);
                if let Some(model) = &options.model {
                    process.arg("--model").arg(model);
                }
                if let Some(text) = &options.settings.system_prompt {
                    process.arg(format!("--system-prompt={text}"));
                }
                if let Some(text) = &options.settings.append_system_prompt {
                    process.arg(format!("--append-system-prompt={text}"));
                }
                process.args(&options.launch_args);
                if !dirs.is_empty() {
                    process.arg("--include-directories").args(&dirs);
                }
                if !options.settings.disallowed_tools.is_empty() {
                    process
                        .arg("--exclude-tools")
                        .args(&options.settings.disallowed_tools);
                }
            }
            Protocol::Pi => {
                process.arg("--no-mcp");
                if options.pi_session_root.is_some() {
                    process.arg("--session-dir").arg(crate::options::pi_workspace_sessions(&cwd, &options)?);
                }
                if let Some(pi) = &pi {
                    pi.configure(&mut process);
                }
                if let Some(effort) = &options.settings.effort {
                    process.arg("--thinking").arg(effort);
                }
                if let (Some(entries), Some(sdk)) = (pi_mcp_entries, &pi_sdk) {
                    pi_mcp =
                        Some(crate::pi_mcp::prepare(sdk, &options, entries, &mut process).await?);
                }
                if let Some(root) = crate::options::env_path(&options, "KAR_PI_AGENT_DIR") {
                    process.env("PI_CODING_AGENT_DIR", root);
                }
                let existing = options
                    .resume
                    .as_deref()
                    .map(|id| crate::options::pi_file(&cwd, &options, id))
                    .transpose()?;
                if let Some(path) = &existing {
                    process.arg("--session").arg(path);
                }
                if let Some(name) = &options.settings.name
                    && !existing
                        .as_deref()
                        .map(crate::options::pi_has_name)
                        .transpose()?
                        .unwrap_or(false)
                {
                    process.arg("--name").arg(name);
                }
                if let Some(model) = &options.model {
                    process.arg("--model").arg(model);
                }
            }
            _ => {}
        }
        match runtime.protocol {
            Protocol::Codex if !options.launch_args.is_empty() => {
                process
                    .args(&options.launch_args)
                    .args(["--listen", "stdio://"]);
            }
            Protocol::Claude | Protocol::Factory | Protocol::Acp if !grok_stdio => {
                process.args(&options.launch_args);
            }
            _ => {}
        }
        let identity = Arc::new(Mutex::new(Identity::default()));
        if matches!(
            runtime.protocol,
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen
        ) {
            identity.lock().unwrap().session = Some(id.clone());
        }
        if (runtime.protocol == Protocol::Factory
            || runtime.protocol == Protocol::Acp && runtime.id == "cursor")
            && let Some(expected) = &options.resume
        {
            identity.lock().unwrap().session = Some(expected.clone());
        }
        let request_capture = reader
            .map(RequestReader::Claude)
            .or_else(|| pi.as_ref().map(|p| RequestReader::Pi(p.reader.clone())));
        let dsh_patch = if runtime.protocol == Protocol::Acp
            && runtime.id == "dsh"
            && crate::dsh::profile::needs_verification(&options, runtime.protocol)
        {
            Some(
                crate::dsh::profile::Patch::prepare(
                    runtime,
                    &options,
                    version,
                    &mut process,
                    Vec::new(),
                )
                .await?,
            )
        } else {
            None
        };
        let agent_binding = dsh_patch.as_ref().and_then(|p| p.agent_binding());
        let receipt = dsh_patch.as_ref().and_then(|p| p.receipt());
        let mut dsh_startup = if runtime.protocol == Protocol::Acp && runtime.id == "dsh" {
            Some(crate::dsh::profile::session_lock(&options, "acp").await?)
        } else {
            None
        };
        let pi_mcp_receipt = pi_mcp.as_ref().map(crate::pi_mcp::Config::receipt);
        let mcp = McpSupport::new(runtime.protocol, &runtime.id, installed_version.as_deref());
        if runtime.protocol == Protocol::Qwen
            && options
                .mcp_servers
                .values()
                .any(|server| server.headers().is_some())
            && mcp
                .transports
                .is_some_and(|t| !t.contains(&crate::McpTransport::Http))
        {
            return Err(Error::Unsupported(
                "HTTP MCP requires Qwen Code 0.26.0 or later".into(),
            ));
        }
        let child_process = crate::process::OwnedProcess::spawn(process)?;
        if let Some(config) = pi_mcp.as_mut()
            && let Some(pid) = child_process.id()
        {
            config.spawned(pid);
        }
        let process_root = crate::resources::Root::new(child_process.id(), None);
        let native_memory = memory.as_ref().and_then(|m| m.effective);
        let preflight_secrets = Arc::new(secrets.clone());
        let commands = Actor::start(
            runtime,
            &options,
            crate::transport::Launch {
                opencode_v2: runtime.id == "opencode" && !opencode_v1,
                dsh_patch,
                pi_mcp,
                qwen_defaults: memory.and_then(|m| m.directory),
                prompts: prompt_files,
                capture,
                pi,
            },
            child_process,
            journal.clone(),
            identity.clone(),
            secrets,
        );
        let mut session = Self {
            preflight_secrets,
            mcp,
            native_memory,
            cwd: cwd.clone(),
            commands,
            journal,
            id: String::new(),
            protocol: runtime.protocol,
            cursor_parameterized: parameterized,
            manual_compaction: matches!(
                runtime.protocol,
                Protocol::Codex
                    | Protocol::Pi
                    | Protocol::Claude
                    | Protocol::Qoder
                    | Protocol::Factory
            ),
            goals: options.settings.goals,
            info: Value::Null,
            image_input: !matches!(
                runtime.protocol,
                Protocol::Acp | Protocol::Qwen | Protocol::Qoder
            ),
            request_capture,
            process_root,
            factory_dir: (runtime.protocol == Protocol::Factory)
                .then(|| crate::factory::recovery::directory(&options, &cwd))
                .flatten(),
            dsh_file: None,
            text_limit: crate::input::text_limit(&runtime.id),
            native_resume: runtime.protocol != Protocol::Acp,
            acp_steering: false,
            supports_output_schema: runtime.protocol.supports_output_schema(&runtime.id),
            native_fork: matches!(runtime.protocol, Protocol::Codex | Protocol::Claude),
            timeout: if runtime.protocol == Protocol::Acp {
                options.acp_timeout(&runtime.id, "initialize")
            } else {
                options.handshake_timeout
            },
        };
        if let Some(path) = receipt
            && let Err(err) = path
                .confirm(session.commands.closed(), options.handshake_timeout)
                .await
        {
            let _ = session.close().await;
            return Err(err);
        }
        let expected_resume = options.resume.clone();
        use crate::opening::OpenStep;
        match runtime.protocol {
            Protocol::Factory => {
                (id, session.info) = crate::factory::open(&session, &options, &cwd).await?;
            }
            Protocol::Qoder => {
                let opened = qoder
                    .expect("Qoder launch prepared")
                    .open(&session, &options, &id, cwd_str)
                    .await;
                session.info = match opened {
                    Ok(info) => info,
                    Err(error) => {
                        let _ = session.close().await;
                        return Err(error);
                    }
                };
            }
            Protocol::Codex => {
                session.rpc("initialize",json!({"clientInfo":{"name":"kar","title":"Kar","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
                session
                    .call(|tx| Command::Send(json!({"jsonrpc":"2.0","method":"initialized"}), tx))
                    .await?;
                let config = if session.goals
                    || options.settings.append_system_prompt.is_some()
                    || !options.settings.disallowed_tools.is_empty()
                {
                    session
                        .rpc("config/read", json!({"cwd":cwd_str,"includeLayers":false}))
                        .await?
                } else {
                    Value::Null
                };
                if session.goals && config["goals"] != true {
                    return Err(Error::Refused(
                        "Codex did not confirm that goals are enabled".into(),
                    ));
                }
                let mut params = json!({"cwd":cwd_str});
                if !dirs.is_empty() {
                    let mut roots = vec![cwd.clone()];
                    roots.extend(dirs.iter().filter(|dir| **dir != cwd).cloned());
                    params["runtimeWorkspaceRoots"] = serde_json::to_value(roots)?;
                }
                if options.approvals == crate::ApprovalMode::Host {
                    params["approvalPolicy"] = "on-request".into();
                }
                if !options.mcp_servers.is_empty() {
                    let servers: serde_json::Map<_, _> = options
                        .mcp_servers
                        .iter()
                        .map(|(name, s)| (name.clone(), s.codex()))
                        .collect();
                    params["config"] = json!({"mcp_servers":servers});
                }
                crate::options::instructions::codex_open(
                    &mut params,
                    &options.settings,
                    &options.mcp_servers,
                    &config,
                )?;
                if let Some(tier) = &options.settings.service_tier {
                    params["serviceTier"] = tier.as_str().into();
                }
                if fork.is_none()
                    && options.resume.is_none()
                    && let Some(effort) = &options.settings.effort
                {
                    params["config"]["model_reasoning_effort"] = effort.as_str().into();
                }
                if let Some(model) = options.model.as_deref() {
                    params["model"] = model.into();
                }
                let method = if let Some((source, through)) = fork {
                    params["threadId"] = source.into();
                    if let Some(anchor) = through {
                        params["lastTurnId"] = anchor.into();
                        params["excludeTurns"] = false.into();
                    }
                    "thread/fork"
                } else if let Some(resume) = options.resume.as_deref() {
                    params["threadId"] = resume.into();
                    "thread/resume"
                } else {
                    "thread/start"
                };
                let step = OpenStep::Codex {
                    target: params["threadId"].as_str().map(str::to_owned),
                    service_tier: options.settings.service_tier.clone(),
                };
                session.info = session.open_rpc(method, params, step).await?;
                id = required_id(&session.info, "/thread/id")?;
                if let Some(model) = session.info["model"].as_str() {
                    options.confirm_model("Codex", Some(model))?;
                }
                session
                    .confirm_codex_settings(&id, method, &options.settings)
                    .await?;
                if !dirs.is_empty() {
                    let roots = session.info["runtimeWorkspaceRoots"]
                        .as_array()
                        .ok_or_else(|| {
                            Error::Refused("Codex did not report runtimeWorkspaceRoots".into())
                        })?;
                    if std::iter::once(&cwd).chain(&dirs).any(|dir| {
                        !roots
                            .iter()
                            .any(|p| p.as_str().map(std::path::Path::new) == Some(dir.as_path()))
                    }) {
                        return Err(Error::Refused(
                            "Codex did not confirm all requested workspace roots".into(),
                        ));
                    }
                }
                if let Some(name) = &options.settings.name
                    && (method != "thread/resume"
                        || session
                            .info
                            .pointer("/thread/name")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty))
                {
                    session
                        .rpc("thread/name/set", json!({"threadId":id,"name":name}))
                        .await?;
                }
                if let Some((_, Some(anchor))) = fork
                    && session
                        .info
                        .pointer("/thread/turns")
                        .and_then(Value::as_array)
                        .and_then(|v| v.last())
                        .and_then(|turn| turn["id"].as_str())
                        != Some(anchor)
                {
                    return Err(Error::Protocol(
                        "Codex fork history did not end at the requested anchor".into(),
                    ));
                }
            }
            Protocol::Acp => {
                let mut hello = json!({"protocolVersion":1,"clientInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{}});
                if cursor_acp {
                    hello["clientCapabilities"] = crate::cursor_models::capabilities(parameterized);
                }
                if extended_children {
                    hello["clientCapabilities"]["_meta"][crate::wire::opencode::CAPABILITY] =
                        true.into();
                }
                if grok {
                    hello["clientCapabilities"]["terminal"] = true.into();
                    hello["_meta"] = crate::grok::initialize(&options);
                }
                let init = session
                    .open_rpc("initialize", hello, OpenStep::Open)
                    .await?;
                dsh_startup.take();
                session.mcp = McpSupport::acp(&runtime.id, installed_version.as_deref(), &init);
                let steering =
                    crate::wire::acp_prompt::Steering::negotiate(&runtime.id, opencode_v1, &init);
                session.acp_steering = steering.is_some();
                identity.lock().unwrap().acp_steering = steering;
                // Native OpenCode releases were resolved before launch, including for pinned binaries.
                session.manual_compaction = grok || (runtime.id == "opencode" && !opencode_v1);
                session.native_fork = init
                    .pointer("/agentCapabilities/sessionCapabilities/fork")
                    .is_some_and(|v| v.is_object() || v == &Value::Bool(true));
                let caps = &init["agentCapabilities"]["sessionCapabilities"];
                let resume_rpc = crate::opening::advertised(caps, "resume");
                session.native_resume =
                    resume_rpc || init["agentCapabilities"]["loadSession"] == true;
                // Grok accepts standard ACP image blocks even when its handshake omits or denies image support.
                session.image_input = grok
                    || init
                        .pointer("/agentCapabilities/promptCapabilities/image")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                if init["protocolVersion"] != 1 {
                    return Err(Error::Protocol("ACP version 1 was not negotiated".into()));
                }
                let auth_method = match runtime.id.as_str() {
                    "grok" => crate::grok::auth_method(&init),
                    "kimi" => init["authMethods"].as_array().and_then(|methods| {
                        methods
                            .iter()
                            .any(|m| m["id"] == "login")
                            .then_some("login")
                    }),
                    _ => None,
                };
                if let Some(id) = auth_method {
                    session
                        .open_rpc("authenticate", json!({"methodId":id}), OpenStep::Open)
                        .await?;
                }
                if options.mcp_servers.values().any(|s| s.headers().is_some())
                    && init.pointer("/agentCapabilities/mcpCapabilities/http")
                        != Some(&Value::Bool(true))
                {
                    return Err(Error::Unsupported(
                        "ACP server did not advertise HTTP MCP".into(),
                    ));
                }
                let servers: Vec<_> = options
                    .mcp_servers
                    .iter()
                    .filter(|_| runtime.id != "dsh")
                    .map(|(name, s)| s.acp(name))
                    .collect();
                let mut params = json!({"cwd":cwd_str,"mcpServers":servers});
                // DSH registers its directories on the exact native agent scope.
                if !dirs.is_empty() && runtime.id != "dsh" {
                    if !crate::opening::advertised(caps, "additionalDirectories") {
                        return Err(Error::Unsupported(
                            "ACP agent did not advertise additionalDirectories".into(),
                        ));
                    }
                    params["additionalDirectories"] = serde_json::to_value(&dirs)?;
                }
                if runtime.id == "antigravity"
                    && let Some(append) = &options.settings.append_system_prompt
                {
                    if init.pointer("/agentCapabilities/_meta/antigravity/systemPrompt/append")
                        != Some(&Value::Bool(true))
                    {
                        return Err(Error::Unsupported(
                            "Antigravity did not advertise systemPrompt.append".into(),
                        ));
                    }
                    params["_meta"] = json!({"antigravity":{"systemPrompt":{"append":append}}});
                }
                let started_at = tokio::time::Instant::now();
                let source = fork.map(|(s, _)| s).or(options.resume.as_deref());
                if let Some(source) = source
                    && matches!(
                        runtime.id.as_str(),
                        "kimi" | "opencode" | "antigravity" | "devin"
                    )
                    && (if fork.is_some() {
                        session.native_fork
                    } else {
                        session.native_resume
                    })
                {
                    crate::opening::verify_acp_cwd(
                        &session,
                        caps,
                        source,
                        &cwd,
                        &crate::options::session_secrets(&options),
                    )
                    .await?;
                }
                let (method, step) = if let Some((source, _)) = fork {
                    if !session.native_fork {
                        return Err(Error::Unsupported(
                            "ACP agent did not advertise session/fork".into(),
                        ));
                    }
                    params["sessionId"] = source.into();
                    ("session/fork", OpenStep::Target(source.into()))
                } else if let Some(resume) = &options.resume {
                    if !session.native_resume {
                        return Err(Error::Unsupported(
                            "agent does not advertise native session resume".into(),
                        ));
                    }
                    params["sessionId"] = resume.clone().into();
                    let load_history = replay && init["agentCapabilities"]["loadSession"] == true;
                    let method = if resume_rpc && !load_history {
                        "session/resume"
                    } else {
                        "session/load"
                    };
                    (method, OpenStep::Target(resume.clone()))
                } else {
                    if let Some(binding) = &agent_binding {
                        binding.begin()?;
                    }
                    ("session/new", OpenStep::Open)
                };
                let open_limit = options.acp_timeout(&runtime.id, method);
                session.info = session
                    .acp_rpc(method, params, Some(step), open_limit)
                    .await?;
                if let Some(resume) = &options.resume
                    && fork.is_none()
                {
                    if !session.info.is_object() {
                        return Err(Error::Protocol(
                            "expected an ACP resume result object".into(),
                        ));
                    }
                    if session
                        .info
                        .get("sessionId")
                        .is_some_and(|native| native.as_str() != Some(resume))
                    {
                        return Err(Error::Protocol(
                            "ACP resume did not confirm the requested session id".into(),
                        ));
                    }
                    id = resume.clone();
                } else {
                    id = required_id(&session.info, "/sessionId")?;
                    if Some(id.as_str()) == source {
                        return Err(Error::Protocol(
                            "fork returned the source session id".into(),
                        ));
                    }
                }
                if let Some(startup) = grok_mcp
                    && let Err(err) = startup.wait(&id, started_at, open_limit).await
                {
                    let _ = session.close().await;
                    return Err(err);
                }
                crate::options::opencode::confirm(&session.info, &opencode_agents)?;
                if let Some(mode) = &options.settings.acp_mode {
                    if !crate::options::acp_mode_advertised(&session.info, &runtime.id, mode) {
                        return Err(Error::Unsupported(format!(
                            "ACP did not advertise session mode {mode:?}"
                        )));
                    }
                    session
                        .open_rpc(
                            "session/set_mode",
                            json!({"sessionId":id,"modeId":mode}),
                            OpenStep::Open,
                        )
                        .await?;
                    if let Some(modes) =
                        session.info.get_mut("modes").and_then(Value::as_object_mut)
                    {
                        modes.insert("currentModeId".into(), json!(mode));
                    }
                    if let Some(option) = session
                        .info
                        .get_mut("configOptions")
                        .and_then(Value::as_array_mut)
                        .and_then(|opts| opts.iter_mut().find(|o| o["id"] == "mode"))
                    {
                        option["currentValue"] = json!(mode);
                    }
                }
                let antigravity = runtime.id == "antigravity";
                if antigravity {
                    session.info =
                        crate::antigravity::configure(&session, &id, &session.info, &options)
                            .await?;
                } else if let Some(model) = options.model {
                    if let Some(key) = crate::options::acp_model_option(&session.info)
                        .and_then(|v| v["id"].as_str())
                    {
                        let applied = session
                            .open_rpc(
                                "session/set_config_option",
                                json!({"sessionId":id,"configId":key,"value":model}),
                                OpenStep::Model,
                            )
                            .await?;
                        if crate::options::acp_model_option(&applied)
                            .and_then(|v| v["currentValue"].as_str())
                            != Some(&model)
                        {
                            return Err(Error::Refused(
                                "agent did not confirm the selected model".into(),
                            ));
                        }
                        session.info["configOptions"] = applied["configOptions"].clone();
                    } else {
                        session
                            .open_rpc(
                                "session/set_model",
                                json!({"sessionId":id,"modelId":model}),
                                OpenStep::Model,
                            )
                            .await?;
                    }
                }
                if !antigravity && let Some(effort) = &options.settings.effort {
                    let option = crate::options::acp_effort_option(&session.info, cursor_acp)
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "ACP did not advertise thought_level for effort {effort:?}"
                            ))
                        })?;
                    if option["currentValue"] != effort.as_str() {
                        let key = option["id"]
                            .as_str()
                            .ok_or_else(|| Error::Protocol("ACP effort option has no id".into()))?;
                        let reply = session
                            .rpc(
                                "session/set_config_option",
                                json!({"sessionId":id,"configId":key,"value":effort}),
                            )
                            .await?;
                        if cursor_acp
                            && let Some(expected) = crate::options::acp_model_option(&session.info)
                                .and_then(|v| v["currentValue"].as_str())
                            && crate::options::acp_model_option(&reply)
                                .and_then(|v| v["currentValue"].as_str())
                                != Some(expected)
                        {
                            return Err(Error::Refused(format!(
                                "Cursor changed the model while applying effort {effort:?}"
                            )));
                        }
                        let confirmed = crate::options::acp_effort_option(&reply, cursor_acp)
                            .is_some_and(|v| {
                                v["id"] == key && v["currentValue"] == effort.as_str()
                            });
                        if !confirmed {
                            return Err(Error::Refused(format!(
                                "ACP did not confirm effort {effort:?}"
                            )));
                        }
                        session.info["configOptions"] = reply["configOptions"].clone();
                    }
                }
            }
            Protocol::Claude => {
                let initialized = session.rpc("initialize", json!({})).await;
                if let Some(error) = identity.lock().unwrap().claude_open_refusal.take() {
                    return Err(error);
                }
                session.info = initialized?;
                if let Some(config) = private_mcp.take() {
                    config.finish(options.handshake_timeout).await?;
                }
                if session.request_capture.is_some() {
                    session
                        .rpc(crate::model_request::CONFIRM, json!({}))
                        .await?;
                }
                if expected_resume.is_some() || fork.is_some() {
                    // Older runtimes can refuse this optional accounting baseline.
                    let _ = session
                        .rpc(
                            crate::wire::usage::CLAUDE_BASELINE,
                            json!({"skip_behaviors":true}),
                        )
                        .await;
                }
                if let Some(effort) = &options.settings.effort
                    && session.rpc("get_settings", json!({})).await?.as_str() != Some(effort)
                {
                    return Err(Error::Refused(format!(
                        "Claude did not confirm effort {effort:?}"
                    )));
                }
                if let Some(tier) = &options.settings.service_tier
                    && session.info["fast_mode_state"] != if tier == "fast" { "on" } else { "off" }
                {
                    return Err(Error::Refused(
                        "Claude did not confirm requested service tier".into(),
                    ));
                }
                if fork.is_some()
                    && session.rpc("get_status", json!({})).await?.as_str() != Some(id.as_str())
                {
                    return Err(Error::Protocol(
                        "Claude did not confirm the fork session id".into(),
                    ));
                }
            }
            Protocol::Qwen => {
                let servers: serde_json::Map<_, _> = options
                    .mcp_servers
                    .iter()
                    .map(|(name, server)| (name.clone(), server.qwen()))
                    .collect();
                let mut init = json!({"mcpServers":servers});
                if let Some(effort) = &options.settings.effort {
                    init["effort"] = effort.as_str().into();
                }
                session.info = session.rpc("initialize", init).await.map_err(|error| {
                    qwen::initialize_error(&session, error, options.settings.effort.as_deref())
                })?;
                if let Some(effort) = &options.settings.effort {
                    session
                        .confirm_qwen_effort(effort, options.model.as_deref())
                        .await?;
                }
                if required_id(&session.info, "/session_id")? != id {
                    let message = if expected_resume.is_some() {
                        "Qwen resumed a different session"
                    } else {
                        "Qwen initialized a different session"
                    };
                    return Err(Error::Protocol(message.into()));
                }
            }
            Protocol::Pi => {
                session.info = session.rpc("get_state", json!({})).await?;
                if let Some(path) = pi_mcp_receipt {
                    if !std::fs::read(path).is_ok_and(|v| v == b"registered") {
                        let _ = session.close().await;
                        return Err(Error::Refused(
                            "Pi did not register the requested MCP servers".into(),
                        ));
                    }
                    session.mcp.transports = Some(crate::options::mcp_capabilities::BOTH);
                }
                session.native_fork = session.info["sessionFile"].is_string();
                let model = &session.info["model"];
                if let Some(RequestReader::Pi(capture)) = &session.request_capture
                    && let Err(error) = capture.verify(model).await
                {
                    let _ = session.close().await;
                    return Err(error);
                }
                let reported = model["provider"]
                    .as_str()
                    .zip(model["id"].as_str())
                    .map(|(p, m)| format!("{p}/{m}"));
                options.confirm_model("Pi", reported.as_deref())?;
                if let Some(effort) = &options.settings.effort
                    && session.info["thinkingLevel"] != effort.as_str()
                {
                    return Err(Error::Refused(format!(
                        "Pi did not confirm thinking level {effort:?}"
                    )));
                }
                id = required_id(&session.info, "/sessionId")?;
            }
            Protocol::Cybion | Protocol::Morph | Protocol::KimiKap | Protocol::Dsh => {
                return Err(Error::Protocol("expected a stdio runtime".into()));
            }
        }
        if fork.is_some_and(|(source, _)| source == id) {
            return Err(Error::Protocol(
                "fork returned the source session id".into(),
            ));
        }
        if expected_resume.is_some_and(|expected| expected != id) {
            return Err(Error::Protocol(
                "runtime resumed a different session".into(),
            ));
        }
        if let Some(binding) = agent_binding
            && let Err(error) = binding
                .bind(&id)?
                .confirm(session.commands.closed(), options.handshake_timeout)
                .await
        {
            let _ = session.close().await;
            return Err(error);
        }
        if let Some(config) = private_mcp {
            config.finish(options.handshake_timeout).await?;
        }
        identity.lock().unwrap().session = Some(id.clone());
        session.journal.bind_root(&id);
        session.id = id;
        if session.goals {
            session.read_goal().await?;
        }
        Ok(session)
    }
    async fn confirm_codex_settings(
        &self,
        id: &str,
        method: &str,
        settings: &crate::SessionSettings,
    ) -> Result<()> {
        if let Some(requested) = &settings.service_tier {
            let actual = self.info.get("serviceTier").and_then(|v| {
                if v.is_null() {
                    Some("default")
                } else {
                    v.as_str()
                }
            });
            if actual != Some(requested.as_str()) {
                return Err(Error::Refused(
                    "Codex did not confirm requested service tier".into(),
                ));
            }
        }
        let Some(effort) = &settings.effort else {
            return Ok(());
        };
        if self.info["reasoningEffort"] == effort.as_str() {
            return Ok(());
        }
        if method == "thread/start" {
            return Err(Error::Refused(format!(
                "Codex did not confirm reasoning effort {effort:?}"
            )));
        }
        // Subscribe before the update: the notification may arrive before its RPC reply.
        let cursor = self.records().last().map(|r| r.cursor());
        let mut updates = self.subscribe(cursor.as_ref())?;
        self.open_rpc(
            "thread/settings/update",
            json!({"threadId":id,"effort":effort}),
            crate::opening::OpenStep::Open,
        )
        .await?;
        tokio::time::timeout(self.timeout, async {
            while let Some(record) = updates.next().await {
                if record.native["method"] == "thread/settings/updated"
                    && record
                        .native
                        .pointer("/params/threadId")
                        .and_then(Value::as_str)
                        == Some(id)
                {
                    if record
                        .native
                        .pointer("/params/threadSettings/effort")
                        .and_then(Value::as_str)
                        == Some(effort.as_str())
                    {
                        return Ok(());
                    }
                    return Err(Error::Refused(
                        "Codex kept a different reasoning effort".into(),
                    ));
                }
            }
            Err(Error::Closed(
                "Codex closed before confirming effort".into(),
            ))
        })
        .await
        .map_err(|_| Error::Timeout("waiting for confirmed Codex effort".into()))?
    }
    async fn call<T>(&self, f: impl FnOnce(oneshot::Sender<Result<T>>) -> Command) -> Result<T> {
        self.call_with_timeout(Some(self.timeout), f).await
    }
    async fn call_with_timeout<T>(
        &self,
        timeout: Option<Duration>,
        f: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        let command = f(tx);
        let reject = |cmd: &Command| {
            cmd.reject_unreachable(&self.journal, self.protocol, &self.preflight_secrets)
        };
        if let Some(error) = reject(&command) {
            return Err(error);
        }
        self.commands.send(command).await.map_err(|error| {
            reject(&error.0).unwrap_or_else(|| Error::Closed("runtime stopped".into()))
        })?;
        let reply = match timeout {
            Some(limit) => tokio::time::timeout(limit, rx)
                .await
                .map_err(|_| Error::Timeout("agent acknowledgement".into()))?,
            None => rx.await,
        };
        reply.map_err(|_| Error::Closed("runtime stopped before replying".into()))?
    }
    async fn acp_rpc(
        &self,
        method: &str,
        params: Value,
        step: Option<crate::opening::OpenStep>,
        limit: Duration,
    ) -> Result<Value> {
        let response = tokio::time::timeout(
            limit,
            self.call_with_timeout(None, |tx| Command::Rpc(method.into(), params, step, tx)),
        )
        .await;
        match response {
            Ok(result) => result,
            Err(_) => {
                if self.id.is_empty() {
                    // Opening failed. Reap this process before exposing the timeout to a caller that may retry.
                    let _ = self.close().await;
                }
                Err(Error::Timeout(format!(
                    "{method} ({} ms)",
                    limit.as_millis()
                )))
            }
        }
    }
    pub(crate) async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        if self.protocol == Protocol::Acp {
            return self.acp_rpc(method, params, None, self.timeout).await;
        }
        self.call(|tx| Command::Rpc(method.into(), params, None, tx))
            .await
    }
    pub(crate) async fn open_rpc(
        &self,
        method: &str,
        params: Value,
        step: crate::opening::OpenStep,
    ) -> Result<Value> {
        if self.protocol == Protocol::Acp {
            return self.acp_rpc(method, params, Some(step), self.timeout).await;
        }
        self.call(|tx| Command::Rpc(method.into(), params, Some(step), tx))
            .await
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.journal.is_closed()
    }
    pub fn stream_id(&self) -> Uuid {
        self.journal.id()
    }
    pub(crate) fn attach_chat(
        &self,
        chat: crate::chat::Sink,
        retained_bytes: Option<usize>,
    ) -> Result<()> {
        self.journal.attach_chat(chat, retained_bytes)
    }
    pub fn info(&self) -> &Value {
        &self.info
    }
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_memory: self.native_memory,
            protocol: self.protocol,
            image_input: self.image_input,
            resume: self.native_resume,
            fork: self.native_fork,
            steering: self.acp_steering
                || matches!(
                    self.protocol,
                    Protocol::Codex
                        | Protocol::Claude
                        | Protocol::Pi
                        | Protocol::Cybion
                        | Protocol::Factory
                        | Protocol::Morph
                        | Protocol::KimiKap
                        | Protocol::Dsh
                        | Protocol::Qoder
                ),
            session_mcp: !matches!(self.protocol, Protocol::Morph | Protocol::Cybion),
            mcp_transports: self.mcp.transports,
            mcp_startup: self.mcp.startup.or_else(|| {
                (self.protocol == Protocol::Codex && self.journal.has_mcp_startup())
                    .then_some(McpSupport::PER_SERVER)
            }),
            model_requests: self.request_capture.is_some(),
            native_session_file: self.factory_dir.is_some()
                || self.dsh_file.is_some()
                || self.reported_session_file().is_some(),
            host_queue: self.protocol != Protocol::Codex,
            durable_native_queue: self.protocol == Protocol::Codex,
            output_schema: self.supports_output_schema,
            stop_task: matches!(
                self.protocol,
                Protocol::Codex
                    | Protocol::Claude
                    | Protocol::Qoder
                    | Protocol::KimiKap
                    | Protocol::Dsh
            ),
            goals: self.goals,
            manual_compaction: self.manual_compaction,
        }
    }
    pub fn placement(&self) -> Option<crate::SessionPlacement> {
        self.journal.placement()
    }

    pub fn control_receipt(&self, request_id: &str) -> Option<crate::ControlReceipt> {
        self.journal.control_receipt(request_id)
    }

    pub fn records(&self) -> Vec<Arc<Record>> {
        self.journal.records()
    }
    /// Consistent root queries and the last record they include.
    pub fn snapshot(&self) -> crate::observe::SessionSnapshot {
        self.journal.snapshot()
    }
    pub fn model_readback(&self) -> crate::observe::Readback<Option<String>> {
        self.snapshot().into_readback(|state| state.model)
    }
    pub fn usage_readback(&self) -> crate::observe::Readback<crate::SessionUsage> {
        self.snapshot().into_readback(|state| state.usage)
    }
    /// Read retained context measurements synchronously, including their journal position.
    pub fn context_usage_readback(&self) -> crate::observe::Readback<Option<crate::ContextUsage>> {
        self.snapshot().into_readback(|state| state.context_usage)
    }
    pub fn observe_stalls(&self, threshold: Duration) -> Result<crate::observe::StallSubscription> {
        crate::observe::StallSubscription::new(self, threshold)
    }
    pub fn status(&self) -> crate::observe::AgentStatus {
        self.snapshot().status
    }
    /// Read the local runtime's resident memory and process count without recording an event.
    pub async fn resources(&self) -> Option<crate::SessionResources> {
        let root = self.process_root.as_ref()?;
        if !root.shared() && self.journal.is_closed() {
            return None;
        }
        let reading = root.read().await;
        if !root.shared() && self.journal.is_closed() {
            return None;
        }
        reading
    }
    /// Read the latest native request for this open without appending a record.
    pub async fn last_model_request(&self) -> Result<Option<crate::ModelRequest>> {
        let capture = self
            .request_capture
            .as_ref()
            .ok_or_else(|| Error::Unsupported("session opened without model_requests".into()))?;
        capture.read().await
    }
    pub fn usage(&self) -> crate::SessionUsage {
        self.usage_for(&self.id)
    }
    pub fn usage_for(&self, session: &str) -> crate::SessionUsage {
        self.journal.usage_for(session)
    }
    pub fn graph(&self) -> crate::observe::SessionGraph {
        self.snapshot().graph
    }
    pub fn model(&self) -> Option<String> {
        self.snapshot().model
    }
    pub fn effort(&self) -> Option<String> {
        self.snapshot().effort
    }
    pub fn service_tier(&self) -> Option<String> {
        self.snapshot().service_tier
    }
    /// Subscribe to projected events, including siblings after the supplied event cursor.
    pub fn events(&self, cursor: Option<&crate::EventCursor>) -> Result<crate::EventSubscription> {
        self.journal.events(cursor)
    }
    /// Wait for active and already queued work to settle, or for the session to be released.
    pub async fn await_idle(&self) -> Result<crate::observe::SessionSnapshot> {
        let snapshot = self.snapshot();
        if !snapshot.status.is_running() {
            return Ok(snapshot);
        }
        let mut records = self.subscribe(snapshot.cursor.as_ref())?;
        while records.next().await.is_some() {
            let current = self.snapshot();
            if !current.status.is_running() {
                return Ok(current);
            }
        }
        Ok(self.snapshot())
    }
    /// No cursor starts at the first record. A cursor resumes strictly after its record.
    pub fn subscribe(&self, cursor: Option<&Cursor>) -> Result<Subscription> {
        self.journal.subscribe(cursor)
    }
    pub async fn prompt(&self, text: impl Into<String>) -> Result<Turn> {
        self.prompt_images(text, Vec::new()).await
    }
    pub async fn prompt_images(
        &self,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<Turn> {
        self.prompt_images_with_id(Uuid::new_v4(), text, images)
            .await
    }
    /// Dispatch an ordinary prompt with a caller-supplied identity.
    pub async fn prompt_with_id(&self, id: Uuid, text: impl Into<String>) -> Result<Turn> {
        self.prompt_images_with_id(id, text, Vec::new()).await
    }
    pub async fn prompt_images_with_id(
        &self,
        id: Uuid,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<Turn> {
        match self.submit_prompt(Some(id), text, images).await? {
            PromptDecision::Accepted(turn) => Ok(turn),
            PromptDecision::Rejected { error, .. } => Err(error),
        }
    }
    /// Submit an ordinary prompt and inspect the paired public records even
    /// when the runtime or local validation rejects this attempt.
    pub async fn submit_prompt(
        &self,
        input_id: Option<Uuid>,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<PromptDecision> {
        let id = input_id.unwrap_or_else(Uuid::new_v4);
        let input = crate::input::Input::new(text.into(), images);
        let request = input.request_id;
        let result = match self.check_input("prompt", id, request, &input.text, &input.images) {
            Ok(()) => self.start_prompt(id, input, None).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(turn) => Ok(PromptDecision::Accepted(turn)),
            Err(error) => match self.control_receipt(&request.to_string()) {
                Some(receipt) if receipt.response.native["type"] == "rejected" => {
                    Ok(PromptDecision::Rejected { receipt, error })
                }
                _ => Err(error),
            },
        }
    }
    pub(crate) async fn start_prompt(
        &self,
        id: Uuid,
        input: crate::input::Input,
        admission: Option<crate::chat::Admission>,
    ) -> Result<Turn> {
        let (done, receiver) = oneshot::channel();
        crate::input::check_text(&input.text, self.text_limit)?;
        let request_id = input.request_id;
        // A managed turn owns the workspace until the actor actually attempts dispatch.
        // A local timeout could otherwise leave a still-live command behind after releasing it.
        let timeout = admission.is_none().then_some(self.timeout);
        self.call_with_timeout(timeout, |tx| {
            Command::Prompt(id, input, admission, tx, done)
        })
        .await?;
        Ok(Turn {
            input_id: id,
            admission: self.control_receipt(&request_id.to_string()),
            receiver,
        })
    }
    /// Queue an input, using the backend's native or adapter queue.
    pub async fn queue(&self, text: impl Into<String>) -> Result<(Uuid, Turn)> {
        self.queue_images(text, Vec::new()).await
    }
    pub async fn queue_images(
        &self,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<(Uuid, Turn)> {
        let id = Uuid::new_v4();
        self.queue_images_with_id(id, text, images)
            .await
            .map(|turn| (id, turn))
    }
    pub async fn queue_with_id(&self, id: Uuid, text: impl Into<String>) -> Result<Turn> {
        self.queue_images_with_id(id, text, Vec::new()).await
    }
    /// Keep the loaded attachment bytes and logical input identity through native or adapter queuing.
    pub async fn queue_images_with_id(
        &self,
        id: Uuid,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<Turn> {
        let (done, receiver) = oneshot::channel();
        let input = crate::input::Input::new(text.into(), images);
        let request = input.request_id;
        self.check_input("queue", id, request, &input.text, &input.images)?;
        crate::input::check_text(&input.text, self.text_limit)?;
        self.call(|tx| Command::Queue(id, input, tx, done)).await?;
        Ok(Turn {
            input_id: id,
            admission: self.control_receipt(&request.to_string()),
            receiver,
        })
    }
    pub async fn withdraw(&self, id: Uuid) -> Result<()> {
        self.call(|tx| Command::Withdraw(id, tx)).await
    }
    /// Start now when idle, steer a live native turn when supported, otherwise queue.
    pub async fn deliver(&self, text: impl Into<String>) -> Result<Delivery> {
        self.deliver_with_id(Uuid::new_v4(), text).await
    }
    /// Keep one logical input ID across a status race, steering refusal and queue fallback.
    pub async fn deliver_with_id(
        &self,
        input_id: Uuid,
        text: impl Into<String>,
    ) -> Result<Delivery> {
        self.deliver_images_with_id(input_id, text, Vec::new())
            .await
    }
    pub async fn deliver_images_with_id(
        &self,
        input_id: Uuid,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<Delivery> {
        let text = text.into();
        let running_since = || match self.status() {
            crate::observe::AgentStatus::Running { since_seq, .. } => Some(since_seq),
            _ => None,
        };
        for _ in 0..4 {
            if running_since().is_none() {
                match self
                    .prompt_images_with_id(input_id, text.clone(), images.clone())
                    .await
                {
                    Ok(turn) => return Ok(Delivery::Started(turn)),
                    Err(Error::Busy) => {}
                    Err(error) => return Err(error),
                }
            }
            let Some(target) = running_since() else {
                continue;
            };
            if self.capabilities().steering {
                match self
                    .steer_images_with_id(input_id, text.clone(), images.clone())
                    .await
                {
                    Ok(()) => return Ok(Delivery::Steered),
                    Err(Error::Refused(_) | Error::Unsupported(_))
                        if running_since() != Some(target) =>
                    {
                        continue;
                    }
                    Err(Error::Unsupported(_)) => {}
                    Err(Error::Refused(reason))
                        if matches!(
                            reason.as_str(),
                            "no active turn"
                                | "no active DSH turn"
                                | "runtime has not acknowledged this turn"
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
            let turn = self.queue_images_with_id(input_id, text, images).await?;
            return Ok(Delivery::Queued { input_id, turn });
        }
        Err(Error::Busy)
    }
    pub async fn steer(&self, text: impl Into<String>) -> Result<()> {
        self.steer_with_id(Uuid::new_v4(), text).await
    }
    /// Send a mid-turn input with a caller-supplied identity for observers.
    pub async fn steer_with_id(&self, id: Uuid, text: impl Into<String>) -> Result<()> {
        self.steer_images_with_id(id, text, Vec::new()).await
    }
    pub async fn steer_images(
        &self,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<()> {
        self.steer_images_with_id(Uuid::new_v4(), text, images)
            .await
    }
    pub async fn steer_images_with_id(
        &self,
        id: Uuid,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<()> {
        self.steer_input(id, text.into(), images, None).await
    }
    /// Send a new stable input only to the named root attempt. Omit the target request
    /// UUID to pin the current matching input; the actor rechecks that attempt before dispatch.
    pub async fn steer_turn(
        &self,
        target_input_id: Uuid,
        target_request_id: Option<Uuid>,
        input_id: Uuid,
        text: impl Into<String>,
        images: Vec<crate::InputImage>,
    ) -> Result<()> {
        self.steer_input(
            input_id,
            text.into(),
            images,
            Some(crate::transport::TurnTarget {
                input_id: target_input_id,
                request_id: target_request_id,
            }),
        )
        .await
    }
    async fn steer_input(
        &self,
        id: Uuid,
        text: String,
        images: Vec<crate::InputImage>,
        target: Option<crate::transport::TurnTarget>,
    ) -> Result<()> {
        let input = crate::input::Input::new(text, images);
        self.check_input("steer", id, input.request_id, &input.text, &input.images)?;
        crate::input::check_text(&input.text, self.text_limit)?;
        let target = match target {
            Some(target) => match target.pin(&self.journal) {
                Some(pinned) => Some(pinned),
                None => {
                    return self
                        .journal
                        .steer_request(id, &input, Some(target), false, &self.preflight_secrets)
                        .map(drop);
                }
            },
            None => None,
        };
        self.call(|tx| Command::Steer(id, input, target, tx))
            .await
            .map(drop)
    }
    pub fn tasks(&self) -> Vec<crate::NativeTask> {
        self.snapshot().tasks
    }
    /// Stop one provider task. Codex requires an observed descendant and its current native turn id.
    pub async fn stop_task(&self, id: &str) -> Result<()> {
        if !self.capabilities().stop_task {
            return Err(Error::Unsupported(
                "native task stop on this adapter".into(),
            ));
        }
        if id.is_empty() {
            return Err(Error::Refused("a native task id is required".into()));
        }
        self.call(|tx| Command::StopTask(id.into(), None, tx))
            .await
            .map(|_| ())
    }
    /// Mark live background work on this controller; the native model turn may already have ended.
    pub async fn await_background_tasks(
        &self,
        req: crate::BackgroundWaitRequest,
    ) -> Result<crate::BackgroundWaitReceipt> {
        if self.protocol != Protocol::Claude {
            return Err(Error::Unsupported("background waits on this runtime".into()));
        }
        req.validate()?;
        let receipt = self.rpc("kar/background/wait", serde_json::to_value(req)?).await?;
        Ok(serde_json::from_value(receipt)?)
    }

    /// Stop the selected running workflow in its original Claude process.
    pub async fn stop_workflow(&self, id: &str, occurrence: crate::Cursor) -> Result<()> {
        if self.protocol() != Protocol::Claude {
            return Err(Error::Unsupported("native workflow stop on this adapter".into()));
        }
        if id.is_empty() || occurrence.stream_id != self.stream_id() {
            return Err(Error::Refused("workflow occurrence is no longer running".into()));
        }
        self.call(|reply| Command::StopTask(id.into(), Some(occurrence), reply))
            .await
            .map(drop)
    }
    pub async fn abort(&self) -> Result<()> {
        self.abort_prompt(None).await
    }
    /// Interrupt the active root prompt identified by its input UUID.
    /// Providing its request UUID also distinguishes attempts that reuse the input UUID.
    /// The actor rechecks the selected attempt immediately before issuing the native control.
    pub async fn abort_turn(&self, input_id: Uuid, request_id: Option<Uuid>) -> Result<()> {
        let target = crate::transport::TurnTarget {
            input_id,
            request_id,
        }
        .pin(&self.journal)
        .and_then(|t| t.request_id)
        .ok_or_else(crate::transport::AbortRequest::stale)?;
        self.abort_prompt(Some(target.to_string())).await
    }
    async fn abort_prompt(&self, target_request: Option<String>) -> Result<()> {
        self.call(|reply| {
            Command::Abort(crate::transport::AbortRequest {
                target_request,
                reply,
            })
        })
        .await
        .map(drop)
    }
    /// Answer a live request using its protocol-native response payload.
    pub async fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.call(|tx| Command::Respond(id, crate::transport::HostAnswer::Native(result), tx))
            .await
    }
    /// Answer a particular occurrence of a question or another native request.
    pub async fn answer_request(
        &self,
        request: &crate::PendingRequest,
        payload: Value,
    ) -> Result<()> {
        self.call(|tx| {
            Command::Respond(
                request.id.clone(),
                crate::transport::HostAnswer::Checked(request.cursor.clone(), payload),
                tx,
            )
        })
        .await
    }
    pub async fn select_policy(
        &self,
        request: &crate::PendingRequest,
        choice: crate::PolicyChoice,
    ) -> Result<()> {
        self.call(|tx| {
            Command::Respond(
                request.id.clone(),
                crate::transport::HostAnswer::Policy(request.cursor.clone(), choice),
                tx,
            )
        })
        .await
    }
    pub async fn grant_resources(
        &self,
        request: &crate::PendingRequest,
        scope: crate::PermissionDecision,
        resources: Vec<crate::ResourceId>,
    ) -> Result<()> {
        self.call(|tx| {
            Command::Respond(
                request.id.clone(),
                crate::transport::HostAnswer::Resources(request.cursor.clone(), scope, resources),
                tx,
            )
        })
        .await
    }
    pub async fn answer_question(
        &self,
        request: &crate::PendingRequest,
        reply: crate::QuestionReply,
    ) -> Result<()> {
        self.answer_question_with_files(request, reply, Default::default()).await
    }
    pub(crate) async fn answer_question_with_files(
        &self, request: &crate::PendingRequest, reply: crate::QuestionReply,
        files: std::collections::BTreeMap<String,String>,
    ) -> Result<()> {
        self.call(|tx| {
            Command::Respond(
                request.id.clone(),
                crate::transport::HostAnswer::Question(request.cursor.clone(), reply, files),
                tx,
            )
        })
        .await
    }
    /// Submit a one-time permission decision for this specific pending request.
    pub async fn decide(
        &self,
        request: &crate::PendingRequest,
        decision: crate::PermissionDecision,
    ) -> Result<()> {
        self.call(|tx| {
            Command::Respond(
                request.id.clone(),
                crate::transport::HostAnswer::Permission(request.cursor.clone(), decision),
                tx,
            )
        })
        .await
    }
    /// Dispatch compaction and follow its native admission and completion independently.
    pub async fn start_compaction(&self) -> Result<crate::ManualCompaction> {
        self.dispatch_compaction(Uuid::new_v4()).await
    }
    async fn dispatch_compaction(&self, request: Uuid) -> Result<crate::ManualCompaction> {
        if !self.manual_compaction {
            return Err(Error::Unsupported(
                "manual compaction through this protocol".into(),
            ));
        }
        let revision = if self.protocol == Protocol::Claude {
            let v = self
                .rpc(crate::transport::CLAUDE_COMPACT_CATALOG, json!({}))
                .await?;
            Some(
                v.as_u64()
                    .ok_or_else(|| Error::Protocol("invalid command catalog revision".into()))?,
            )
        } else {
            None
        };
        self.call_with_timeout(None, |tx| Command::Compact(request, revision, tx))
            .await
    }
    /// Observe compaction through its terminal frame. A caller limit after dispatch
    /// releases the waiter while the session continues to track the native operation.
    pub async fn compact_with(
        &self,
        options: &crate::CompactionOptions,
    ) -> Result<crate::ManualCompactionReport> {
        if !self.manual_compaction {
            return Err(Error::Unsupported(
                "manual compaction through this protocol".into(),
            ));
        }
        let request = Uuid::new_v4();
        let started = tokio::time::Instant::now();
        let (code, mut reason) = if options.signal.is_cancelled() {
            (
                "aborted",
                "caller cancelled compaction before dispatch".to_owned(),
            )
        } else {
            match self.dispatch_compaction(request).await {
                Ok(op) => {
                    let mut remaining = options.clone();
                    remaining.timeout =
                        options.timeout.map(|d| d.saturating_sub(started.elapsed()));
                    return Ok(op.wait_with(&remaining).await);
                }
                Err(Error::Busy) => ("busy", "another operation owns the session".to_owned()),
                Err(Error::Refused(reason)) => ("runtime_refused", reason),
                Err(Error::Closed(reason)) => ("session_released", reason),
                Err(err) => return Err(err),
            }
        };
        let id = request.to_string();
        let receipt = if let Some(receipt) = self.control_receipt(&id) {
            receipt
        } else {
            crate::transport::redact_text(&mut reason, &self.preflight_secrets);
            // The local rejection and any intervening native exit share one journal lock.
            let _ = self.journal.refused_input(
                crate::Direction::ToRuntime.into(),
                json!({"type":"compact","id":id}),
                request,
                code,
                &reason,
            );
            self.control_receipt(&id)
                .ok_or_else(|| Error::Protocol("missing compaction refusal".into()))?
        };
        let code = serde_json::from_value(receipt.response.native["code"].clone())?;
        let reason = receipt.response.native["reason"]
            .as_str()
            .ok_or_else(|| Error::Protocol("compaction refusal has no reason".into()))?
            .to_owned();
        Ok(crate::ManualCompactionReport {
            request: receipt.request.cursor(),
            response: Some(receipt.response.cursor()),
            status: crate::CompactionStatus::Rejected { code, reason },
        })
    }
    /// Returns the native RPC reply, which can precede native completion.
    pub async fn compact(&self) -> Result<Value> {
        self.start_compaction().await?.native(self.timeout).await
    }
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }
    fn reported_session_file(&self) -> Option<&str> {
        match self.protocol {
            Protocol::Codex => self.info.pointer("/thread/path"),
            Protocol::Pi => self.info.get("sessionFile"),
            _ => None,
        }?
        .as_str()
    }
    /// The provider may report its intended transcript file before it exists.
    pub async fn native_session_file(&self) -> Result<Option<std::path::PathBuf>> {
        if let Some(dir) = &self.factory_dir {
            return crate::factory::recovery::file(dir.clone(), self.id.clone(), self.cwd.clone())
                .await;
        }
        let cached;
        let raw = if let Some(file) = &self.dsh_file {
            if !self.journal.is_closed()
                && let Err(e) = self.rpc("sessionFile", json!({})).await
                && !self.journal.is_closed()
            {
                return Err(e);
            }
            cached = file.lock().unwrap().clone();
            let Some(path) = cached.as_deref() else {
                return Ok(None);
            };
            path
        } else {
            self.reported_session_file().ok_or_else(|| {
                Error::Unsupported("the native runtime did not report a session file".into())
            })?
        };
        let path = std::path::Path::new(raw);
        if !path.is_absolute() || raw.contains("[redacted]") {
            return Ok(None);
        }
        match tokio::fs::metadata(path).await {
            Ok(m) => Ok(m.is_file().then(|| path.to_owned())),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
    pub(crate) async fn recover_morph_turn(
        &self,
        options: &SessionOptions,
        local: &crate::chat::ChatTurn,
        saved: &crate::community::morph::recovery::Saved,
    ) -> Result<crate::community::morph::recovery::Resume> {
        use crate::community::morph::recovery::Resume;
        let (Some(task), Some(prompt)) = (saved.task(local), local.prompt()) else {
            return Ok(Resume::Unconfirmed);
        };
        let info = self.rpc("readTask", json!({"id":task})).await?;
        if saved.running(self.id(), prompt, &info) {
            return saved.active_task();
        }
        let Some(events) = saved.terminal_events(self.id(), prompt, &info)? else {
            return Ok(Resume::Unconfirmed);
        };
        let frame = crate::chat::ChatFrame::morph_recovery(
            &self.journal,
            self.id(),
            task,
            events,
            options,
        )?;
        Ok(frame.map_or(Resume::Unconfirmed, Resume::Finished))
    }
    pub(crate) async fn recover_pi_turn(
        &self,
        options: &crate::SessionOptions,
        local: &crate::chat::ChatTurn,
    ) -> Result<Option<crate::chat::ChatFrame>> {
        let (Some(timestamp), Some(prompt)) = (local.pi_input_timestamp_ms, local.prompt()) else {
            return Ok(None);
        };
        let Some(path) = self.native_session_file().await? else {
            return Ok(None);
        };
        let (root, cwd, prompt) = (self.id.clone(), self.cwd.clone(), prompt.to_owned());
        let attachments = local.attachments.clone();
        let echo = local.pi_image_content_sha256.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            crate::chat::recover_pi(
                &path, &root, &cwd, timestamp, &prompt, &attachments, echo.as_deref(),
            )
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
        let Some((entry, edits)) = snapshot else {
            return Ok(None);
        };
        crate::chat::ChatFrame::transcript(
            &self.journal,
            self.id(),
            entry,
            "pi/transcript",
            edits,
            options,
            |edits| local.transcript_adds_content(self.id(), edits).is_some(),
        )
    }
    pub(crate) fn recover_transcript(
        &self,
        kind: &str,
        user: String,
        edits: Vec<crate::chat::ChatEdit>,
        options: &SessionOptions,
        accept: impl FnOnce(&[crate::chat::ChatEdit]) -> bool,
    ) -> Result<Option<crate::chat::ChatFrame>> {
        crate::chat::ChatFrame::transcript(
            &self.journal,
            self.id(),
            user,
            kind,
            edits,
            options,
            accept,
        )
    }
    pub(crate) async fn recover_factory_turn(
        &self,
        options: &SessionOptions,
        local: &crate::chat::ChatTurn,
        prompt: &str,
    ) -> Result<Option<crate::chat::ChatFrame>> {
        crate::factory::recovery::recover(
            &self.journal,
            self.id(),
            self.cwd(),
            options,
            local,
            prompt,
        )
        .await
    }
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    pub(crate) async fn pages(&self, method: &str, mut params: Value) -> Result<Value> {
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        loop {
            let page = self.rpc(method, params.clone()).await?;
            items.extend(
                page["data"]
                    .as_array()
                    .ok_or_else(|| Error::Protocol(format!("expected {method} data")))?
                    .iter()
                    .cloned(),
            );
            let Some(cursor) = page["nextCursor"].as_str() else {
                break;
            };
            if !seen.insert(cursor.to_owned()) {
                return Err(Error::Protocol(format!("{method} repeated its cursor")));
            }
            params["cursor"] = cursor.into();
        }
        Ok(Value::Array(items))
    }
    pub async fn models(&self) -> Result<Value> {
        if self.cursor_parameterized {
            match self.rpc(crate::cursor_models::LIST, json!({})).await {
                Ok(body) => return crate::cursor_models::project(&body),
                Err(Error::Unsupported(_)) => {}
                Err(error) => return Err(error),
            }
        }
        match self.protocol {
            Protocol::KimiKap | Protocol::Dsh => self.rpc("models", json!({})).await,
            Protocol::Factory => crate::factory::project(
                crate::Catalog::Models,
                &self.rpc("droid.list_models", json!({})).await?,
            ),
            Protocol::Morph => self.rpc("profiles", json!({})).await,
            Protocol::Cybion => Err(Error::Unsupported(
                "Cybion's Agent API has no model catalog".into(),
            )),
            Protocol::Codex => self.pages("model/list", json!({})).await,
            Protocol::Claude => self
                .info
                .get("models")
                .cloned()
                .ok_or_else(|| Error::Unsupported("no model catalog in initialization".into())),
            Protocol::Acp => self
                .info
                .pointer("/models/availableModels")
                .or_else(|| {
                    crate::options::acp_model_option(&self.info).and_then(|v| v.get("options"))
                })
                .cloned()
                .ok_or_else(|| Error::Unsupported("no model catalog in session metadata".into())),
            Protocol::Qoder => {
                self.rpc("get_models", json!({"fetchStrategy":"live"}))
                    .await
            }
            Protocol::Qwen | Protocol::Pi => {
                let mut reply = self.rpc("get_available_models", json!({})).await?;
                if !reply["models"].is_array() {
                    return Err(Error::Protocol(
                        "runtime returned an invalid model catalog".into(),
                    ));
                }
                let mut models = reply["models"].take();
                if self.protocol == Protocol::Qwen {
                    qwen::project_models(models.as_array_mut().expect("checked model array"));
                }
                Ok(models)
            }
        }
    }
    pub async fn close(&self) -> Result<()> {
        if let Some(capture) = &self.request_capture {
            capture.close();
        }
        if self.journal.is_closed() {
            self.journal.dispose_after_exit();
            return Ok(());
        }
        match self.call(Command::Close).await {
            Err(Error::Closed(_)) if self.journal.is_closed() => {
                self.journal.dispose_after_exit();
                Ok(())
            }
            result => result,
        }
    }
}
fn required_id(v: &Value, path: &str) -> Result<String> {
    v.pointer(path)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol(format!("missing native identity at {path}")))
}
