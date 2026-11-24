use crate::{McpServer, McpTransport, SessionOptions};
use std::fmt;

/// Ephemeral MCP registration minted by a running Kar workbench.
#[derive(Clone)]
pub struct WorkbenchRegistration {
    pub(crate) http: McpServer,
    pub(crate) stdio: Option<McpServer>,
    #[cfg(target_os = "macos")]
    pub(crate) root: tokio::sync::watch::Sender<Option<crate::resources::tree::ProcessId>>,
}
impl fmt::Debug for WorkbenchRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkbenchRegistration")
    }
}
impl WorkbenchRegistration {
    pub(crate) const NAME: &'static str = "kar_workbench";
    pub(crate) fn new(http: McpServer) -> Self {
        Self {
            http,
            stdio: None,
            #[cfg(target_os = "macos")]
            root: tokio::sync::watch::channel(None).0,
        }
    }
}
impl SessionOptions {
    pub(crate) fn add_workbench_server(&mut self, transport: McpTransport) {
        let Some(registration) = &self.workbench_registration else {
            return;
        };
        let server = match transport {
            McpTransport::Http => Some(&registration.http),
            McpTransport::Stdio => registration.stdio.as_ref(),
        };
        if let Some(s) = server {
            self.mcp_servers
                .entry(WorkbenchRegistration::NAME.into())
                .or_insert_with(|| s.clone());
        }
    }
    pub(crate) fn add_dsh_workbench(&mut self, version: Option<&str>) {
        let supported = version
            .and_then(crate::options::mcp_capabilities::release)
            .is_some_and(|v| v >= semver::Version::parse("0.2.1-rc.5").expect("release"));
        let addons_allowed = crate::installation::Probe::for_session(self)
            .ok()
            .is_some_and(|p| {
                p.get("NODE_OPTIONS").is_none_or(|s| {
                    !s.to_string_lossy()
                        .split_whitespace()
                        .any(|arg| arg == "--no-addons")
                })
            });
        if supported && addons_allowed {
            self.add_workbench_server(McpTransport::Stdio);
        }
    }
    pub(crate) fn bind_bridge_process(&self, _root: Option<&crate::resources::Root>) {
        #[cfg(target_os = "macos")]
        if let (Some(r), Some(root)) = (&self.workbench_registration, _root) {
            r.root.send_replace(Some(root.identity()));
        }
    }
}
