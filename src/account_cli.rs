//! Aggregate read-only account queries without sharing one provider's options or timeout with another.
use super::{Account, Error, Result, json_line, runtime, runtimes};
use kar::{
    InstallationOptions, InstallationSnapshot, Runtime,
    account::{AccountUsageOptions, AuthStatusOptions},
};
use serde_json::{Value, json};
use std::time::Duration;

enum Read {
    Status(AuthStatusOptions),
    Usage(AccountUsageOptions),
}

impl Read {
    fn options(&self) -> (&InstallationOptions, Option<Duration>) {
        match self {
            Self::Status(opts) => (&opts.installation, opts.timeout),
            Self::Usage(opts) => (&opts.installation, opts.timeout),
        }
    }

    async fn query(&self, provider: &Runtime) -> Result<Value> {
        let (opts, timeout) = self.options();
        let read = async {
            let installation = provider.probe_installation(opts).await?;
            if !matches!(installation, InstallationSnapshot::Available { .. }) {
                return Ok(json!({"id":provider.id,"installation":installation}));
            }
            let result = match self {
                Self::Status(opts) => {
                    json!({"id":provider.id, "status":provider.auth_status(opts).await?})
                }
                Self::Usage(opts) => {
                    json!({"id":provider.id, "usage":provider.account_usage(opts).await?})
                }
            };
            Ok(result)
        };
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, read)
                .await
                .map_err(|_| Error::Timeout(format!("{} account query", provider.id)))?
        } else {
            read.await
        }
    }
}

pub(super) async fn batch(action: Account) -> Result<()> {
    let query = match action {
        Account::Status { timeout_ms, .. } => Read::Status(AuthStatusOptions {
            timeout: timeout_ms.map(|n| Duration::from_millis(n.get())),
            ..Default::default()
        }),
        Account::Usage {
            bin: None,
            cwd,
            count_only,
            timeout_ms,
            ..
        } => Read::Usage(AccountUsageOptions {
            installation: InstallationOptions {
                cwd: Some(cwd),
                ..Default::default()
            },
            timeout: timeout_ms.map(Duration::from_millis),
            limit_reset_details: !count_only,
        }),
        Account::Usage { bin: Some(_), .. } => {
            return Err(Error::Protocol(
                "--bin requires one selected runtime".into(),
            ));
        }
        _ => {
            return Err(Error::Protocol(
                "select one runtime for this account operation".into(),
            ));
        }
    };
    let providers: Vec<_> = runtimes()
        .into_iter()
        .chain([runtime("morph")?, runtime("cybion")?])
        .collect();
    let checks = providers.iter().map(|rt| query.query(rt));
    let mut failed = false;
    for (rt, result) in providers
        .iter()
        .zip(futures_util::future::join_all(checks).await)
    {
        let row = match result {
            Ok(value) => value,
            Err(Error::Unsupported(detail)) => json!({"id":rt.id,"unsupported":detail}),
            Err(error) => {
                failed = true;
                json!({"id":rt.id,"error":error.to_string()})
            }
        };
        json_line(&row, std::io::stdout())?;
    }
    if failed {
        return Err(Error::Refused("one or more account queries failed".into()));
    }
    Ok(())
}
