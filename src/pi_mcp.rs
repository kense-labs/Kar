//! Session-scoped MCP configuration for the official Pi RPC host.
mod handoff;
use crate::{Error, McpServer, Result, SessionOptions, pi_sdk::Launch, private_dir::PrivateDir};
use serde_json::{Value, json};
use std::{collections::HashSet, fs};
use tokio::process::Command;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
const PLUGIN: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/pi-mcp.node")));
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const PLUGIN: Option<&[u8]> = None;

fn literal_values(
    values: &std::collections::BTreeMap<String, String>,
) -> serde_json::Map<String, Value> {
    values
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                Value::String(crate::pi_sdk::config_literal(value)),
            )
        })
        .collect()
}

pub(crate) fn entries(opts: &SessionOptions) -> Result<Vec<Value>> {
    let mut names = HashSet::new();
    opts.mcp_servers
        .iter()
        .map(|(name, server)| {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                || crate::options::instructions::reserved_server(name)
                || !names.insert(name.replace('-', "_"))
            {
                return Err(Error::Unsupported(
                    "Pi MCP server names must be valid and have distinct tool namespaces".into(),
                ));
            }
            let config = match server {
                McpServer::Stdio { command, args, env } => {
                    json!({"type":"stdio","command":command,"args":args,"env":literal_values(env),"exposure":"direct"})
                }
                McpServer::Http { url, headers } | McpServer::HostedHttp { url, headers } => {
                    json!({"type":"http","url":url,"headers":literal_values(headers),"exposure":"direct"})
                }
            };
            Ok(json!({"name":name,"config":config}))
        })
        .collect()
}

pub(crate) struct Config {
    directory: PrivateDir,
    transfer: handoff::Transfer,
}
impl Config {
    pub(crate) fn receipt(&self) -> std::path::PathBuf {
        self.directory.path().join("receipt")
    }
    pub(crate) fn spawned(&mut self, pid: u32) {
        self.transfer.spawned(pid);
    }
}

pub(crate) async fn prepare(
    launch: &Launch,
    opts: &SessionOptions,
    entries: Vec<Value>,
    cmd: &mut Command,
) -> Result<Config> {
    let bytes = PLUGIN.ok_or_else(|| {
        Error::Unsupported("Pi native MCP host is unavailable on this platform".into())
    })?;
    let root = std::env::temp_dir();
    #[cfg(unix)]
    let root = if std::os::unix::net::SocketAddr::from_pathname(
        root.join("kar-pi-mcp-XXXXXX/config.sock"),
    )
    .is_err()
    {
        std::path::PathBuf::from("/tmp")
    } else {
        root
    };
    let directory = PrivateDir::create(&root, "kar-pi-mcp-", b"kar-pi-session-mcp-v1")?;
    let dir = directory.path().canonicalize()?;
    let plugin = dir.join("pi-mcp.node");
    fs::write(&plugin, bytes)?;
    launch.check_plugin(&plugin, opts).await?;
    let transfer = handoff::prepare(
        cmd,
        &dir,
        serde_json::to_vec(&entries)?,
        opts.handshake_timeout,
    )?;
    cmd.arg("--extension")
        .arg(plugin)
        .env("KAR_PI_MCP_DIRECTORY", &dir)
        .env("KAR_PI_MCP_SDK", launch.dist.join("index.js"));
    Ok(Config {
        directory,
        transfer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn validates_names_and_passes_literal_server_credentials() {
        let http = McpServer::Http {
            url: "http://localhost/mcp".into(),
            headers: BTreeMap::new(),
        };
        let stdio = McpServer::Stdio {
            command: "my tool".into(),
            args: vec!["$HOME".into(), "--".into()],
            env: BTreeMap::new(),
        };
        let mut opts = SessionOptions::new(".");
        opts.mcp_servers.insert("a-b".into(), http);
        opts.mcp_servers.insert("c_d".into(), stdio.clone());
        let projected = entries(&opts).unwrap();
        assert_eq!(projected[1]["config"]["args"], json!(["$HOME", "--"]));
        for name in ["a_b", "with.space", "🦀", "", "__proto__"] {
            opts.mcp_servers.insert(name.into(), stdio.clone());
            assert!(matches!(entries(&opts), Err(Error::Unsupported(_))));
            opts.mcp_servers.remove(name);
        }
        let values = BTreeMap::from([("key".into(), "!literal$VALUE".into())]);
        for (field, server) in [
            (
                "headers",
                McpServer::Http {
                    url: "http://localhost/mcp".into(),
                    headers: values.clone(),
                },
            ),
            (
                "env",
                McpServer::Stdio {
                    command: "tool".into(),
                    args: vec![],
                    env: values,
                },
            ),
        ] {
            opts.mcp_servers.clear();
            opts.mcp_servers.insert("one".into(), server);
            assert_eq!(
                entries(&opts).unwrap()[0]["config"][field]["key"],
                "$!literal$$VALUE"
            );
        }
    }
}
