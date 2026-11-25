//! Installation, account and catalog facts obtained under one caller-owned deadline.
mod customization;
use crate::{
    Error, InstallationOptions, InstallationSnapshot, Result, Runtime,
    SessionCapabilityDeclaration, SessionOptions,
    account::{
        AccountUsageOptions, AccountUsageSnapshot, AuthStatus, AuthStatusOptions, LoginCancellation,
    },
    brands::{RuntimeBrand, RuntimePlatform, upstream_verified_platforms},
    credentials::RuntimeCredentials,
    forms::{ListingReason, ModelEntry, ModelListing},
    providers::{ProviderAuthOptions, ProviderStatus},
};
pub use customization::Customization;
use futures_util::future::join_all;
use serde::Serialize;
use std::{collections::HashSet, time::Duration};
use tokio::{sync::Semaphore, time::Instant};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    #[default]
    Local,
    Account,
    Models,
}

#[derive(Clone, Default)]
pub struct OrientOptions {
    pub installation: InstallationOptions,
    pub depth: Depth,
    pub runtimes: Option<Vec<String>>,
    pub customization: bool,
    pub timeout: Option<Duration>,
    pub cancellation: LoginCancellation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Skipped {
    NotInstalled,
    LoggedOut,
    Depth,
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Probe<T> {
    Ok {
        value: T,
        observed_at: String,
    },
    Failed {
        reason: &'static str,
        observed_at: String,
    },
    TimedOut {
        observed_at: String,
    },
    Skipped {
        because: Skipped,
    },
    Unsupported,
}
fn now() -> String {
    format!("{:.3}", jiff::Timestamp::now())
}
impl<T> Probe<T> {
    fn ok(value: T) -> Self {
        Self::Ok {
            value,
            observed_at: now(),
        }
    }
    fn failed() -> Self {
        Self::Failed {
            reason: "The runtime query failed",
            observed_at: now(),
        }
    }
    fn expired() -> Self {
        Self::TimedOut { observed_at: now() }
    }
}

#[derive(Debug, Serialize)]
pub struct RuntimeOrientation {
    pub id: String,
    pub brand: RuntimeBrand,
    /// Upstream native CI declarations, identified independently of Kar's validation ledger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_verified_platforms: Option<&'static [RuntimePlatform]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<&'static RuntimeCredentials>,
    #[serde(skip_serializing_if = "<[crate::InstallLine]>::is_empty")]
    pub install_lines: &'static [crate::InstallLine],
    pub installation: Probe<InstallationSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilityDeclaration>,
    pub auth: Probe<AuthStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<Probe<Vec<ProviderStatus>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_usage: Option<Probe<AccountUsageSnapshot>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Probe<Vec<ModelEntry>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customization: Option<Probe<Customization>>,
}

#[derive(Debug, Serialize)]
pub struct Orientation {
    pub observed_at: String,
    pub depth: Depth,
    pub runtimes: Vec<RuntimeOrientation>,
}

struct Budget<'a> {
    end: Instant,
    slots: &'a Semaphore,
}
impl Budget<'_> {
    async fn read<T>(
        &self,
        limited: bool,
        operation: impl AsyncFnOnce(Duration) -> Result<T>,
    ) -> Probe<T> {
        let end = self.end.min(Instant::now() + Duration::from_secs(30));
        let read = async {
            let _permit = if limited {
                Some(self.slots.acquire().await.expect("owned orientation slots"))
            } else {
                None
            };
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Probe::expired();
            }
            match operation(left).await {
                Ok(value) => Probe::ok(value),
                Err(Error::Timeout(_)) => Probe::expired(),
                Err(_) => Probe::failed(),
            }
        };
        tokio::time::timeout_at(end, read)
            .await
            .unwrap_or_else(|_| Probe::expired())
    }
}

impl Probe<InstallationSnapshot> {
    fn blocked<T>(&self, supported: bool) -> Probe<T> {
        if !supported {
            return Probe::Unsupported;
        }
        match self {
            Self::Ok {
                value: InstallationSnapshot::Missing,
                ..
            } => Probe::Skipped {
                because: Skipped::NotInstalled,
            },
            Self::TimedOut { observed_at } => Probe::TimedOut {
                observed_at: observed_at.clone(),
            },
            Self::Failed {
                reason,
                observed_at,
            } => Probe::Failed {
                reason,
                observed_at: observed_at.clone(),
            },
            _ => Probe::Unsupported,
        }
    }
}

