use crate::{Error, Protocol, Result, Runtime};
use serde::Deserialize;
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct SideQuestionOptions {
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub unset_env: std::collections::BTreeSet<String>,
    pub model: Option<String>,
    pub timeout: Duration,
    pub signal: tokio_util::sync::CancellationToken,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub exclude_user_configuration: std::collections::BTreeSet<crate::UserConfigurationCategory>,
}
impl SideQuestionOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            env: BTreeMap::new(),
            unset_env: Default::default(),
            model: None,
            timeout: Duration::from_secs(60),
            signal: Default::default(),
            system_prompt: None,
            append_system_prompt: None,
            exclude_user_configuration: Default::default(),
        }
    }
}
#[derive(Deserialize)]
struct Reply {
    #[serde(rename = "type")]
    kind: String,
    subtype: String,
    is_error: bool,
    session_id: Uuid,
    result: String,
}
impl Runtime {
    /// Read one answer from an ephemeral native fork, with tool execution disabled.
    pub async fn side_question(
        &self,
        source: &str,
        question: &str,
        opts: &SideQuestionOptions,
    ) -> Result<String> {
        if self.protocol != Protocol::Claude {
            return Err(Error::Unsupported(
                "native side questions require Claude".into(),
            ));
        }
        let source = Uuid::parse_str(source)
            .map_err(|_| Error::Protocol("expected a Claude session UUID".into()))?;
        if question.is_empty() || opts.timeout.is_zero() {
            return Err(Error::Protocol(
                "side questions require text and a positive timeout".into(),
            ));
        }
        tokio::select! {
            biased;
            () = opts.signal.cancelled() => Err(Error::Refused("side question cancelled".into())),
            answer = self.side_answer(source, question, opts) => answer,
        }
    }

    async fn side_answer(
        &self,
        source: Uuid,
        question: &str,
        opts: &SideQuestionOptions,
    ) -> Result<String> {
        let cwd = opts.cwd.canonicalize()?;
        let mut launch = crate::SessionOptions::new(&cwd);
        launch.env.clone_from(&opts.env);
        launch.unset_env.clone_from(&opts.unset_env);
        let selected = self.selected(&launch).await?;
        let version = selected.launch_version(&launch).await?;
        if !version
            .split_whitespace()
            .filter_map(|v| semver::Version::parse(v.trim_start_matches('v')).ok())
            .any(|v| v >= semver::Version::new(2, 1, 326))
        {
            return Err(Error::Unsupported(
                "native side questions require Claude 2.1.326 or later".into(),
            ));
        }
        let fork = Uuid::new_v4();
        let mut command = launch.command(&selected.program)?;
        command.args(&self.args);
        crate::options::claude_environment(&mut command, &opts.env);
        launch
            .settings
            .system_prompt
            .clone_from(&opts.system_prompt);
        launch
            .settings
            .append_system_prompt
            .clone_from(&opts.append_system_prompt);
        launch
            .settings
            .exclude_user_configuration
            .clone_from(&opts.exclude_user_configuration);
        // Side questions already use a strict, empty MCP configuration.
        launch
            .settings
            .exclude_user_configuration
            .remove(&crate::UserConfigurationCategory::Mcp);
        let _prompts = crate::options::instructions::launch(&mut command, self, &launch, &cwd)?;
        crate::options::user_configuration::claude(&mut command, &launch, &cwd, true, None)?;
        command
            .current_dir(&cwd)
            .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
            .args(["--print", "--output-format", "json", "--resume"])
            .arg(source.to_string())
            .args(["--fork-session", "--session-id"])
            .arg(fork.to_string())
            .args([
                "--no-session-persistence",
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--disable-slash-commands",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(model) = &opts.model {
            command.arg("--model").arg(model);
        }
        let output = tokio::time::timeout(opts.timeout, async {
            let mut child = command.spawn()?;
            let mut stdin = child.stdin.take().expect("piped stdin");
            let send = async move {
                stdin.write_all(question.as_bytes()).await?;
                stdin.shutdown().await
            };
            let (_, output) = tokio::try_join!(send, child.wait_with_output())?;
            Ok::<_, std::io::Error>(output)
        })
        .await
        .map_err(|_| Error::Timeout("Claude side question".into()))??;
        if !output.status.success() {
            return Err(Error::Refused(format!(
                "Claude side question returned {}",
                output.status
            )));
        }
        let reply: Reply = serde_json::from_slice(&output.stdout)?;
        if reply.kind != "result"
            || reply.subtype != "success"
            || reply.is_error
            || reply.session_id != fork
        {
            return Err(Error::Protocol(
                "expected a successful answer from the disposable Claude fork".into(),
            ));
        }
        let mut answer = reply.result;
        let mut secrets = crate::options::environment_secrets(&opts.env);
        secrets.sort_by_key(|v| std::cmp::Reverse(v.len()));
        for secret in secrets {
            answer = answer.replace(&secret, "[redacted]");
        }
        Ok(answer)
    }
}
