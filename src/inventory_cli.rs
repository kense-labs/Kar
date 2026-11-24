//! Pre-open native workspace inventory commands.
use super::{Error, Result, json_line, runtime, runtimes};
use kar::{InstallationSnapshot, Runtime, inventory::InventoryOptions};
use serde_json::{Value, json};
use std::{num::NonZeroU64, path::PathBuf, time::Duration};

#[derive(clap::Args)]
pub(super) struct WorkspaceInventory {
    /// Choose one runtime, or use all (the default) to inspect every installation.
    runtime: Option<String>,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    #[arg(long, requires = "runtime")]
    bin: Option<PathBuf>,
    #[arg(long)]
    timeout_ms: Option<NonZeroU64>,
}

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Skills,
    Commands,
    McpServers,
    Tools,
}

impl Kind {
    async fn query(self, rt: &Runtime, options: &InventoryOptions) -> Result<Value> {
        Ok(match self {
            Self::Skills => serde_json::to_value(rt.skills(options).await?)?,
            Self::Commands => serde_json::to_value(rt.commands(options).await?)?,
            Self::McpServers => serde_json::to_value(rt.mcp_servers(options).await?)?,
            Self::Tools => serde_json::to_value(rt.tools(options).await?)?,
        })
    }

    async fn row(self, rt: &Runtime, options: &InventoryOptions) -> Result<Value> {
        let read = async {
            let installation = rt.probe_installation(&options.installation).await?;
            if matches!(installation, InstallationSnapshot::Available { .. }) {
                let result = self.query(rt, options).await?;
                Ok(json!({"id":rt.id,"inventory":result}))
            } else {
                Ok(json!({"id":rt.id,"installation":installation}))
            }
        };
        if let Some(timeout) = options.timeout {
            tokio::time::timeout(timeout, read)
                .await
                .map_err(|_| Error::Timeout(format!("{} inventory", rt.id)))?
        } else {
            read.await
        }
    }
}

impl WorkspaceInventory {
    pub async fn print(self, kind: Kind) -> Result<()> {
        let options = InventoryOptions {
            installation: kar::InstallationOptions {
                cwd: Some(self.cwd),
                ..Default::default()
            },
            timeout: self.timeout_ms.map(|n| Duration::from_millis(n.get())),
        };
        if self.runtime.as_deref() == Some("all") && self.bin.is_some() {
            return Err(Error::Refused("--bin requires a single runtime".into()));
        }
        if let Some(id) = self.runtime.filter(|id| id != "all") {
            let mut rt = runtime(&id)?;
            if let Some(bin) = self.bin {
                rt.program = bin;
            }
            return json_line(&kind.query(&rt, &options).await?, std::io::stdout());
        }
        let providers: Vec<_> = runtimes()
            .into_iter()
            .chain([runtime("morph")?, runtime("cybion")?])
            .collect();
        let reads = providers.iter().map(|rt| kind.row(rt, &options));
        let mut failed = false;
        for (rt, result) in providers
            .iter()
            .zip(futures_util::future::join_all(reads).await)
        {
            let row = match result {
                Ok(row) => row,
                Err(error) => {
                    failed = true;
                    json!({"id":rt.id,"error":error.to_string()})
                }
            };
            json_line(&row, std::io::stdout())?;
        }
        if failed {
            return Err(Error::Refused("one or more inventory reads failed".into()));
        }
        Ok(())
    }
}
