#![forbid(unsafe_code)]
mod account_cli;
mod chat_cli;
mod git_cli;
mod inventory_cli;
use inventory_cli::WorkspaceInventory;
mod mcp;
mod models_cli;
mod providers_cli;
use clap::{Args, Parser, Subcommand};
use kar::{Cursor, Error, Event, Outcome, Record, Result, SessionOptions, runtime, runtimes};
use serde_json::json;
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(version, about = "Rust-native interface to installed coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Save, reopen and continue chats in their original workspaces.
    Chat(chat_cli::Chat),
    /// Serve local projects, workspace files and saved chats to workbench clients.
    Workbench {
        /// Directories in which clients may register projects.
        #[arg(long = "root", required = true)]
        roots: Vec<PathBuf>,
        /// Chat storage directory. Defaults to the normal Kar chat store.
        #[arg(long)]
        state: Option<PathBuf>,
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Manage repositories and independent chat worktrees.
    Git(git_cli::Git),
    /// Report installed executables, versions and pre-open session capabilities.
    List {
        /// Only inspect this runtime.
        runtime: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, requires = "runtime")]
        bin: Option<PathBuf>,
    },
    /// Read installed runtime capabilities and optionally account, model and customization facts.
    Orient {
        /// Runtime IDs in the order to inspect. Omit to inspect the Kar registry.
        #[arg(value_delimiter = ',')]
        runtimes: Vec<String>,
        #[arg(long, value_enum, default_value = "local")]
        depth: kar::orientation::Depth,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Inspect user customization from native catalogs in an empty directory.
        #[arg(long)]
        customization: bool,
        #[arg(long)]
        timeout_ms: Option<std::num::NonZeroU64>,
    },
    /// Install an absent vendor CLI or print its installation plan.
    Install {
        runtime: String,
        #[arg(long)]
        plan: bool,
        /// Select a release line for an OpenCode installation (v1 or v2).
        #[arg(long)]
        line: Option<String>,
        /// Bound installer execution in milliseconds.
        #[arg(long, default_value = "600000")]
        timeout_ms: std::num::NonZeroU64,
    },
    /// Query or update installed CLIs. Omit the runtime to select all installed runtimes.
    Update {
        runtime: Option<String>,
        /// Only query the release the installed native updater would select.
        #[arg(long)]
        check: bool,
        /// Set the per-runtime deadline in milliseconds.
        #[arg(long)]
        timeout_ms: Option<std::num::NonZeroU64>,
    },
    /// Serve a group of local agents to an MCP client over stdio.
    Mcp {
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long, value_delimiter = ',')]
        runtimes: Vec<String>,
        /// Maximum concurrent active agents. Idle sessions may remain open.
        #[arg(long, default_value_t = 4)]
        max_running: usize,
        /// Optional separate cap on the number of open sessions.
        #[arg(long)]
        max_sessions: Option<usize>,
        /// Maximum delegation depth. A child inherits its parent's limit.
        #[arg(long)]
        max_depth: Option<usize>,
        /// Write each child's Kar recording to a new file in this directory.
        #[arg(long)]
        log_dir: Option<PathBuf>,
    },
    /// Use the provider's own interactive login, logout or status command.
    #[command(subcommand_precedence_over_arg = true)]
    Account {
        /// Select one runtime. Status and usage may query every installation.
        runtime: Option<String>,
        #[command(subcommand)]
        action: Account,
    },
    /// Show the credential variables and stores declared for a runtime.
    Credentials { runtime: String },
    /// Manage a runtime's model-provider credentials.
    Providers(providers_cli::Providers),
    /// Read the provider's reported quota windows.
    Usage(Open),
    /// Read skills, commands or MCP servers where the agent exposes a catalog.
    Inventory {
        #[command(flatten)]
        open: Open,
        #[arg(value_enum)]
        kind: Catalog,
    },
    /// Read a workspace's native skill metadata, including scope and partial results.
    Skills(WorkspaceInventory),
    /// Read the commands advertised by a runtime for the current workspace.
    Commands(WorkspaceInventory),
    /// Discover native MCP servers, authentication state and tool counts before opening a chat.
    McpServers(WorkspaceInventory),
    /// List tools in the runtime's native inventory before opening a chat.
    Tools(WorkspaceInventory),
    /// Negotiate a session and show the native session id without sending a prompt.
    Open(Open),
    /// List the running provider's advertised models.
    Models(models_cli::Models),
    /// Print the runtime's declarative settings form, without launching its CLI.
    ConfigForm { runtime: Option<String> },
    /// Send a prompt and stream the reply.
    Run {
        #[command(flatten)]
        open: Open,
        prompt: String,
        #[arg(long = "image")]
        images: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Create a new JSONL recording (existing files are never overwritten).
        #[arg(long)]
        record: Option<PathBuf>,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Import a kar-voyage/3 recording into a Kar JSONL file for replay.
    Import { input: PathBuf, output: PathBuf },
    /// Print previously recorded records, optionally after a stream-specific cursor.
    Replay {
        path: PathBuf,
        #[arg(long, requires = "stream")]
        after: Option<u64>,
        #[arg(long, requires = "after")]
        stream: Option<uuid::Uuid>,
    },
}
#[derive(Subcommand)]
enum Account {
    /// Report account quota windows without creating a chat.
    Usage {
        /// Emit the structured account snapshot (the default).
        #[arg(long)]
        json: bool,
        #[arg(long)]
        bin: Option<PathBuf>,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// Ask Codex for the reset count without fetching individual reset details.
        #[arg(long)]
        count_only: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Inspect Claude's native reset offers interactively. The UI can use subscription quota at startup.
    InspectLimitResets {
        #[arg(long)]
        include_five_hour: bool,
        /// Accept that an older Claude may spend its five-hour reset when the command opens.
        #[arg(long)]
        five_hour_limit_consent: bool,
        #[arg(long)]
        bin: Option<PathBuf>,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Spend a native limit reset. Persist the key and reuse it for a retry of the same attempt.
    RedeemLimitReset {
        /// Accept that an older Claude may spend its five-hour reset in place of the selected kind.
        #[arg(long)]
        five_hour_limit_consent: bool,
        #[arg(long)]
        idempotency_key: String,
        #[arg(long)]
        reset_id: Option<String>,
        #[arg(long)]
        bin: Option<PathBuf>,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    Login {
        /// Stream native pairing events and the final result as JSON lines.
        #[arg(long)]
        json: bool,
        /// Bound this provider's operation in milliseconds.
        #[arg(long)]
        timeout_ms: Option<std::num::NonZeroU64>,
    },
    Logout {
        /// Return the outcome after checking the selected native profile.
        #[arg(long)]
        json: bool,
        /// Bound this provider's operation in milliseconds.
        #[arg(long)]
        timeout_ms: Option<std::num::NonZeroU64>,
    },
    Status {
        /// Return a structured account status suitable for a host application.
        #[arg(long)]
        json: bool,
        /// Bound this provider's operation in milliseconds.
        #[arg(long)]
        timeout_ms: Option<std::num::NonZeroU64>,
    },
}
#[derive(Clone, Copy, clap::ValueEnum)]
enum Catalog {
    Skills,
    Commands,
    McpServers,
}
fn parse_output_schema(s: &str) -> serde_json::Result<serde_json::Map<String, serde_json::Value>> {
    serde_json::from_str(s)
}
#[derive(Args)]
struct NativeSettings {
    /// Pass one native session argument. Repeat for separate arguments.
    #[arg(long = "launch-arg", value_name = "ARG", allow_hyphen_values = true)]
    launch_args: Vec<String>,
    /// Remove one inherited environment variable from the native session. Repeatable.
    #[arg(long = "unset-env", value_name = "NAME", value_parser = parse_env_name)]
    unset_env: Vec<String>,
    /// End the aborted turn's Codex commands or hosted Grok terminals.
    #[arg(long)]
    end_turn_processes_on_abort: bool,
    /// Disable vendor session log uploads and confirm the setting before opening DSH.
    #[arg(long, value_enum)]
    session_log_upload: Option<kar::SessionLogUpload>,
    /// Lower Qwen's native background-memory default. Explicit settings take precedence.
    #[arg(long, value_enum)]
    native_memory_default: Option<kar::NativeMemoryDefault>,
    /// Keep the latest attributed root request available to this session's host.
    #[arg(long, value_enum)]
    model_requests: Option<kar::ModelRequests>,
    /// Exclude user settings, MCP servers or instructions. Without a value excludes all three.
    #[arg(long = "exclude-user-config", num_args = 0..=1, require_equals = true, value_delimiter = ',', default_missing_value = "settings,mcp,instructions")]
    exclude_user_configuration: Vec<kar::UserConfigurationCategory>,
    /// Replace the native base instructions.
    #[arg(long)]
    system_prompt: Option<String>,
    /// Append to the included native instructions.
    #[arg(long)]
    append_system_prompt: Option<String>,
    /// Exclude a native tool or rule. Repeatable.
    #[arg(long = "disallow-tool")]
    disallowed_tools: Vec<String>,
    /// Trust or exclude project-level Pi resources for this session.
    #[arg(long)]
    pi_project_trusted: Option<bool>,
    /// JSON Schema object for native final output.
    #[arg(long, value_parser = parse_output_schema)]
    output_schema: Option<serde_json::Map<String, serde_json::Value>>,
    /// Enable and confirm native session goals.
    #[arg(long)]
    goals: bool,
    /// Select a mode advertised by the native ACP session.
    #[arg(long)]
    acp_mode: Option<String>,
    /// Native reasoning effort, confirmed at startup.
    #[arg(long)]
    effort: Option<String>,
    /// Native service tier, confirmed at startup.
    #[arg(long)]
    service_tier: Option<String>,
    /// Name the session in the native runtime.
    #[arg(long)]
    name: Option<String>,
    /// Additional workspace directory, resolved relative to the session cwd. Repeatable.
    #[arg(long = "add-dir")]
    additional_directories: Vec<PathBuf>,
}
fn parse_env_name(name: &str) -> std::result::Result<String, String> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err("expected an environment variable name".into());
    }
    Ok(name.into())
}
impl NativeSettings {
    fn options(self, cwd: PathBuf) -> SessionOptions {
        let mut opts = SessionOptions::new(cwd);
        opts.launch_args = self.launch_args;
        opts.unset_env = self.unset_env.into_iter().collect();
        opts.settings = kar::SessionSettings {
            native_memory_default: self.native_memory_default,
            session_log_upload: self.session_log_upload,
            end_turn_processes_on_abort: self.end_turn_processes_on_abort,
            model_requests: self.model_requests,
            output_schema: self.output_schema,
            goals: self.goals,
            exclude_user_configuration: self.exclude_user_configuration.into_iter().collect(),
            system_prompt: self.system_prompt,
            append_system_prompt: self.append_system_prompt,
            disallowed_tools: self.disallowed_tools,
            pi_project_trusted: self.pi_project_trusted,
            name: self.name,
            effort: self.effort,
            acp_mode: self.acp_mode,
            service_tier: self.service_tier,
            additional_directories: self.additional_directories,
        };
        opts
    }
}
#[derive(Args)]
struct Open {
    runtime: String,
    #[command(flatten)]
    args: SessionArgs,
}
#[derive(Args)]
struct SessionArgs {
    #[command(flatten)]
    settings: NativeSettings,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    #[arg(long)]
    resume: Option<String>,
    /// Copy this native session into a new session.
    #[arg(long, conflicts_with = "resume")]
    fork: Option<String>,
    /// Copy through this provider's turn anchor, inclusive.
    #[arg(long, requires = "fork")]
    through: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    bin: Option<PathBuf>,
}
impl SessionArgs {
    fn options(self) -> SessionOptions {
        let mut opts = self.settings.options(self.cwd);
        opts.resume = self.resume;
        opts.model = self.model;
        opts
    }
}
impl Open {
    fn runtime(&mut self) -> Result<kar::Runtime> {
        let mut r = runtime(&self.runtime)?;
        if let Some(bin) = self.args.bin.take() {
            if r.protocol == kar::Protocol::Cybion {
                return Err(Error::Unsupported(
                    "--bin for a remote Cybion thread".into(),
                ));
            }
            r.program = bin;
        }
        Ok(r)
    }
    async fn session(mut self) -> Result<kar::Session> {
        let rt = self.runtime()?;
        let fork = self.args.fork.take();
        let through = self.args.through.take();
        let options = self.args.options();
        if let Some(source) = fork {
            rt.fork(&source, through.as_deref(), options).await
        } else {
            rt.open(options).await
        }
    }
    async fn catalog(mut self, kind: kar::Catalog) -> Result<()> {
        let value = if self.args.fork.is_some() {
            let session = self.session().await?;
            let result = session.catalog(kind).await;
            let closed = session.close().await;
            let value = result?;
            closed?;
            value
        } else {
            self.runtime()?.catalog(kind, self.args.options()).await?
        };
        json_line(&value, std::io::stdout())
    }
}
fn create_record(path: &Path) -> Result<std::fs::File> {
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    Ok(open.open(path)?)
}
fn json_line(value: &impl serde::Serialize, mut out: impl Write) -> Result<()> {
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}
async fn execute(command: Action) -> Result<()> {
    match command {
        Action::Chat(chat) => chat.run().await?,
        Action::Workbench { roots, state, port } => {
            let store = match state {
                Some(path) => kar::chat::ChatStore::open(path)?,
                None => kar::chat::ChatStore::local()?,
            };
            let app = kar::workbench::Workbench::open(store, &roots)?;
            let token = std::env::var("KAR_WORKBENCH_TOKEN")
                .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
            let router = app.router(token.clone())?;
            let listener =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
            let address = listener.local_addr()?;
            json_line(
                &json!({"url":format!("http://{address}"),"token":token}),
                std::io::stdout().lock(),
            )?;
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Action::Git(git) => git.run()?,
        Action::Mcp {
            cwd,
            runtimes: ids,
            max_sessions,
            max_depth,
            log_dir,
            max_running,
        } => {
            let mut selected = runtimes();
            selected.push(runtime("morph")?);
            if !ids.is_empty() {
                // The Kar service adapter may also be selected explicitly.
                selected.push(runtime("cybion")?);
                selected.retain(|r| ids.iter().any(|id| id.trim() == r.id));
            }
            let capacity = mcp::Limits::from_env(max_sessions, max_running, max_depth)?;
            mcp::serve(cwd, selected, capacity, log_dir).await?;
        }
        Action::Install {
            runtime: id,
            plan,
            line,
            timeout_ms,
        } => {
            let r = runtime(&id)?;
            if plan {
                json_line(&r.install_plan_for(line.as_deref())?, std::io::stdout())?;
            } else {
                json_line(
                    &r.install_with(line.as_deref(), Duration::from_millis(timeout_ms.get()))
                        .await?,
                    std::io::stdout(),
                )?;
            }
        }
        Action::Update {
            runtime: id,
            check,
            timeout_ms,
        } => {
            let timeout =
                Duration::from_millis(timeout_ms.map(|ms| ms.get()).unwrap_or(if check {
                    20_000
                } else {
                    600_000
                }));
            if !check && let Some(id) = &id {
                return json_line(&runtime(id)?.update_with(timeout).await?, std::io::stdout());
            }
            let providers = match id {
                Some(id) => vec![runtime(&id)?],
                None => runtimes()
                    .into_iter()
                    .chain([runtime("morph")?, runtime("cybion")?])
                    .collect(),
            };
            let mut failed = false;
            if check {
                let checks = providers.iter().map(|rt| rt.check_update(timeout));
                for (rt, result) in providers
                    .iter()
                    .zip(futures_util::future::join_all(checks).await)
                {
                    let row = match result {
                        Ok(check) => json!({"id":rt.id, "check":check}),
                        Err(error) => {
                            failed = true;
                            json!({"id":rt.id,"error":error.to_string()})
                        }
                    };
                    json_line(&row, std::io::stdout())?;
                }
            } else {
                for rt in providers {
                    let installation = rt.probe_installation(&Default::default()).await?;
                    let eligible = matches!(
                        &installation,
                        kar::InstallationSnapshot::Available {
                            command: Some(_),
                            ..
                        } | kar::InstallationSnapshot::Failed {
                            code: kar::InstallationIssue::VersionUnsupported,
                            ..
                        }
                    );
                    let row = if eligible {
                        match rt.update_with(timeout).await {
                            Ok(updated) => json!({"id":rt.id, "installation":updated}),
                            Err(Error::Unsupported(detail)) => {
                                json!({"id":rt.id, "unsupported":detail})
                            }
                            Err(error) => {
                                failed = true;
                                json!({"id":rt.id,"error":error.to_string()})
                            }
                        }
                    } else {
                        json!({"id":rt.id,"installation":installation})
                    };
                    json_line(&row, std::io::stdout())?;
                }
            }
            if failed {
                return Err(Error::Refused(
                    "one or more update operations failed".into(),
                ));
            }
        }
        Action::Orient {
            runtimes: ids,
            depth,
            cwd,
            customization,
            timeout_ms,
        } => {
            let registry: Vec<_> = runtimes()
                .into_iter()
                .chain([runtime("morph")?, runtime("cybion")?])
                .collect();
            let opts = kar::orientation::OrientOptions {
                installation: kar::InstallationOptions {
                    cwd,
                    ..Default::default()
                },
                runtimes: (!ids.is_empty()).then_some(ids),
                depth,
                customization,
                timeout: timeout_ms.map(|n| Duration::from_millis(n.get())),
                ..Default::default()
            };
            let result = kar::orientation::orient(&registry, &opts).await?;
            json_line(&result, std::io::stdout().lock())?;
        }
        Action::List {
            runtime: id,
            cwd,
            bin,
        } => {
            let selected = if let Some(id) = id {
                let mut rt = runtime(&id)?;
                if let Some(path) = bin {
                    rt.program = path;
                }
                vec![rt]
            } else {
                runtimes()
                    .into_iter()
                    .chain([runtime("morph")?, runtime("cybion")?])
                    .collect()
            };
            let mut probes = Vec::new();
            for rt in selected {
                let options = kar::InstallationOptions {
                    cwd: cwd.clone(),
                    ..Default::default()
                };
                probes.push(tokio::spawn(async move {
                    let installation = rt.probe_installation(&options).await?;
                    let mut row = json!({"id":rt.id,"protocol":rt.protocol,"program":installation.command().unwrap_or(&rt.program),"installation":installation});
                    if !rt.install_lines().is_empty() {
                        row["install_lines"] = json!(rt.install_lines());
                    }
                    if let Some(caps) = rt.session_capabilities(&installation) {
                        row["session_capabilities"] = serde_json::to_value(caps)?;
                    }
                    Ok::<_,Error>(row)
                }));
            }
            for probe in probes {
                json_line(
                    &probe.await.map_err(|e| Error::Protocol(e.to_string()))??,
                    std::io::stdout(),
                )?;
            }
        }
        Action::Account {
            runtime: id,
            action,
        } => {
            let Some(id) = id else {
                return account_cli::batch(action).await;
            };
            let action = match action {
                Account::Usage {
                    json: _,
                    bin,
                    cwd,
                    count_only,
                    timeout_ms,
                } => {
                    let mut rt = runtime(&id)?;
                    if let Some(bin) = bin {
                        rt.program = bin;
                    }
                    let opts = kar::account::AccountUsageOptions {
                        installation: kar::InstallationOptions {
                            cwd: Some(cwd),
                            ..Default::default()
                        },
                        timeout: timeout_ms.map(std::time::Duration::from_millis),
                        limit_reset_details: !count_only,
                    };
                    return json_line(&rt.account_usage(&opts).await?, std::io::stdout());
                }
                Account::InspectLimitResets {
                    include_five_hour,
                    five_hour_limit_consent,
                    bin,
                    cwd,
                    timeout_ms,
                } => {
                    let mut rt = runtime(&id)?;
                    if let Some(path) = bin {
                        rt.program = path;
                    }
                    let opts = kar::account::LimitResetInspectOptions {
                        include_five_hour,
                        five_hour_limit_consent,
                        installation: kar::InstallationOptions {
                            cwd: Some(cwd),
                            ..Default::default()
                        },
                        timeout: timeout_ms.map(std::time::Duration::from_millis),
                    };
                    return json_line(&rt.inspect_limit_resets(&opts).await?, std::io::stdout());
                }
                Account::RedeemLimitReset {
                    five_hour_limit_consent,
                    idempotency_key,
                    reset_id,
                    bin,
                    cwd,
                    timeout_ms,
                } => {
                    let mut rt = runtime(&id)?;
                    if let Some(path) = bin {
                        rt.program = path;
                    }
                    let mut opts = kar::account::LimitResetOptions::new(idempotency_key);
                    opts.five_hour_limit_consent = five_hour_limit_consent;
                    opts.reset_id = reset_id;
                    opts.installation.cwd = Some(cwd);
                    opts.timeout = timeout_ms.map(std::time::Duration::from_millis);
                    return json_line(
                        &rt.redeem_limit_reset_report(&opts).await?,
                        std::io::stdout(),
                    );
                }
                Account::Login { json, timeout_ms }
                    if json
                        || timeout_ms.is_some()
                        || matches!(id.as_str(), "factory" | "codebuddy") =>
                {
                    let rt = runtime(&id)?;
                    let options = kar::account::LoginOptions {
                        open_browser: json.then_some(false),
                        timeout: timeout_ms.map(|ms| Duration::from_millis(ms.get())),
                        ..Default::default()
                    };
                    let mut present = |event: kar::account::LoginEvent| {
                        if json {
                            json_line(&event, std::io::stdout())
                        } else {
                            Ok(event.present(std::io::stdout())?)
                        }
                    };
                    let result = rt.login(&options, &mut present);
                    tokio::pin!(result);
                    let result = tokio::select! {
                        answer=&mut result=>answer?,
                        signal=tokio::signal::ctrl_c()=>{signal?;options.cancellation.cancel();result.await?}
                    };
                    return if json {
                        json_line(&result, std::io::stdout())
                    } else {
                        result.into_result()
                    };
                }
                Account::Login { .. } => kar::AccountAction::Login,
                Account::Logout { json, timeout_ms } if json || timeout_ms.is_some() => {
                    let options = kar::account::LogoutOptions {
                        timeout: timeout_ms.map(|ms| Duration::from_millis(ms.get())),
                        ..Default::default()
                    };
                    let result = runtime(&id)?.logout(&options).await?;
                    return if json {
                        json_line(&result, std::io::stdout())
                    } else {
                        result.into_result()
                    };
                }
                Account::Logout { .. } => kar::AccountAction::Logout,
                Account::Status { json, timeout_ms } if json || timeout_ms.is_some() => {
                    let options = kar::account::AuthStatusOptions {
                        timeout: timeout_ms.map(|ms| Duration::from_millis(ms.get())),
                        ..Default::default()
                    };
                    let status = runtime(&id)?.auth_status(&options).await?;
                    return json_line(&status, std::io::stdout());
                }
                Account::Status { .. } => kar::AccountAction::Status,
            };
            runtime(&id)?.account(action).await?;
        }
        Action::Providers(args) => providers_cli::run(args).await?,
        Action::Credentials { runtime: id } => {
            let shape = kar::credentials::for_runtime(&id)
                .ok_or_else(|| Error::Unsupported(format!("credential declarations for {id}")))?;
            json_line(shape, std::io::stdout())?;
        }
        Action::Usage(args) => {
            let session = args.session().await?;
            json_line(&session.account_usage().await?, std::io::stdout())?;
            session.close().await?;
        }
        Action::Skills(args) => args.print(inventory_cli::Kind::Skills).await?,
        Action::Commands(args) => args.print(inventory_cli::Kind::Commands).await?,
        Action::McpServers(args) => args.print(inventory_cli::Kind::McpServers).await?,
        Action::Tools(args) => args.print(inventory_cli::Kind::Tools).await?,
        Action::Inventory { open, kind } => {
            let kind = match kind {
                Catalog::Skills => kar::Inventory::Skills,
                Catalog::Commands => kar::Inventory::Commands,
                Catalog::McpServers => kar::Inventory::McpServers,
            };
            open.catalog(kar::Catalog::Inventory(kind)).await?;
        }
        Action::Open(args) => {
            let s = args.session().await?;
            json_line(
                &json!({"id":s.id(),"stream":s.stream_id(),"capabilities":s.capabilities()}),
                std::io::stdout(),
            )?;
            s.close().await?;
        }
        Action::Models(args) => args.run().await?,
        Action::ConfigForm { runtime } => {
            let data = if let Some(id) = runtime {
                let form = kar::forms::config_form(&id)
                    .ok_or_else(|| Error::Unsupported(format!("no settings form for {id}")))?;
                serde_json::to_value(form)?
            } else {
                serde_json::to_value(kar::forms::config_forms())?
            };
            json_line(&data, std::io::stdout())?;
        }
        Action::Run {
            open,
            prompt,
            images,
            json,
            record,
            timeout,
        } => {
            let mut file = record.as_deref().map(create_record).transpose()?;
            let session = open.session().await?;
            eprintln!("session {} (stream {})", session.id(), session.stream_id());
            let mut records = session.subscribe(None)?;
            let output = tokio::spawn(async move {
                let mut streamed = false;
                while let Some(r) = records.next().await {
                    if let Some(f) = &mut file {
                        json_line(&*r, f)?;
                    }
                    let mut out = std::io::stdout().lock();
                    if json {
                        json_line(&*r, &mut out)?;
                    } else {
                        for e in &r.events {
                            match e {
                                Event::TextDelta { text } => {
                                    write!(out, "{text}")?;
                                    streamed = true;
                                }
                                Event::Message { text } if !streamed => write!(out, "{text}")?,
                                Event::Message { .. } => {
                                    streamed = false;
                                }
                                _ => {}
                            }
                        }
                        out.flush()?;
                    }
                }
                Result::Ok(())
            });
            let outcome = match session.prompt_image_paths(prompt, &images).await {
                Err(error) => Err(error),
                Ok(turn) => tokio::select! {
                    result = turn.wait() => result,
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        let _ = tokio::time::timeout(Duration::from_secs(1), session.abort()).await;
                        Ok(Outcome::Aborted)
                    },
                    _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                        let _ = tokio::time::timeout(Duration::from_secs(1), session.abort()).await;
                        Err(Error::Timeout("turn".into()))
                    },
                },
            };
            let close = session.close().await;
            output.await.map_err(|e| Error::Protocol(e.to_string()))??;
            if !json {
                writeln!(std::io::stdout())?;
            }
            match outcome? {
                Outcome::Completed => close?,
                other => return Err(Error::Refused(format!("turn ended: {other:?}"))),
            }
        }
        Action::Import { input, output } => {
            let parent = output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
            let reader = std::io::BufReader::new(std::fs::File::open(input)?);
            let summary = kar::voyage::import(reader, |r| json_line(&r, &mut tmp))?;
            tmp.as_file().sync_all()?;
            tmp.persist_noclobber(output).map_err(|e| e.error)?;
            json_line(&summary, std::io::stdout())?;
        }
        Action::Replay {
            path,
            after,
            stream,
        } => {
            let cursor = after.zip(stream).map(|(after_seq, stream_id)| Cursor {
                stream_id,
                after_seq,
            });
            replay(&path, cursor.as_ref(), std::io::stdout().lock())?;
        }
    }
    Ok(())
}
fn replay(path: &Path, cursor: Option<&Cursor>, mut out: impl Write) -> Result<()> {
    let mut stream = None;
    let mut seen = cursor.is_none();
    for (seq, line) in std::io::BufReader::new(std::fs::File::open(path)?)
        .lines()
        .enumerate()
    {
        let r: Record = serde_json::from_str(&line?)?;
        if r.seq != seq as u64 || stream.is_some_and(|s| s != r.stream_id) {
            return Err(Error::Protocol(format!(
                "discontinuous recording at line {}",
                seq + 1
            )));
        }
        stream = Some(r.stream_id);
        if let Some(c) = cursor {
            if c.stream_id != r.stream_id {
                return Err(Error::InvalidCursor);
            }
            if r.seq == c.after_seq {
                seen = true;
            }
            if r.seq <= c.after_seq {
                continue;
            }
        }
        json_line(&r, &mut out)?;
    }
    if !seen {
        return Err(Error::InvalidCursor);
    }
    Ok(())
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let action = Cli::parse().command;
    let json = match &action {
        Action::Chat(chat) => chat.json,
        Action::Run { json, .. } => *json,
        _ => false,
    };
    match execute(action).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(Error::Open(e)) if json => {
            eprintln!("{}", json!({"error":e}));
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            std::process::ExitCode::FAILURE
        }
    }
}
