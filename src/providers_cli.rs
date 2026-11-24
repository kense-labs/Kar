use clap::{Args, Subcommand};
use kar::{
    Error, Result,
    account::LoginCancellation,
    providers::{
        Method, PiModelRuntimePaths, PiModelsPath, Prompt, ProviderAuthOptions, ProviderEvent,
        ProviderInteraction, QwenConnect, QwenProtocol, QwenScope, QwenWireApi,
    },
};
use serde_json::json;
use std::io::{self, BufRead, IsTerminal, Read, Write};

#[derive(Args)]
pub struct Providers {
    runtime: String,
    /// Working directory used by the native configuration reader.
    #[arg(long)]
    cwd: Option<std::path::PathBuf>,
    /// Pi credential file. Relative paths use --cwd.
    #[arg(long)]
    auth_path: Option<std::path::PathBuf>,
    /// Pi static models file. Relative paths use --cwd.
    #[arg(long, conflicts_with = "no_models")]
    models_path: Option<std::path::PathBuf>,
    /// Disable Pi's static models configuration.
    #[arg(long)]
    no_models: bool,
    /// Refuse a remote sign-in that needs a browser on the OpenCode host.
    #[arg(long)]
    refuse_loopback: bool,
    #[command(subcommand)]
    command: Operation,
}
#[derive(Subcommand)]
enum Operation {
    /// Configure a Qwen provider. Omit --key-stdin to reuse its stored key.
    Connect {
        provider: String,
        #[arg(long)]
        key_stdin: bool,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long, value_enum)]
        protocol: Option<QwenProtocol>,
        #[arg(long, value_enum)]
        wire_api: Option<QwenWireApi>,
        #[arg(long = "model")]
        models: Vec<String>,
        #[arg(long, value_enum)]
        scope: Option<QwenScope>,
    },
    /// List supported providers and their offered login methods in native order.
    Menu,
    /// Read Pi's native model catalog. Use --refresh to reload it.
    Catalog {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        refresh: bool,
        /// Reload configuration and cached models without requesting remote catalogs.
        #[arg(long, requires = "refresh")]
        offline: bool,
    },
    /// List only providers with stored credentials.
    List {
        /// Resolve each stored credential through Pi's native check.
        #[arg(long)]
        resolve: bool,
    },
    /// Inspect a provider, including an environment-only connection.
    Status { provider: String },
    /// Read a literal API key from a pipe. One trailing line ending is removed.
    SetKey { provider: String },
    /// Run a sign-in. Events and prompts are JSON lines; answer prompts with a JSON string on stdin.
    Login {
        provider: String,
        #[arg(value_enum)]
        method: Method,
        /// Host name sent to Pi's OAuth provider.
        #[arg(long)]
        agent_name: Option<String>,
        /// Stable UUID of the installation performing Pi sign-in.
        #[arg(long)]
        device_id: Option<uuid::Uuid>,
    },
    /// Delete this provider's stored credentials.
    Logout { provider: String },
}
#[derive(Default)]
struct Interaction {
    answers: Option<tokio::sync::mpsc::Receiver<Result<String>>>,
}
impl ProviderInteraction for Interaction {
    fn concurrent_events(&self) -> Option<kar::providers::ProviderEventSink> {
        Some(Box::new(|event| line(&json!({"event":event}))))
    }
    fn event(&mut self, event: ProviderEvent) -> Result<()> {
        line(&json!({"event":event}))
    }
    async fn prompt(&mut self, prompt: Prompt) -> Result<String> {
        if io::stdin().is_terminal()
            && matches!(prompt, Prompt::Secret { .. } | Prompt::ManualCode { .. })
        {
            return Err(Error::Refused(
                "Supply private prompt answers through a pipe".into(),
            ));
        }
        line(&json!({"prompt":prompt}))?;
        let answers = self.answers.get_or_insert_with(|| {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            // Keep one reader for the whole login. Dismissing a pending
            // browser prompt leaves the next line available for the next prompt.
            std::thread::spawn(move || {
                let mut input = io::stdin().lock();
                loop {
                    let answer = read_answer(&mut input);
                    let failed = answer.is_err();
                    if tx.blocking_send(answer).is_err() || failed {
                        break;
                    }
                }
            });
            rx
        });
        answers
            .recv()
            .await
            .ok_or_else(|| Error::Closed("Provider input closed".into()))?
    }
}
fn read_answer(input: &mut impl BufRead) -> Result<String> {
    let mut bytes = vec![];
    let n = input.take(1024 * 1024 + 1).read_until(b'\n', &mut bytes)?;
    if n == 0 || n > 1024 * 1024 {
        return Err(Error::Refused(
            "Missing or oversized provider answer".into(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Protocol("Expected a JSON string for the provider prompt".into()))
}
fn line(v: &impl serde::Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, v)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}
fn key() -> Result<String> {
    if io::stdin().is_terminal() {
        return Err(Error::Refused("Pipe the key to set-key on stdin".into()));
    }
    let mut input = String::new();
    let n = io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut input)?;
    if input.ends_with('\n') {
        input.pop();
        if input.ends_with('\r') {
            input.pop();
        }
    }
    if input.is_empty() || n > 1024 * 1024 {
        return Err(Error::Refused(
            "Missing or oversized provider API key".into(),
        ));
    }
    Ok(input)
}
async fn finish_login(
    cancel: &LoginCancellation,
    task: impl Future<Output = Result<kar::providers::ProviderStatus>>,
) -> Result<()> {
    tokio::pin!(task);
    let status = tokio::select! {
        result=&mut task=>result?,
        signal=tokio::signal::ctrl_c()=>{signal?;cancel.cancel();task.await?}
    };
    line(&json!({"status":status}))
}
pub async fn run(args: Providers) -> Result<()> {
    let rt = kar::runtime(&args.runtime)?;
    let paths = PiModelRuntimePaths {
        auth_path: args.auth_path,
        models_path: match args.models_path {
            Some(path) => PiModelsPath::File(path),
            None if args.no_models => PiModelsPath::Disabled,
            None => PiModelsPath::AgentDefault,
        },
    };
    if rt.id != "pi" && paths != PiModelRuntimePaths::default() {
        return Err(Error::Unsupported(
            "Independent models and credential files are Pi options".into(),
        ));
    }
    let opt = ProviderAuthOptions {
        installation: kar::InstallationOptions {
            cwd: args.cwd,
            ..Default::default()
        },
        refuse_loopback: args.refuse_loopback,
        ..Default::default()
    };
    if rt.id == "pi" {
        let auth = rt.pi_provider_auth_with_paths(opt, paths).await?;
        return match args.command {
            Operation::Menu => line(&auth.login_providers().await?),
            Operation::Catalog {
                provider,
                refresh,
                offline,
            } => {
                let mut catalog = auth.model_catalog().await?;
                let mut out = json!({});
                if refresh {
                    let cancel = kar::providers::CatalogCancellation::default();
                    let options = kar::providers::CatalogRefreshOptions {
                        providers: provider.as_ref().map(|p| vec![p.clone()]),
                        allow_network: !offline,
                    };
                    let pending = catalog.refresh(options, &cancel);
                    tokio::pin!(pending);
                    out["refresh"] = serde_json::to_value(tokio::select! {
                        result = &mut pending => result?,
                        signal = tokio::signal::ctrl_c() => { signal?; cancel.cancel(); pending.await? }
                    })?;
                }
                out["providers"] = serde_json::to_value(
                    catalog
                        .providers()
                        .iter()
                        .filter(|p| provider.as_ref().is_none_or(|id| *id == p.id))
                        .collect::<Vec<_>>(),
                )?;
                out["models"] =
                    serde_json::to_value(catalog.models(provider.as_deref()).collect::<Vec<_>>())?;
                if let Some(id) = provider {
                    out["defaultModel"] = json!(catalog.default_model(&id));
                }
                line(&out)
            }
            Operation::SetKey { provider } => {
                auth.set_api_key(&provider, &key()?).await?;
                line(&json!({"stored":true}))
            }
            Operation::Login {
                provider,
                method,
                agent_name,
                device_id,
            } => {
                let get_id =
                    || device_id.ok_or_else(|| Error::Protocol("Missing host device UUID".into()));
                let identity = kar::providers::PiLoginOptions {
                    agent_name: agent_name.as_deref(),
                    get_device_id: device_id.as_ref().map(|_| &get_id as _),
                };
                let cancel = LoginCancellation::default();
                finish_login(
                    &cancel,
                    auth.login_with_options(
                        &provider,
                        method,
                        &mut Interaction::default(),
                        &cancel,
                        &identity,
                    ),
                )
                .await
            }
            Operation::Status { provider } => line(&auth.status(&provider).await?),
            Operation::List { resolve } => line(&auth.list_providers(resolve).await?),
            Operation::Logout { provider } => {
                auth.logout(&provider).await?;
                line(&json!({"removed":true}))
            }
            Operation::Connect { .. } => Err(Error::Unsupported(
                "This Pi provider operation is unavailable".into(),
            )),
        };
    }
    if rt.id == "qwen" {
        let auth = rt.qwen_provider_auth(opt).await?;
        return match args.command {
            Operation::Menu => line(&auth.list_providers().await?),
            Operation::Connect {
                provider,
                key_stdin,
                base_url,
                protocol,
                wire_api,
                models,
                scope,
            } => {
                let input = QwenConnect {
                    provider_id: provider,
                    api_key: key_stdin.then(key).transpose()?,
                    base_url,
                    protocol,
                    wire_api,
                    scope,
                    model_ids: (!models.is_empty()).then_some(models),
                };
                line(&auth.connect(&input).await?)
            }
            _ => Err(Error::Unsupported(
                "Qwen provides a settings menu and connect operation".into(),
            )),
        };
    }
    let auth = rt.opencode_provider_auth(opt).await?;
    match args.command {
        Operation::Catalog { .. } => {
            return Err(Error::Unsupported("This model catalog requires Pi".into()));
        }
        Operation::Connect { .. } => {
            return Err(Error::Unsupported("connect requires Qwen Code".into()));
        }
        Operation::Menu => line(&auth.login_providers())?,
        Operation::List { .. } => line(&auth.list_providers().await?)?,
        Operation::Status { provider } => line(&auth.status(&provider).await?)?,
        Operation::SetKey { provider } => {
            auth.set_api_key(&provider, &key()?).await?;
            line(&json!({"stored":true}))?;
        }
        Operation::Logout { provider } => {
            auth.logout(&provider).await?;
            line(&json!({"removed":true}))?;
        }
        Operation::Login {
            provider,
            method,
            agent_name,
            device_id,
        } => {
            if agent_name.is_some() || device_id.is_some() {
                return Err(Error::Unsupported(
                    "Host login identity on this adapter".into(),
                ));
            }
            let cancel = LoginCancellation::default();
            finish_login(
                &cancel,
                auth.login(&provider, method, &mut Interaction::default(), &cancel),
            )
            .await?;
        }
    }
    auth.close().await;
    Ok(())
}
