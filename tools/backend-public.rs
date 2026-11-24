//! One fixed public contract on one installed provider, in a new disposable workspace.
#[path = "../tests/support/declaration.rs"]
mod declaration;
#[path = "../tests/support/declared_turn.rs"]
mod declared_turn;
use anyhow::{Result, bail, ensure};
use clap::Parser;
use kar::{
    Error, InstallationSnapshot, InstallationSource, Outcome, Session, SessionOptions,
    account::{AccountUsageOptions, AccountUsageSnapshot},
};
use serde_json::json;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    runtime: String,
    #[arg(long)]
    case: String,
}

async fn completed(s: &Session, prompt: &str) -> Result<()> {
    ensure!(
        s.prompt(prompt).await?.wait().await? == Outcome::Completed,
        "root turn did not complete"
    );
    Ok(())
}

async fn listed_efforts(rt: &kar::Runtime, mut opts: SessionOptions) -> Result<serde_json::Value> {
    // A saved selector limits which model to exercise. The inventory itself is workspace-scoped.
    let wanted = opts.model.take();
    let list = rt.form_model_listing(&opts, Vec::new()).await;
    let kar::forms::ModelListing::Live { models } = list else {
        bail!("installed model catalog did not return live choices: {list:?}");
    };
    let candidates: Vec<_> = models
        .iter()
        .filter(|m| m.disabled.is_none() && wanted.as_deref().is_none_or(|id| id == m.id))
        .collect();
    if let Some(selected) = &wanted {
        ensure!(
            !candidates.is_empty(),
            "requested test model {selected} is absent or disabled"
        );
    }
    let entry = candidates
        .iter()
        .find(|m| m.effort_levels.as_ref().is_some_and(|v| v.len() >= 2));
    if let Some(model) = entry {
        let levels = model.effort_levels.as_ref().expect("selected effort menu");
        opts.model = Some(model.id.clone());
        for effort in &levels[..2] {
            opts.settings.effort = Some(effort.clone());
            let session = rt.open(opts.clone()).await?;
            if let Some(expected) = &opts.resume {
                ensure!(
                    session.id() == expected,
                    "resuming changed the native session ID"
                );
            }
            if let Some(actual) = session.effort() {
                ensure!(&actual == effort, "selected {effort} but observed {actual}");
            }
            completed(&session, "Reply hello.").await?;
            opts.resume = Some(session.id().to_owned());
            session.close().await?;
        }
        return Ok(
            json!({"model":model.id,"effort_levels":&levels[..2],"native_session":opts.resume}),
        );
    }
    opts.model = candidates.first().map(|m| m.id.clone());
    opts.settings.effort = Some("low".into());
    match rt.open(opts).await {
        Ok(session) => {
            session.close().await?;
            bail!("model without two listed effort levels accepted low");
        }
        Err(e @ (Error::Refused(_) | Error::Unsupported(_) | Error::Open(_)))
            if e.to_string().contains("low") =>
        {
            Ok(json!({"refusal":e.to_string()}))
        }
        Err(e) => Err(e.into()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let rt = kar::runtime(&args.runtime)?;
    let dir = tempfile::tempdir()?;
    let mut opts = SessionOptions::new(dir.path().canonicalize()?);
    // Provider accounts come from the explicitly configured environment. Keep all
    // generated workspace files outside the candidate repository.
    opts.model = std::env::var(format!(
        "KAR_PUBLIC_{}_MODEL",
        args.runtime.to_ascii_uppercase()
    ))
    .ok()
    .filter(|s| !s.is_empty());
    let installation = rt.probe_installation(&opts.installation_options()).await?;
    let mut evidence = json!({"runtime":rt.id,"case":args.case,"installation":installation});
    match args.case.as_str() {
        "installation.snapshot" => match &installation {
            InstallationSnapshot::Available {
                via: InstallationSource::Executable,
                command,
                ..
            } => {
                ensure!(
                    command.as_ref().is_some_and(|p| p.is_file()),
                    "installed executable has no resolvable file"
                );
            }
            InstallationSnapshot::Failed { reason, .. } => ensure!(
                !reason.trim().is_empty(),
                "failed installation has no reason"
            ),
            _ => {}
        },
        case => {
            ensure!(
                matches!(installation, InstallationSnapshot::Available { .. }),
                "available installation required: {installation:?}"
            );
            match case {
                "account-usage.snapshot" => {
                    ensure!(
                        rt.supports_account_usage(),
                        "account usage is not declared for this runtime"
                    );
                    let usage = rt
                        .account_usage(&AccountUsageOptions {
                            installation: opts.installation_options(),
                            ..Default::default()
                        })
                        .await?;
                    evidence["usage"] = match usage {
                        AccountUsageSnapshot::Available(q) => {
                            ensure!(
                                q.windows
                                    .iter()
                                    .all(|w| (0.0..=1.0).contains(&w.used_ratio)),
                                "quota window fraction out of range"
                            );
                            json!({"kind":"available","windows":q.windows.len()})
                        }
                        AccountUsageSnapshot::ReauthRequired { reason, .. } => {
                            json!({"kind":"reauth_required","reason":reason})
                        }
                        AccountUsageSnapshot::Unavailable { reason, .. } => {
                            json!({"kind":"unavailable","reason":reason})
                        }
                        AccountUsageSnapshot::Unsupported { reason, .. } => {
                            json!({"kind":"unsupported","reason":reason})
                        }
                    };
                }
                "session.capabilities-match-declaration" => {
                    let declared = declared_turn::prepare(&rt, &opts).await;
                    let session = rt.open(opts).await?;
                    declaration::check(&declared, session.capabilities());
                    if declared.native_session_file == Some(true)
                        || declared
                            .context_usage
                            .is_some_and(|u| u.tokens || u.context_window)
                    {
                        completed(&session, "Reply hello.").await?;
                        declared_turn::completed(&declared, &session).await;
                    }
                    evidence["declared"] = json!(declared);
                    evidence["observed"] = json!(session.capabilities());
                    evidence["native_session"] = json!(session.id());
                    session.close().await?;
                }
                "session.effort-listed-levels-apply-across-resume" => {
                    evidence["exercise"] = listed_efforts(&rt, opts).await?;
                }
                "session.effort-never-substituted" => {
                    let requested = "kar-no-such-effort";
                    opts.settings.effort = Some(requested.into());
                    match rt.open(opts).await {
                        Ok(session) => {
                            let actual = session.effort();
                            session.close().await?;
                            ensure!(
                                actual.as_deref() == Some(requested),
                                "requested effort {requested} but observed {actual:?}"
                            );
                            evidence["effort"] = json!(actual);
                        }
                        Err(e @ (Error::Refused(_) | Error::Unsupported(_) | Error::Open(_)))
                            if e.to_string().contains(requested) =>
                        {
                            evidence["refusal"] = json!(e.to_string())
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                "session.resume-or-loud-rejection" => {
                    let first = rt.open(opts.clone()).await?;
                    completed(&first, "Remember this conversation. Reply OK.").await?;
                    let id = first.id().to_owned();
                    first.close().await?;
                    opts.resume = Some(id.clone());
                    match rt.open(opts).await {
                        Ok(session) => {
                            ensure!(
                                session.id() == id,
                                "resuming changed the native session identity"
                            );
                            completed(&session, "Hello again. Reply OK.").await?;
                            session.close().await?;
                            evidence["native_session"] = json!(id);
                        }
                        Err(e @ (Error::Refused(_) | Error::Unsupported(_) | Error::Open(_)))
                            if e.to_string().to_ascii_lowercase().contains("resume") =>
                        {
                            evidence["refusal"] = json!(e.to_string())
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                _ => bail!("unknown fixed public case {case}"),
            }
        }
    }
    println!("{evidence}");
    Ok(())
}
