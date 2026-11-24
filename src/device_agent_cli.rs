use clap::{Args, Subcommand};
use kar::{Error, Result};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};

#[derive(Args)]
pub struct DeviceAgent {
    /// Private config issued for this chat and device when agent access is enabled.
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Operation,
}
#[derive(Subcommand)]
enum Operation {
    /// Read the device's settings and foreground app.
    Detail,
    /// Write the device screenshot to stdout as PNG.
    Screenshot,
    /// Apply one device action from a JSON object.
    Action {
        #[arg(long)]
        json: String,
    },
}
#[derive(Deserialize)]
struct Config {
    endpoint: String,
    token: String,
}
impl DeviceAgent {
    pub async fn run(self) -> Result<()> {
        let mut data = Vec::new();
        File::open(self.config)?.take(8193).read_to_end(&mut data)?;
        if data.len() > 8192 {
            return Err(Error::Refused(
                "oversized device agent configuration".into(),
            ));
        }
        let config: Config = serde_json::from_slice(&data)?;
        let url = url::Url::parse(&config.endpoint)
            .map_err(|_| Error::Refused("invalid device agent endpoint".into()))?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || config.token.len() != 64
            || !config.token.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Error::Refused(
                "invalid local device agent configuration".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()?;
        let png = matches!(self.command, Operation::Screenshot);
        let req = match self.command {
            Operation::Detail => client.get(format!("{}detail", url)),
            Operation::Screenshot => client.get(format!("{}screenshot", url)),
            Operation::Action { json } => client
                .post(format!("{}action", url))
                .json(&serde_json::from_str::<Value>(&json)?),
        };
        let mut response = req.bearer_auth(config.token).send().await?;
        if !response.status().is_success() {
            return Err(Error::Refused(format!(
                "device agent request returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > 32 * 1024 * 1024 {
                return Err(Error::Refused("device response exceeded size limit".into()));
            }
            body.extend_from_slice(&chunk);
        }
        let mut out = std::io::stdout().lock();
        if png {
            out.write_all(&body)?;
        } else {
            serde_json::to_writer(&mut out, &serde_json::from_slice::<Value>(&body)?)?;
            writeln!(out)?;
        }
        Ok(())
    }
}
