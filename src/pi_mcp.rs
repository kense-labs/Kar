//! Session-scoped MCP configuration for the official Pi RPC host.
use crate::{Error, McpServer, Result, SessionOptions, pi_sdk::Launch, private_dir::PrivateDir};
use serde_json::{Value, json};
use std::{collections::HashSet, fs};
use tokio::process::Command;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
const PLUGIN: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/pi-mcp.node")));
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const PLUGIN: Option<&[u8]> = None;

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
            let config =
                match server {
                    McpServer::Stdio { command, args, env } if env.is_empty() => {
                        json!({"type":"stdio","command":command,"args":args,"exposure":"direct"})
                    }
                    McpServer::Http { url, headers } if headers.is_empty() => {
                        json!({"type":"http","url":url,"exposure":"direct"})
                    }
                    _ => return Err(Error::Unsupported(
                        "Pi RPC does not accept per-server environment variables or HTTP headers"
                            .into(),
                    )),
                };
            Ok(json!({"name":name,"config":config}))
        })
        .collect()
}

pub(crate) async fn prepare(
    launch: &Launch,
    opts: &SessionOptions,
    entries: Vec<Value>,
    cmd: &mut Command,
) -> Result<PrivateDir> {
    let bytes = PLUGIN.ok_or_else(|| {
        Error::Unsupported("Pi native MCP host is unavailable on this platform".into())
    })?;
    let directory = PrivateDir::create(
        &std::env::temp_dir(),
        "kar-pi-mcp-",
        b"kar-pi-session-mcp-v1",
    )?;
    let dir = directory.path().canonicalize()?;
    let plugin = dir.join("pi-mcp.node");
    fs::write(&plugin, bytes)?;
    launch.check_plugin(&plugin, opts).await?;
    let config = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(config.as_file(), &entries)?;
    config
        .persist(dir.join("servers.json"))
        .map_err(|e| e.error)?;
    cmd.arg("--extension")
        .arg(plugin)
        .env("KAR_PI_MCP_DIRECTORY", &dir)
        .env("KAR_PI_MCP_SDK", launch.dist.join("index.js"));
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn validates_names_and_refuses_entry_credentials_before_loading_pi() {
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
        let credentials = BTreeMap::from([("key".into(), "secret-marker".into())]);
        for server in [
            McpServer::Http {
                url: "http://localhost".into(),
                headers: credentials.clone(),
            },
            McpServer::Stdio {
                command: "tool".into(),
                args: vec![],
                env: credentials,
            },
        ] {
            opts.mcp_servers.insert("credentials".into(), server);
            let err = entries(&opts).unwrap_err();
            assert!(matches!(err, Error::Unsupported(_)));
            assert!(!err.to_string().contains("secret-marker"));
        }
    }
}
