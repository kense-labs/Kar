use crate::{Error, Result, private_dir::PrivateDir};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

pub(crate) fn agent_controls(settings: &crate::SessionSettings) -> bool {
    !settings.disallowed_tools.is_empty()
        || !settings.additional_directories.is_empty()
        || settings.system_prompt.is_some()
        || settings.append_system_prompt.is_some()
}

pub(crate) fn needs_verification(opts: &crate::SessionOptions, protocol: crate::Protocol) -> bool {
    opts.settings.session_log_upload.is_some()
        || !opts.mcp_servers.is_empty()
        || agent_controls(&opts.settings)
        || (protocol == crate::Protocol::Acp && opts.settings.name.is_some())
}

/// Lifetime is tied to the child, including the loaded DLL on Windows.
struct Verification {
    _mcp_bridges: super::mcp_bridge::McpBridgeSet,
    dir: PrivateDir,
    agent_controls: bool,
    requested: &'static str,
}

impl Verification {
    fn new(rows: &mut Vec<Value>, opts: &crate::SessionOptions, cmd: &mut Command) -> Result<Self> {
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let addon: Option<&[u8]> = Some(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/dsh-profile.node"
        )));
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let addon: Option<&[u8]> = None;
        let bytes = addon.ok_or_else(|| {
            Error::Unsupported("DSH profile verification on this platform".into())
        })?;
        let dir = PrivateDir::create(
            &std::env::temp_dir(),
            "kar-dsh-profile-",
            b"Kar DSH profile v1\n",
        )?;
        let library = dir.path().join("verify.node");
        std::fs::write(&library, bytes)?;
        let url = url::Url::from_file_path(library)
            .map_err(|_| Error::Protocol("invalid DSH plugin path".into()))?;
        let mut required = Vec::new();
        let log_off = opts.settings.session_log_upload.is_some();
        if log_off {
            rows.push(json!({"id":"session-log-deepseek","name":"@deepseek-ai/dsh-session-log-deepseek","config":{"enabled":false}}));
            required.push("session-log-deepseek".to_owned());
        }
        let mut insert = Vec::new();
        let mut mcp_bridges = super::mcp_bridge::McpBridgeSet::default();
        let cwd = opts.cwd.canonicalize()?;
        for (index, (name, server)) in opts.mcp_servers.iter().enumerate() {
            let id = format!("kar-mcp-{}", uuid::Uuid::new_v4());
            insert.push(mcp_bridges.register_server(
                dir.path(),
                index,
                &id,
                name,
                server,
                &cwd,
                &opts.env,
                &opts.unset_env,
            )?);
            required.push(id);
        }
        insert.push(json!({"id":"kar-verify-profile","name":url.as_str()}));
        rows.push(json!({"insert":insert}));
        let agent_controls = agent_controls(&opts.settings) || opts.settings.name.is_some();
        if agent_controls {
            let prompt = match (
                &opts.settings.system_prompt,
                &opts.settings.append_system_prompt,
            ) {
                (Some(base), tail) => Some(
                    json!({"name":"kar:system-prompt","complete":true,"interpolate":false,
                    "text":tail.as_ref().map_or_else(||base.clone(), |tail|format!("{base}\n\n{tail}"))}),
                ),
                (None, Some(tail)) => {
                    Some(json!({"name":"kar:system-prompt","interpolate":false,"text":tail}))
                }
                (None, None) => None,
            };
            let directories = serde_json::to_value(opts.settings.directories(&cwd)?)?;
            std::fs::write(
                dir.path().join("agent-options"),
                serde_json::to_vec(&json!({
                    "tools":{"deny":opts.settings.disallowed_tools},
                    "directories":directories, "prompt":prompt,
                    "name":opts.settings.name, "resuming":opts.resume.is_some(),
                    "cwd":cwd,"barrierName":format!("kar_startup_{}",uuid::Uuid::new_v4().simple())
                }))?,
            )?;
            if let Some(id) = &opts.resume {
                std::fs::write(dir.path().join("agent-target"), id)?;
            }
        }
        let probe = crate::installation::Probe::for_session(opts)?;
        let mut node = probe
            .get("NODE_OPTIONS")
            .map(OsString::from)
            .unwrap_or_default();
        if node
            .to_string_lossy()
            .split_whitespace()
            .any(|arg| arg == "--no-addons")
        {
            return Err(Error::Unsupported(
                "DSH cannot verify native plugin activation with native addons disabled".into(),
            ));
        }
        node.push(" --experimental-addon-modules");
        cmd.env("NODE_OPTIONS", node)
            .env("KAR_DSH_PROFILE_RECEIPT", dir.path().join("result"))
            .env("KAR_DSH_REQUIRED_PLUGINS", required.join(","));
        let requested = match (log_off, opts.mcp_servers.is_empty()) {
            (true, true) => "session_log_upload",
            (true, false) => "session_log_upload and MCP servers",
            (false, false) => "MCP servers",
            (false, true) => "agent policy plugin",
        };
        Ok(Self {
            _mcp_bridges: mcp_bridges,
            dir,
            agent_controls,
            requested,
        })
    }
}