/// Construct a new report for a registry. A specified selection preserves its order.
/// Cancellation drops queued reads and native readers retain ownership of child cleanup.
pub async fn orient(registry: &[Runtime], opts: &OrientOptions) -> Result<Orientation> {
    if opts.cancellation.is_cancelled() {
        return Err(Error::Refused("orientation cancelled".into()));
    }
    let timeout = opts.timeout.unwrap_or(Duration::from_secs(60));
    if timeout.is_zero() || timeout > Duration::from_millis(i32::MAX as u64) {
        return Err(Error::Protocol(
            "orientation timeout must be positive and fit a timer".into(),
        ));
    }
    let mut selected = vec![];
    if let Some(ids) = &opts.runtimes {
        let mut seen = HashSet::new();
        for id in ids {
            if seen.insert(id) {
                selected.push(
                    registry
                        .iter()
                        .find(|r| &r.id == id)
                        .ok_or_else(|| Error::Unsupported(format!("unknown runtime {id}")))?,
                );
            }
        }
    } else {
        selected.extend(registry);
    }
    let end = Instant::now() + timeout.min(Duration::from_secs(45));
    let slots = Semaphore::new(2);
    let budget = Budget { end, slots: &slots };
    let rows = join_all(
        selected
            .into_iter()
            .map(|rt| read_runtime(rt, opts, &budget)),
    );
    tokio::select! {biased;
        _=opts.cancellation.cancelled()=>Err(Error::Refused("orientation cancelled".into())),
        runtimes=rows=>Ok(Orientation{observed_at:now(),depth:opts.depth,runtimes})
    }
}

async fn read_runtime(
    rt: &Runtime,
    opts: &OrientOptions,
    budget: &Budget<'_>,
) -> RuntimeOrientation {
    let mut config = opts.installation.clone();
    if rt.protocol.child_environment_refusal().is_some() {
        config.env.clear();
    }
    let install = budget
        .read(false, async |_| rt.probe_installation(&config).await)
        .await;
    let has_providers = rt.id == "pi";
    let mut row = RuntimeOrientation {
        id: rt.id.clone(),
        brand: rt.brand(),
        credentials: rt.credentials(),
        install_lines: rt.install_lines(),
        upstream_verified_platforms: upstream_verified_platforms(&rt.id),
        session_capabilities: None,
        auth: install.blocked(rt.has_auth_status()),
        providers: has_providers.then(|| install.blocked(true)),
        account_usage: (opts.depth != Depth::Local)
            .then(|| install.blocked(rt.supports_account_usage())),
        models: (opts.depth == Depth::Models).then(|| install.blocked(has_model_listing(rt))),
        customization: opts.customization.then(|| install.blocked(true)),
        installation: install,
    };
    let Probe::Ok {
        value: snapshot @ InstallationSnapshot::Available { .. },
        ..
    } = &row.installation
    else {
        return row;
    };
    row.session_capabilities = rt.session_capabilities(snapshot);
    let auth = async {
        if !rt.has_auth_status() {
            return Probe::Unsupported;
        }
        if opts.depth == Depth::Local && !rt.auth_status_local_only() {
            return Probe::Skipped {
                because: Skipped::Depth,
            };
        }
        budget
            .read(false, async |timeout| {
                rt.auth_status(&AuthStatusOptions {
                    installation: config.clone(),
                    timeout: Some(timeout),
                })
                .await
            })
            .await
    };
    let providers = async {
        if !has_providers {
            return None;
        }
        let report = budget
            .read(false, async |timeout| {
                let options = ProviderAuthOptions {
                    installation: config.clone(),
                    timeout,
                    ..Default::default()
                };
                rt.pi_provider_auth(options)
                    .await?
                    .list_providers(opts.depth != Depth::Local)
                    .await
            })
            .await;
        Some(report)
    };
    (row.auth, row.providers) = tokio::join!(auth, providers);
    let out = matches!(
        row.auth,
        Probe::Ok {
            value: AuthStatus::LoggedOut { .. },
            ..
        }
    );
    let usage = async {
        if opts.depth == Depth::Local {
            return None;
        }
        if !rt.supports_account_usage() {
            return Some(Probe::Unsupported);
        }
        if out {
            return Some(Probe::Skipped {
                because: Skipped::LoggedOut,
            });
        }
        Some(
            budget
                .read(false, async |timeout| {
                    rt.account_usage(&AccountUsageOptions {
                        installation: config.clone(),
                        timeout: Some(timeout),
                        ..Default::default()
                    })
                    .await
                })
                .await,
        )
    };
    let models = async {
        if opts.depth != Depth::Models {
            return None;
        }
        if !has_model_listing(rt) {
            return Some(Probe::Unsupported);
        }
        if out {
            return Some(Probe::Skipped {
                because: Skipped::LoggedOut,
            });
        }
        Some(model_probe(rt, &config, budget).await)
    };
    let customization = async {
        if opts.customization {
            Some(customization::read(rt, &config, budget).await)
        } else {
            None
        }
    };
    (row.account_usage, row.models, row.customization) = tokio::join!(usage, models, customization);
    row
}

