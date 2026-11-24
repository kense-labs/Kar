use super::super::native::Wire;
use super::*;
use serde_json::json;

fn short(v: &Value) -> Option<String> {
    text(v).filter(|s| s.encode_utf16().count() <= 80)
}
fn epoch(v: &Value) -> Option<String> {
    let n = v.as_f64()?;
    let ms = if n >= 1e12 { n } else { n * 1000.0 };
    Some(format!(
        "{:.3}",
        jiff::Timestamp::from_millisecond(ms as i64).ok()?
    ))
}
fn label(minutes: Option<f64>) -> String {
    let Some(m) = minutes else {
        return "Usage limit".into();
    };
    for (period, name) in [(10080.0, "week"), (1440.0, "day"), (60.0, "hour")] {
        if m % period == 0.0 {
            let n = m / period;
            return format!("{n} {name}{}", if n == 1.0 { "" } else { "s" });
        }
    }
    format!("{m} minutes")
}
fn resets(v: &Value) -> Option<LimitResets> {
    Some(LimitResets {
        available: v["availableCount"].as_u64()?,
        resets: v["credits"].as_array().map(|items| {
            items
                .iter()
                .filter_map(|c| {
                    if c["resetType"] != "codexRateLimits" {
                        return None;
                    }
                    Some(LimitReset {
                        id: c["id"].as_str().filter(|s| !s.is_empty())?.into(),
                        status: match c["status"].as_str() {
                            Some("available") => ResetStatus::Available,
                            Some("redeeming") => ResetStatus::Redeeming,
                            Some("redeemed") => ResetStatus::Redeemed,
                            _ => ResetStatus::Unknown,
                        },
                        granted_at: epoch(&c["grantedAt"]),
                        expires_at: epoch(&c["expiresAt"]),
                        title: c["title"].as_str().map(str::to_owned),
                        description: c["description"].as_str().map(str::to_owned),
                    })
                })
                .collect()
        }),
    })
}
pub(super) fn project(root: &Value, email: Option<String>) -> Result<AccountUsageSnapshot> {
    let historical = root.get("rateLimits").filter(|v| v.is_object());
    let indexed = root["rateLimitsByLimitId"].as_object();
    let historical_id = historical.and_then(|b| short(&b["limitId"]));
    let mut buckets: Vec<_> = historical.into_iter().collect();
    for (id, b) in indexed.into_iter().flatten() {
        if b.is_object()
            && !historical_id
                .as_ref()
                .is_some_and(|h| h == id || short(&b["limitId"]).as_ref() == Some(h))
        {
            buckets.push(b);
        }
    }
    let account = buckets
        .iter()
        .copied()
        .find(|b| b["limitId"] == "codex")
        .or_else(|| {
            historical.filter(|b| historical_id.is_none() && short(&b["limitName"]).is_none())
        })
        .or_else(|| indexed?.get("codex"));
    let credits = account.and_then(|b| {
        let c = &b["credits"];
        (c["hasCredits"].is_boolean() || c["unlimited"] == true).then(|| Credits {
            available: c["hasCredits"] == true || c["unlimited"] == true,
            unlimited: c["unlimited"].as_bool(),
            balance: c["balance"].as_str().map(str::to_owned),
        })
    });
    let mut quota = AccountQuota {
        email,
        rate_limited: account.is_some_and(|b| !b["rateLimitReachedType"].is_null())
            && !credits.as_ref().is_some_and(|c| c.available),
        credits,
        limit_resets: resets(&root["rateLimitResetCredits"]),
        ..Default::default()
    };
    for b in buckets {
        if quota.plan.is_none() {
            quota.plan = short(&b["planType"]);
        }
        let id = short(&b["limitId"]);
        let name = short(&b["limitName"]);
        let scope = if let Some(name) = &name {
            Some(QuotaScope::Model { name: name.clone() })
        } else if id.as_deref() == Some("codex") || account.is_some_and(|a| std::ptr::eq(a, b)) {
            Some(QuotaScope::Account)
        } else {
            None
        };
        let prefix = name.as_deref().or(id
            .as_deref()
            .map(|id| if id == "codex" { "Codex" } else { id }));
        for kind in ["primary", "secondary"] {
            let window = &b[kind];
            let Some(percent) = window["usedPercent"]
                .as_f64()
                .filter(|v| (0.0..=100.0).contains(v))
            else {
                continue;
            };
            let m = window["windowDurationMins"].as_f64().filter(|m| *m > 0.0);
            let duration = label(m);
            quota.windows.push(QuotaWindow {
                label: prefix.map_or_else(|| duration.clone(), |p| format!("{p} · {duration}")),
                used_ratio: ratio(percent / 100.0),
                resets_at: epoch(&window["resetsAt"]),
                id: id.as_ref().map(|s| format!("{s}:{kind}")),
                duration_ms: m.map(|n| (n * 60_000.0) as u64),
                scope: scope.clone(),
            });
        }
    }
    if quota.windows.is_empty() {
        return Err(protocol("Codex returned no usable quota windows"));
    }
    Ok(AccountUsageSnapshot::Available(quota))
}

