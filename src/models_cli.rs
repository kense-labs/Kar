use super::*;
use futures_util::future::join_all;
use kar::{Catalog, InstallationSnapshot, forms::ModelListing};

#[derive(Args)]
pub(super) struct Models {
    runtime: Option<String>,
    #[command(flatten)]
    args: SessionArgs,
    /// Return the model choices used by the workbench, including effort and service tiers.
    #[arg(long)]
    picker: bool,
    /// Per-runtime query deadline in milliseconds.
    #[arg(long)]
    timeout_ms: Option<std::num::NonZeroU64>,
}

impl Models {
    pub(super) async fn run(self) -> Result<()> {
        let timeout = self.timeout_ms.map(|ms| Duration::from_millis(ms.get()));
        if let Some(runtime) = self.runtime {
            let mut open = Open {
                runtime,
                args: self.args,
            };
            if !self.picker {
                let result = open.catalog(Catalog::Models);
                return match timeout {
                    Some(limit) => tokio::time::timeout(limit, result)
                        .await
                        .map_err(|_| Error::Timeout("model catalog".into()))?,
                    None => result.await,
                };
            }
            let rt = open.runtime()?;
            return json_line(
                &choices(rt, listing_options(open.args, timeout)?).await,
                std::io::stdout(),
            );
        }
        if self.args.bin.is_some() {
            return Err(Error::Refused("--bin needs a single runtime".into()));
        }
        let opts = listing_options(self.args, timeout)?;
        // One row per runtime in registry order, even when its executable or account is missing.
        let rows = join_all(runtimes().into_iter().map(|rt| {
            let opts = opts.clone();
            async move {
                let probe = tokio::time::timeout(opts.handshake_timeout, rt.probe_installation(&opts.installation_options())).await;
                match probe {
                    Ok(Ok(inst @ InstallationSnapshot::Available{..})) => json!({"runtime":rt.id,"installation":inst,"choices":choices(rt.clone(),opts).await}),
                    Ok(Ok(inst)) => json!({"runtime":rt.id,"installation":inst}),
                    Ok(Err(error)) => json!({"runtime":rt.id,"error":error.to_string()}),
                    Err(_) => json!({"runtime":rt.id,"error":"installation probe timed out"}),
                }
            }
        })).await;
        for row in rows {
            json_line(&row, std::io::stdout().lock())?;
        }
        Ok(())
    }
}

fn listing_options(args: SessionArgs, timeout: Option<Duration>) -> Result<SessionOptions> {
    if args.fork.is_some() || args.resume.is_some() || args.model.is_some() {
        return Err(Error::Refused(
            "model choices are read for a workspace before choosing a model or opening a session"
                .into(),
        ));
    }
    let mut opts = args.options();
    if opts.settings != kar::SessionSettings::default() || !opts.launch_args.is_empty() {
        return Err(Error::Refused(
            "session launch settings need a single runtime in native catalog mode".into(),
        ));
    }
    if let Some(limit) = timeout {
        opts.handshake_timeout = limit;
    }
    Ok(opts)
}
async fn choices(rt: kar::Runtime, opts: SessionOptions) -> ModelListing {
    rt.form_model_listing(&opts, Vec::new()).await
}