async fn model_probe(
    rt: &Runtime,
    config: &InstallationOptions,
    budget: &Budget<'_>,
) -> Probe<Vec<ModelEntry>> {
    let result = budget
        .read(true, async |timeout| {
            let cwd = match &config.cwd {
                Some(p) => p.clone(),
                None => std::env::current_dir()?,
            };
            let mut opts = SessionOptions::new(cwd);
            for (key, value) in &config.env {
                if let Some(v) = value {
                    opts.env.insert(key.clone(), v.clone());
                } else {
                    opts.remove_env(
                        key.to_str()
                            .ok_or_else(|| Error::Protocol("invalid environment key".into()))?,
                    )?;
                }
            }
            opts.handshake_timeout = timeout;
            Ok(rt.form_model_listing(&opts, vec![]).await)
        })
        .await;
    match result {
        Probe::Ok {
            value: ModelListing::Live { models },
            observed_at,
        } => Probe::Ok {
            value: models,
            observed_at,
        },
        Probe::Ok {
            value:
                ModelListing::Unavailable {
                    reason: ListingReason::Empty,
                },
            observed_at,
        } => Probe::Ok {
            value: vec![],
            observed_at,
        },
        Probe::Ok {
            value:
                ModelListing::Unavailable {
                    reason: ListingReason::Unauthenticated,
                },
            ..
        } => Probe::Skipped {
            because: Skipped::LoggedOut,
        },
        Probe::Ok {
            value:
                ModelListing::Unavailable {
                    reason: ListingReason::Unsupported,
                },
            ..
        }
        | Probe::Unsupported => Probe::Unsupported,
        Probe::Ok {
            value:
                ModelListing::Unavailable {
                    reason: ListingReason::Timeout,
                },
            ..
        }
        | Probe::TimedOut { .. } => Probe::expired(),
        _ => Probe::failed(),
    }
}

fn has_model_listing(runtime: &Runtime) -> bool {
    crate::acp_models::Menu::for_runtime(runtime).is_some()
        || matches!(
            runtime.id.as_str(),
            "dsh"
                | "pi"
                | "qoder"
                | "morph"
                | "claude"
                | "factory"
                | "codex"
                | "qwen"
                | "cursor"
                | "opencode"
                | "grok"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test(start_paused = true)]
    async fn one_deadline_limits_queued_probes_and_hides_private_errors() {
        let budget = Budget {
            end: Instant::now() + Duration::from_secs(1),
            slots: &Semaphore::new(2),
        };
        let starts = AtomicUsize::new(0);
        let outstanding = join_all((0..5).map(|_| {
            budget.read(true, async |_| {
                starts.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<()>>().await
            })
        }))
        .await;
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert!(
            outstanding
                .iter()
                .all(|p| matches!(p, Probe::TimedOut { .. }))
        );
        let next = Budget {
            end: Instant::now() + Duration::from_secs(45),
            slots: &Semaphore::new(2),
        };
        let failure: Probe<()> = next
            .read(false, async |left| {
                assert!(left <= Duration::from_secs(30));
                Err(Error::Protocol("private source text".into()))
            })
            .await;
        assert!(matches!(
            failure,
            Probe::Failed {
                reason: "The runtime query failed",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cancellation_and_unknown_selection_exit_before_any_reader() {
        let options = OrientOptions::default();
        options.cancellation.cancel();
        assert!(matches!(
            orient(&[], &options).await,
            Err(Error::Refused(_))
        ));
        let options = OrientOptions {
            runtimes: Some(vec!["absent".into()]),
            ..Default::default()
        };
        assert!(matches!(
            orient(&[], &options).await,
            Err(Error::Unsupported(_))
        ));
    }
}
