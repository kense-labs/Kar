use crate::{Error, Result, Session};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

/// Native protocol spoken by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Codex,
    Claude,
    Qwen,
    Qoder,
    KimiKap,
    Factory,
    Dsh,
    Muse,
    Acp,
    Pi,
    Cybion,
    Morph,
}

impl Protocol {
    /// Reason the adapter cannot carry caller-provided environment variables.
    pub fn child_environment_refusal(self) -> Option<&'static str> {
        match self {
            Self::Morph => Some("Morph's shared Console owns its environment and tools"),
            Self::Cybion => Some("Cybion workers own their environment, tools and approvals"),
            _ => None,
        }
    }
    pub(crate) fn supports_output_schema(self, runtime: &str) -> bool {
        matches!(self, Self::Codex | Self::Claude) || (self == Self::Acp && runtime == "grok")
    }
}

#[derive(Clone, Debug)]
pub struct Runtime {
    pub id: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub protocol: Protocol,
    pub placement: crate::PlacementRequest,
}

/// How an already open model session reacts to an MCP tools/list_changed notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolsListChanged {
    Full,
    Additions,
    None,
}

impl Runtime {
    pub fn brand(&self) -> crate::brands::RuntimeBrand {
        crate::brands::runtime_brands()
            .get(&self.id)
            .cloned()
            .unwrap_or_else(|| crate::brands::RuntimeBrand {
                name: self.id.clone(),
                icon: None,
                icons: None,
            })
    }