enum Reply {
    Data {
        result: Value,
        email: Option<String>,
    },
    Reauth,
    Unsupported(UnsupportedReason),
}

async fn request(
    probe: &Probe,
    path: &Path,
    method: &str,
    params: Value,
    legacy_read: bool,
) -> Result<Reply> {
    let mut cmd = probe.command(path);
    cmd.args(["app-server", "--listen", "stdio://"]);
    let mut wire = Wire::open(cmd)?;
    wire.initialize_codex().await?;
    let identity = wire.codex(2, "account/read", json!({})).await?;
    let acc = &identity["result"]["account"];
    if matches!(acc["type"].as_str(), Some("apiKey" | "amazonBedrock"))
        || identity["result"]["requiresOpenaiAuth"] == false
    {
        return Ok(Reply::Unsupported(UnsupportedReason::UnsupportedAuthMode));
    }
    let email = (acc["type"] == "chatgpt")
        .then(|| text(&acc["email"]))
        .flatten();
    let mut reply = wire.codex(3, method, params).await?;
    if legacy_read
        && reply["error"]["message"]
            .as_str()
            .is_some_and(|m| m.to_ascii_lowercase().contains("expected unit"))
    {
        reply = wire.codex(4, method, Value::Null).await?;
    }
    if let Some(error) = reply.get("error").filter(|e| !e.is_null()) {
        let msg = error["message"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if msg.contains("chatgpt authentication required for rate limit reset") {
            return Ok(Reply::Unsupported(UnsupportedReason::UnsupportedAuthMode));
        }
        if msg.contains("authentication required") {
            return Ok(Reply::Reauth);
        }
        if error["code"] == -32601
            || msg.contains("method not found")
            || (msg.contains("unknown variant") && msg.contains(&method.to_ascii_lowercase()))
        {
            return Ok(Reply::Unsupported(UnsupportedReason::EndpointUnavailable));
        }
        return Err(protocol("Codex refused the account quota query"));
    }
    Ok(Reply::Data {
        result: reply["result"].take(),
        email,
    })
}

pub(super) async fn read(
    probe: &Probe,
    path: &Path,
    details: bool,
) -> Result<AccountUsageSnapshot> {
    let params = if details {
        json!({})
    } else {
        json!({"excludeResetCreditDetails":true})
    };
    match request(probe, path, "account/rateLimits/read", params, true).await? {
        Reply::Data { result, email } => project(&result, email),
        Reply::Reauth => Ok(signed_out(None)),
        Reply::Unsupported(reason) => Ok(unsupported(reason)),
    }
}

pub(super) async fn redeem(
    probe: &Probe,
    path: &Path,
    options: &LimitResetOptions,
) -> Result<LimitResetResult> {
    let mut params = json!({"idempotencyKey": options.idempotency_key});
    if let Some(id) = &options.reset_id {
        params["creditId"] = json!(id);
    }
    match request(
        probe,
        path,
        "account/rateLimitResetCredit/consume",
        params,
        false,
    )
    .await?
    {
        Reply::Data { result, .. } => match result["outcome"].as_str() {
            Some("reset") => Ok(LimitResetResult::Reset),
            Some("nothingToReset") => Ok(LimitResetResult::NothingToReset),
            Some("noCredit") => Ok(LimitResetResult::NoReset),
            Some("alreadyRedeemed") => Ok(LimitResetResult::AlreadyRedeemed),
            _ => Err(protocol("Codex did not report a recognized reset outcome")),
        },
        Reply::Reauth => Ok(LimitResetResult::ReauthRequired {
            reason: ReauthReason::NotAuthenticated,
        }),
        Reply::Unsupported(reason) => Ok(LimitResetResult::Unsupported { reason }),
    }
}
