use super::*;
use futures_util::future::join_all;
use kar::{
    Catalog, InstallationSnapshot,
    forms::{ListingReason, ModelListing},
};
use serde_json::Value;

#[derive(Args)]
pub(super) struct Models {
    runtime: Option<String>,
    #[command(flatten)]
    args: SessionArgs,
    /// Return the model choices used by the workbench, including effort and service tiers.
    #[arg(long)]
    picker: bool,
    /// Print one JSON array of per-runtime reports.
    #[arg(long, conflicts_with = "text")]
    json: bool,
    /// Print human-readable model rows, including the native selector and defaults.
    #[arg(long)]
    text: bool,
    /// Per-runtime query deadline in milliseconds.
    #[arg(long)]
    timeout_ms: Option<std::num::NonZeroU64>,
}

impl Models {
    pub(super) async fn run(self) -> Result<()> {
        let timeout = self.timeout_ms.map(|ms| Duration::from_millis(ms.get()));
        let single = self.runtime.as_deref().filter(|id| *id != "all");
        if let Some(id) = single
            && !self.json
            && !self.text
        {
            let mut open = Open {
                runtime: id.to_owned(),
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
        let mut args = self.args;
        let providers = if let Some(id) = single {
            let mut rt = runtime(id)?;
            if let Some(bin) = args.bin.take() {
                if rt.protocol == kar::Protocol::Cybion {
                    return Err(Error::Unsupported(
                        "--bin for a remote Cybion thread".into(),
                    ));
                }
                rt.program = bin;
            }
            vec![rt]
        } else {
            if args.bin.is_some() {
                return Err(Error::Refused("--bin needs a single runtime".into()));
            }
            runtimes()
                .into_iter()
                .chain([runtime("morph")?, runtime("cybion")?])
                .collect()
        };
        let opts = listing_options(args, timeout)?;
        let rows = join_all(providers.iter().map(|rt| report(rt, &opts, timeout))).await;
        if self.json {
            json_line(&rows, std::io::stdout().lock())?;
        } else if self.text || !self.picker {
            let mut out = std::io::stdout().lock();
            for row in &rows {
                render(&mut out, row)?;
            }
        } else {
            for row in &rows {
                json_line(row, std::io::stdout().lock())?;
            }
        }
        if rows.iter().any(|r| r.get("error").is_some()) {
            return Err(Error::Refused("one or more model queries failed".into()));
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

async fn report(rt: &kar::Runtime, opts: &SessionOptions, timeout: Option<Duration>) -> Value {
    let read = async {
        if rt.protocol == kar::Protocol::Cybion {
            return Ok(
                json!({"runtime":rt.id,"choices":{"status":"unavailable","reason":"unsupported"}}),
            );
        }
        let installation = rt.probe_installation(&opts.installation_options()).await?;
        if !matches!(installation, InstallationSnapshot::Available { .. }) {
            return Ok(json!({"runtime":rt.id, "installation":installation, "choices":null}));
        }
        let menu = rt.form_model_listing(opts, vec![]).await;
        if let ModelListing::Unavailable {
            reason: ListingReason::Timeout | ListingReason::Failed,
        } = menu
        {
            let reason = match menu {
                ModelListing::Unavailable {
                    reason: ListingReason::Timeout,
                } => "native model query timed out",
                _ => "native model query failed",
            };
            return Err(Error::Refused(reason.to_owned()));
        }
        Ok(json!({"runtime":rt.id, "installation":installation, "choices":menu}))
    };
    let result = match timeout {
        Some(limit) => tokio::time::timeout(limit, read)
            .await
            .unwrap_or_else(|_| Err(Error::Timeout("model query deadline exceeded".into()))),
        None => read.await,
    };
    result.unwrap_or_else(|e| json!({"runtime":rt.id,"error":e.to_string()}))
}

fn render(out: &mut impl Write, row: &Value) -> Result<()> {
    let runtime = row["runtime"].as_str().unwrap_or_default();
    if let Some(error) = row["error"].as_str() {
        writeln!(out, "{runtime}\terror\t{error}")?;
    } else if row["choices"].is_null() {
        writeln!(
            out,
            "{runtime}\t{}",
            row["installation"]["kind"].as_str().unwrap_or("unknown")
        )?;
    } else if let Some(models) = row["choices"]["models"].as_array() {
        for model in models {
            let id = model["id"].as_str().unwrap_or_default();
            write!(out, "{runtime}\t{id}")?;
            if let Some(resolved) = model["resolvedId"].as_str().filter(|r| *r != id) {
                write!(out, " -> {resolved}")?;
            }
            if let Some(name) = model["displayName"].as_str().filter(|s| *s != id) {
                write!(out, " {name:?}")?;
            }
            for (field, default, label) in [
                ("effortLevels", "defaultEffort", "effort"),
                ("serviceTiers", "defaultServiceTier", "tiers"),
            ] {
                if let Some(items) = model[field].as_array().filter(|list| !list.is_empty()) {
                    let items = items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|s| {
                            if model[default].as_str() == Some(s) {
                                format!("{s}*")
                            } else {
                                s.to_owned()
                            }
                        })
                        .collect::<Vec<_>>();
                    write!(out, " {label} [{}]", items.join(","))?;
                }
            }
            if model["default"] == true {
                write!(out, " (default)")?;
            }
            if let Some(disabled) = model.get("disabled") {
                write!(
                    out,
                    " (disabled {})",
                    disabled
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unavailable")
                )?;
            }
            writeln!(out)?;
        }
    } else {
        writeln!(
            out,
            "{runtime}\t{}",
            row["choices"]["reason"].as_str().unwrap_or("unknown")
        )?;
    }
    Ok(())
}