    /// Numeric minimum for an adapter with a single declared version floor.
    pub fn min_version(&self) -> Option<&'static str> {
        match self.id.as_str() {
            "qoder" => Some("1.1.78"),
            "pi" => Some("1.4.0"),
            "antigravity" => Some("1.5.0"),
            "dsh" => Some("0.2.1-rc.5"),
            _ => None,
        }
    }

    /// Returns the runtime's declared refresh behavior, when known.
    pub fn mcp_tools_list_changed(&self) -> Option<McpToolsListChanged> {
        use McpToolsListChanged::*;
        match self.id.as_str() {
            "opencode" | "pi" => Some(Full),
            "claude" => Some(Additions),
            "codex" | "kimi" | "grok" | "antigravity" => Some(None),
            _ => Option::None,
        }
    }
    pub fn new(id: &str, program: impl Into<PathBuf>, args: &[&str], protocol: Protocol) -> Self {
        Self {
            id: id.into(),
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            protocol,
            placement: Default::default(),
        }
    }
    /// The executable adapters, including Pi's RPC/SDK subprocesses, each own
    /// one process per Kar session. A remote API and an attached Morph Console
    /// do not expose a local per-session process placement.
    pub fn execution(&self) -> Option<crate::Execution> {
        match self.protocol {
            Protocol::Cybion | Protocol::Morph => None,
            _ => Some(crate::Execution::Process),
        }
    }

    pub fn with_placement(&self, placement: crate::PlacementRequest) -> Self {
        Self {
            placement,
            ..self.clone()
        }
    }

    fn validate_placement(&self) -> Result<()> {
        if self.placement == crate::PlacementRequest::Shared && self.protocol == Protocol::Acp {
            let reason = match self.id.as_str() {
                "opencode" => Some(
                    "configured plugins can start children in the shared process HOME instead of the session directory",
                ),
                "grok" => Some(
                    "configured stdio MCP servers can start in the shared process HOME instead of the session directory",
                ),
                _ => None,
            };
            if let Some(reason) = reason {
                return Err(Error::Unsupported(format!(
                    "placement: {} requires an isolated native process because {reason}",
                    self.id
                )));
            }
        }
        self.placement.resolve(self.execution()).map(|_| ())
    }

    pub(crate) fn session_journal(
        &self,
        retained_bytes: Option<usize>,
    ) -> Result<crate::stream::Journal> {
        crate::stream::Journal::for_open(retained_bytes, self.placement, self.execution())
    }

    pub async fn open(&self, options: SessionOptions) -> Result<Session> {
        self.validate_placement()?;
        let (rt, version) = self.selected_with_version(&options).await?;
        rt.open_selected_with_version(options, version.as_deref())
            .await
    }
    pub(crate) async fn open_selected(&self, options: SessionOptions) -> Result<Session> {
        self.open_selected_with_version(options, None).await
    }
    pub(crate) async fn open_selected_with_version(
        &self,
        options: SessionOptions,
        version: Option<&str>,
    ) -> Result<Session> {
        self.validate_placement()?;
        crate::options::launch_args::validate(self, &options, false)?;
        if self.protocol == Protocol::Dsh {
            let release = crate::dsh::release(self, &options, version).await?;
            let version = release.to_string();
            if crate::dsh::uses_web(&release) {
                return crate::dsh::open(self, options, None, None, Some(&version)).await;
            }
            let mut acp = self.clone();
            acp.protocol = Protocol::Acp;
            acp.args.splice(0..0, ["--profile".into(), "acp".into()]);
            return Session::open_with_replay(&acp, options, None, None, false, Some(&version))
                .await;
        }
        if self.protocol == Protocol::KimiKap {
            return crate::kimi::open(self, options, true, &[]).await;
        }
        if self.protocol == Protocol::Morph {
            return crate::community::Morph::from_env(&self.program)?
                .open(options)
                .await;
        }
        if self.protocol == Protocol::Cybion {
            return crate::community::Cybion::from_env()?.open(options).await;
        }
        Session::open_with_replay(self, options, None, None, false, version).await
    }
    /// Open a new native session with the source history, optionally through a provider-issued anchor.
    pub async fn fork(
        &self,
        source: &str,
        through: Option<&str>,
        options: SessionOptions,
    ) -> Result<Session> {
        self.fork_with_id(source, through, options, None).await
    }
    pub(crate) async fn fork_with_id(
        &self,
        source: &str,
        through: Option<&str>,
        options: SessionOptions,
        child: Option<uuid::Uuid>,
    ) -> Result<Session> {
        self.validate_placement()?;
        self.selected(&options)
            .await?
            .fork_selected(source, through, options, child)
            .await
    }
    pub(crate) async fn fork_selected(
        &self,
        source: &str,
        through: Option<&str>,
        options: SessionOptions,
        child: Option<uuid::Uuid>,
    ) -> Result<Session> {
        self.fork_selected_version(source, through, options, child, None)
            .await
    }
    pub(crate) async fn fork_selected_version(
        &self,
        source: &str,
        through: Option<&str>,
        options: SessionOptions,
        child: Option<uuid::Uuid>,
        version: Option<&str>,
    ) -> Result<Session> {
        self.validate_placement()?;
        crate::options::launch_args::validate(self, &options, true)?;
        if source.trim().is_empty()
            || through.is_some_and(|s| s.trim().is_empty())
            || options.resume.is_some()
        {
            return Err(Error::Protocol(
                "fork needs a source, a nonempty optional anchor, and no separate resume id".into(),
            ));
        }
        match self.protocol {
            Protocol::Pi => return crate::pi_fork::fork(self, options, source, through).await,
            Protocol::Dsh => {
                return crate::dsh::open(self, options, Some((source, through)), None, None).await;
            }
            Protocol::Codex | Protocol::Claude if through.is_some() => {
                let detected;
                let report = if let Some(report) = version {
                    report
                } else {
                    detected = self.launch_version(&options).await?;
                    &detected
                };
                self.check_turn_fork_version(report, &options)?;
            }
            Protocol::Codex | Protocol::Claude => {}
            Protocol::Acp if self.id == "opencode" => {
                if let Some(id) = through {
                    let target = crate::providers::opencode_fork::Target::Message(id);
                    return crate::providers::opencode_fork::open(self, options, source, target)
                        .await
                        .map(|(session, _)| session);
                }
            }
            Protocol::Acp if through.is_none() => {}
            _ => {
                return Err(Error::Unsupported(
                    "native session fork with the requested scope".into(),
                ));
            }
        }
        Session::open(self, options, Some((source, through)), child).await
    }
    pub(crate) fn check_turn_fork_version(
        &self,
        report: &str,
        options: &SessionOptions,
    ) -> Result<()> {
        let version = report
            .split_whitespace()
            .find_map(|s| semver::Version::parse(s.trim_start_matches('v')).ok());
        let min = if self.protocol == Protocol::Codex {
            semver::Version::new(0, 143, 0)
        } else {
            semver::Version::new(2, 1, 323)
        };
        if version.is_none_or(|v| v < min) {
            let message = format!("forking through a turn requires {} {min} or later", self.id);
            if self.protocol == Protocol::Codex {
                return Err(crate::OpenError::RuntimeFailure {
                    failure: crate::TurnFailure::new(crate::FailureClass::InvalidRequest, message),
                    code: Some(crate::RuntimeFailureCode::RuntimeOutdated),
                    cause: Some(crate::NativeCause {
                        method: "--version".into(),
                        native: serde_json::json!({"version":report.trim()}),
                    }),
                }
                .redacted(&crate::options::session_secrets(options)));
            }
            return Err(Error::Unsupported(message));
        }
        Ok(())
    }
    /// Read the version from the selected local installation.
    pub async fn version(&self) -> Result<String> {
        let options = SessionOptions::new(std::env::current_dir()?);
        self.selected(&options)
            .await?
            .launch_version(&options)
            .await
    }
    pub(crate) async fn launch_version(&self, options: &SessionOptions) -> Result<String> {
        if self.protocol == Protocol::Cybion {
            return Ok(format!("bundled adapter {}", env!("CARGO_PKG_VERSION")));
        }
        let output = tokio::time::timeout(
            options.handshake_timeout,
            options
                .command(&self.program)?
                .current_dir(&options.cwd)
                .arg(if self.protocol == Protocol::Morph {
                    "version"
                } else {
                    "--version"
                })
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| Error::Timeout("version".into()))??;
        if !output.status.success() {
            return Err(Error::Refused(format!(
                "version probe returned {}",
                output.status
            )));
        }
        let output = if output.stdout.is_empty() {
            output.stderr
        } else {
            output.stdout
        };
        Ok(String::from_utf8_lossy(&output).trim().to_owned())
    }
}