/// The transport drains the child concurrently and signals when it exits.
pub(crate) struct Receipt {
    path: PathBuf,
    requested: &'static str,
}
impl Receipt {
    pub(crate) async fn confirm(self, stopped: impl Future, timeout: Duration) -> Result<()> {
        let path = self.path;
        let receipt = async {
            loop {
                match tokio::fs::read(&path).await {
                    Ok(v) if !v.is_empty() => return v,
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Vec::new(),
                    _ => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        };
        let result = tokio::select! {
            result = receipt => result,
            _ = stopped => Vec::new(),
            _ = tokio::time::sleep(timeout) => Vec::new(),
        };
        let setting = match result.as_slice() {
            b"1" => return Ok(()),
            b"N" => "name in the native session title",
            b"R" => "system_prompt in the native prompt assembly",
            b"A" => "append_system_prompt in the native prompt assembly",
            _ => self.requested,
        };
        Err(Error::Unsupported(format!(
            "DSH did not confirm the requested {setting} during session startup"
        )))
    }
}

/// Publish only the root selected by the transport. A fork's source can be
/// visible in the same DSH process and must keep its own tool policy.
pub(crate) struct AgentBinding(PathBuf);

impl AgentBinding {
    /// The transport is about to request a fresh native identity. Startup
    /// plugins may already be running agents unrelated to this request.
    pub(crate) fn begin(&self) -> Result<()> {
        Ok(std::fs::write(self.0.join("agent-opening"), [])?)
    }

    pub(crate) fn select(&self, id: &str) -> Result<()> {
        let target = self.0.join("agent-target");
        match std::fs::read(&target) {
            Ok(expected) if expected == id.as_bytes() => (),
            Ok(_) => {
                return Err(Error::Protocol(
                    "DSH returned a different policy target".into(),
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let tmp = self.0.join("agent-target.tmp");
                std::fs::write(&tmp, id)?;
                std::fs::rename(tmp, target)?;
            }
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }

    pub(crate) fn bind(self, id: &str) -> Result<Receipt> {
        self.select(id)?;
        std::fs::write(self.0.join("agent-bound"), [])?;
        Ok(Receipt {
            path: self.0.join("agent-result"),
            requested: "root agent settings (tools, directories or system prompt)",
        })
    }
}

pub(crate) struct Patch {
    _file: tempfile::NamedTempFile,
    verification: Option<Verification>,
}

impl Patch {
    pub(crate) fn receipt(&self) -> Option<Receipt> {
        self.verification.as_ref().map(|v| Receipt {
            path: v.dir.path().join("result"),
            requested: v.requested,
        })
    }

    pub(crate) fn agent_binding(&self) -> Option<AgentBinding> {
        self.verification
            .as_ref()
            .filter(|v| v.agent_controls)
            .map(|v| AgentBinding(v.dir.path().to_owned()))
    }

    pub(crate) async fn prepare(
        rt: &crate::Runtime,
        opts: &crate::SessionOptions,
        version: Option<&str>,
        args: &mut Command,
        mut rows: Vec<Value>,
    ) -> Result<Self> {
        if opts.settings.session_log_upload.is_some()
            || !opts.settings.additional_directories.is_empty()
        {
            super::release(rt, opts, version).await?;
        }
        let verification = if needs_verification(opts, rt.protocol) {
            Some(Verification::new(&mut rows, opts, args)?)
        } else {
            None
        };
        let mut builder = tempfile::Builder::new();
        builder.suffix(".json");
        let file = match &verification {
            Some(guard) => builder.tempfile_in(guard.dir.path())?,
            None => builder.tempfile()?,
        };
        serde_json::to_writer(file.as_file(), &rows)?;
        args.arg("--patch").arg(file.path());
        Ok(Self {
            _file: file,
            verification,
        })
    }
}

/// DSH rewrites its profile root during startup. Coordinate session opens and
/// inventory readers until the child has loaded the shared profile.
pub(crate) async fn lock(
    probe: &crate::installation::Probe,
    profile: &str,
) -> Result<std::fs::File> {
    let user = || {
        crate::account::profile_home(probe)
            .ok_or_else(|| Error::Unsupported("DSH home is unavailable".into()))
    };
    let path = match probe
        .get("DSH_HOME")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
    {
        None => user()?.join(".dsh"),
        Some(path) => {
            let raw = Path::new(path);
            if let Some(tail) = path.to_str().and_then(|s| s.strip_prefix("~\\")) {
                user()?.join(tail)
            } else if let Ok(rest) = raw.strip_prefix("~") {
                user()?.join(rest)
            } else {
                probe.cwd.join(raw)
            }
        }
    };
    let dir = path.join("profiles").join(profile);
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".kar-rs-startup.lock"))?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(25)).await
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

pub(crate) async fn session_lock(
    opts: &crate::SessionOptions,
    profile: &str,
) -> Result<std::fs::File> {
    let probe = crate::installation::Probe::for_session(opts)?;
    tokio::time::timeout(opts.handshake_timeout, lock(&probe, profile))
        .await
        .map_err(|_| Error::Timeout(format!("waiting for DSH {profile} profile startup")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn agent_receipt_is_bound_to_one_native_root() {
        let dir = tempfile::tempdir().unwrap();
        let binding = || AgentBinding(dir.path().to_owned());
        let _ = binding().bind("first").unwrap();
        let receipt = binding().bind("first").unwrap();
        assert!(matches!(binding().bind("other"), Err(Error::Protocol(_))));
        assert_eq!(
            std::fs::read(dir.path().join("agent-target")).unwrap(),
            b"first"
        );
        // Confirmation of the general DSH profile cannot confirm the root.
        std::fs::write(dir.path().join("result"), b"1").unwrap();
        assert!(matches!(
            receipt
                .confirm(std::future::ready(()), Duration::from_secs(1))
                .await,
            Err(Error::Unsupported(_))
        ));
        std::fs::write(dir.path().join("agent-result"), b"1").unwrap();
        binding()
            .bind("first")
            .unwrap()
            .confirm(std::future::pending::<()>(), Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mcp_patch_preserves_argv_validates_inputs_and_bridges_http_and_credentialed_stdio() {
        let dir = tempfile::tempdir().unwrap();
        let server = crate::McpServer::Stdio {
            command: "with spaces".into(),
            args: vec!["a b".into(), "".into(), "$HOME".into()],
            env: Default::default(),
        };
        let path = Path::new("workspace");
        let mut bridges = super::super::mcp_bridge::McpBridgeSet::default();
        let row = bridges
            .register_server(
                dir.path(),
                0,
                "entry",
                "server-1",
                &server,
                path,
                &Default::default(),
                &Default::default(),
            )
            .unwrap();
        assert_eq!(row["config"]["command"], "with spaces");
        assert_eq!(row["config"]["args"], json!(["a b", "", "$HOME"]));
        for name in [
            "",
            "with/slash",
            "thirty-two-characters-plus-extra-characters",
        ] {
            assert!(matches!(
                bridges.register_server(
                    dir.path(),
                    1,
                    "entry",
                    name,
                    &server,
                    path,
                    &Default::default(),
                    &Default::default(),
                ),
                Err(Error::Unsupported(_))
            ));
        }

        let bad_http = crate::McpServer::Http {
            url: "ftp://localhost/mcp".into(),
            headers: Default::default(),
        };
        assert!(matches!(
            bridges.register_server(
                dir.path(),
                2,
                "entry",
                "server",
                &bad_http,
                path,
                &Default::default(),
                &Default::default(),
            ),
            Err(Error::Unsupported(_))
        ));

        #[cfg(unix)]
        {
            let mut credentials = server;
            if let crate::McpServer::Stdio { env, .. } = &mut credentials {
                env.insert("TOKEN".into(), "secret-local-fixture".into());
            }
            let http = crate::McpServer::Http {
                url: "http://127.0.0.1:9999/mcp".into(),
                headers: std::collections::BTreeMap::from([(
                    "Authorization".into(),
                    "Bearer secret-header-fixture".into(),
                )]),
            };
            for (idx, entry) in [(3usize, &credentials), (4usize, &http)] {
                let bridged = bridges
                    .register_server(
                        dir.path(),
                        idx,
                        "entry",
                        "server",
                        entry,
                        dir.path(),
                        &Default::default(),
                        &Default::default(),
                    )
                    .unwrap();
                assert_eq!(bridged["config"]["transport"], "stdio");
                let text = serde_json::to_string(&bridged).unwrap();
                assert!(!text.contains("secret-local-fixture"));
                assert!(!text.contains("secret-header-fixture"));
            }
        }
    }
}