type RuntimeSpec = (
    &'static str,
    &'static str,
    &'static [&'static str],
    Protocol,
);
fn builtins() -> &'static [RuntimeSpec] {
    use Protocol::*;
    &[
        ("codex", "codex", &["app-server"], Codex),
        ("claude", "claude", &[], Claude),
        ("opencode", "opencode", &["acp"], Acp),
        ("grok", "grok", &["agent", "--no-leader", "stdio"], Acp),
        ("kimi", "kimi", &[], KimiKap),
        ("devin", "devin", &["acp"], Acp),
        ("kiro", "kiro-cli", &["acp", "--agent-engine=v3", "--auth-method=cli"], Acp),
        ("fx", "fx", &["acp"], Acp),
        ("muse", "muse", &["serve", "--trust-workspace"], Muse),
        ("codebuddy", "codebuddy", &["--acp"], Acp),
        ("antigravity", "agy", &["acp", "--process-per-session"], Acp),
        ("qwen", "qwen", &[], Qwen),
        ("cursor", "agent", &["acp"], Acp),
        ("qoder", "qodercli", &[], Qoder),
        ("dsh", "dsh", &[], Dsh),
        ("factory", "droid", crate::factory::EXEC_ARGS, Factory),
        ("pi", "pi", &["--mode", "rpc"], Pi),
    ]
}
pub(crate) fn registered_command(id: &str) -> Option<&'static str> {
    if id == "morph" {
        return Some("morph");
    }
    builtins().iter().find(|r| r.0 == id).map(|r| r.1)
}
pub fn runtimes() -> Vec<Runtime> {
    builtins()
        .iter()
        .map(|(id, bin, args, protocol)| {
            let key = format!("KAR_{}_BIN", id.to_ascii_uppercase());
            Runtime::new(
                id,
                std::env::var_os(key)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| bin.into()),
                args,
                *protocol,
            )
        })
        .collect()
}
pub fn runtime(id: &str) -> Result<Runtime> {
    if id == "morph" {
        return Ok(Runtime::new(
            id,
            std::env::var_os("KAR_MORPH_BIN").unwrap_or_else(|| "morph".into()),
            &[],
            Protocol::Morph,
        ));
    }
    if id == "cybion" {
        return Ok(Runtime::new(id, "", &[], Protocol::Cybion));
    }
    runtimes()
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| Error::Unsupported(format!("unknown runtime {id}")))
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Respond with a native denial when the agent asks for a decision.
    #[default]
    Deny,
    /// Publish requests for the application to answer with Session::respond.
    Host,
}

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-operation deadlines for an ACP connection. Unset fields use the runtime's
/// profile or a non-default legacy `SessionOptions::handshake_timeout`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AcpTimeouts {
    /// Other requests, including authentication, settings and model selection.
    pub request: Option<Duration>,
    /// New sessions, lightweight resume and native forks. Kimi defaults to 630 seconds.
    pub session_start: Option<Duration>,
    /// History replay and its readiness handshake. Defaults to at least 60 seconds.
    pub session_load: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    pub resume: Option<String>,
    /// Exact session file for Pi imports. Its native ID and workspace must match on every reopen.
    pub pi_resume_file: Option<PathBuf>,
    /// Configured instance root containing Pi’s per-workspace session directories.
    pub pi_session_root: Option<PathBuf>,
    pub model: Option<String>,
    /// Additional native CLI arguments for this launch. Supply them again on
    /// resume; Kar keeps them out of saved chat configuration and recordings.
    pub launch_args: Vec<String>,
    pub settings: crate::SessionSettings,
    /// Optional volatile destination for next prompts from this controller's root user turns.
    pub prompt_suggestions: Option<crate::PromptSuggestionSource>,
    /// Volatile registration supplied by the workbench that owns this controller.
    #[doc(hidden)]
    pub workbench_registration: Option<crate::WorkbenchRegistration>,
    #[doc(hidden)]
    pub account_admission: Option<crate::account::AccountAdmission>,
    #[doc(hidden)]
    pub account_instance: Option<String>,
    pub mcp_servers: BTreeMap<String, crate::McpServer>,
    pub env: BTreeMap<OsString, OsString>,
    /// Names removed from the inherited environment. Explicit `env` values take precedence.
    pub unset_env: std::collections::BTreeSet<String>,
    pub approvals: ApprovalMode,
    /// Positive byte budget for retained native and tool payloads. Live subscribers and the newest frame stay complete.
    pub retained_bytes: Option<usize>,
    pub handshake_timeout: Duration,
    pub acp_timeouts: AcpTimeouts,
    /// Preferred advertised authentication method for a dynamic ACP launcher.
    pub acp_auth_method: Option<String>,
}
impl SessionOptions {
    pub(crate) fn acp_timeout(&self, runtime: &str, method: &str) -> Duration {
        let legacy =
            (self.handshake_timeout != DEFAULT_HANDSHAKE_TIMEOUT).then_some(self.handshake_timeout);
        let settings = self.acp_timeouts;
        let request = settings.request.or(legacy).unwrap_or_else(|| {
            Duration::from_secs(match runtime {
                "antigravity" | "codebuddy" | "devin" | "grok" | "kimi" | "opencode" => 30,
                _ => 15,
            })
        });
        match method {
            "session/load" => settings
                .session_load
                .or(legacy)
                .unwrap_or(request.max(Duration::from_secs(60))),
            "session/new" | "session/resume" | "session/fork" => settings
                .session_start
                .or(legacy)
                .unwrap_or(if runtime == "kimi" {
                    Duration::from_secs(630)
                } else {
                    request
                }),
            _ => request,
        }
    }
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            resume: None,
            pi_resume_file: None,
            pi_session_root: None,
            model: None,
            launch_args: Vec::new(),
            settings: Default::default(),
            prompt_suggestions: None,
            workbench_registration: None,
            account_admission: None,
            account_instance: None,
            mcp_servers: BTreeMap::new(),
            env: BTreeMap::new(),
            unset_env: Default::default(),
            approvals: ApprovalMode::Deny,
            retained_bytes: None,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            acp_timeouts: AcpTimeouts::default(),
            acp_auth_method: None,
        }
    }
}
