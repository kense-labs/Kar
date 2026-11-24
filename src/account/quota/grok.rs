use super::super::native::Wire;
use super::*;
use serde_json::json;

pub(super) fn project(v: &Value, email: Option<String>) -> Result<AccountUsageSnapshot> {
    let c = &v["config"];
    if !c.is_object() {
        return Ok(unsupported(UnsupportedReason::QuotaUnavailable));
    }
    let cents = |key: &str| c[key]["val"].as_f64();
    let used = cents("used");
    let limit = cents("monthlyLimit");
    let percent = if let Some(p) = c["creditUsagePercent"].as_f64() {
        p
    } else if let Some((n, d)) = used.zip(limit).filter(|(_, d)| *d > 0.0) {
        n / d * 100.0
    } else if ["creditUsagePercent", "used", "monthlyLimit"]
        .iter()
        .all(|k| c[k].is_null())
    {
        0.0
    } else {
        return Err(protocol("Grok returned no usable account usage percentage"));
    };
    if percent < 0.0 {
        return Err(protocol("Grok returned a negative usage percentage"));
    }
    let period = &c["currentPeriod"];
    let label = period["type"]
        .as_str()
        .map(|t| {
            let lower = t.trim_start_matches("USAGE_PERIOD_TYPE_").to_lowercase();
            let mut chars = lower.chars();
            let first = chars
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            format!("{first}{} included usage", chars.as_str())
        })
        .unwrap_or_else(|| "Included usage".into());
    let filled = |label: String, used_ratio: f64| QuotaWindow {
        label,
        used_ratio,
        resets_at: instant(if period["end"].is_null() {
            &c["billingPeriodEnd"]
        } else {
            &period["end"]
        }),
        id: None,
        duration_ms: None,
        scope: Some(QuotaScope::Account),
    };
    let mut windows = vec![filled(label, ratio(percent / 100.0))];
    let cap = cents("onDemandCap").unwrap_or(0.0);
    let on_demand = cents("onDemandUsed")
        .unwrap_or_else(|| used.zip(limit).map_or(0.0, |(u, l)| (u - l).max(0.0)));
    if cap > 0.0 {
        windows.push(filled("Pay-as-you-go".into(), ratio(on_demand / cap)));
    }
    let plan = text(if v["subscription_tier"].is_null() {
        &v["subscriptionTier"]
    } else {
        &v["subscription_tier"]
    });
    Ok(AccountUsageSnapshot::Available(AccountQuota {
        windows,
        email,
        plan,
        rate_limited: percent >= 100.0
            && cents("prepaidBalance").unwrap_or(0.0) == 0.0
            && (cap <= 0.0 || on_demand >= cap),
        ..Default::default()
    }))
}

fn refusal(reply: &Value) -> Result<AccountUsageSnapshot> {
    match reply["error"]["code"].as_i64() {
        Some(-32000) => Ok(signed_out(None)),
        Some(-32601) => Ok(unsupported(UnsupportedReason::EndpointUnavailable)),
        _ => Err(protocol("Grok refused the native account query")),
    }
}
pub(super) async fn read(
    probe: &Probe,
    path: &Path,
    until: Instant,
) -> Result<AccountUsageSnapshot> {
    let mut cmd = probe.command(path);
    cmd.args(["agent", "--no-leader", "stdio"]);
    let mut wire = Wire::open(cmd)?;
    let hello = crate::grok::reader_hello(&crate::SessionOptions::new(&probe.cwd));
    let init = wire.rpc(1, "initialize", hello).await?;
    if !init["error"].is_null() {
        return refusal(&init);
    }
    if init["result"]["protocolVersion"] != 1 {
        return Err(protocol("Grok did not negotiate ACP for account usage"));
    }
    let mut reply = None;
    if let Some(id) = crate::grok::auth_method(&init["result"]) {
        let auth = wire.rpc(2, "authenticate", json!({"methodId":id})).await?;
        if !auth["error"].is_null() {
            reply = Some(auth);
        }
    }
    let billing = match reply {
        Some(refusal) => refusal,
        None => wire.rpc(3, "_x.ai/billing", json!({})).await?,
    };
    let mut snapshot = if !billing["error"].is_null() {
        refusal(&billing)?
    } else if let Some(result) = billing.get("result").filter(|v| v.is_object()) {
        project(result, None)?
    } else {
        return Err(protocol("Grok omitted the billing result"));
    };
    if matches!(
        snapshot,
        AccountUsageSnapshot::Available(_) | AccountUsageSnapshot::ReauthRequired { .. }
    ) && let Some(end) = until
        .checked_sub(Duration::from_millis(100))
        .filter(|t| *t > Instant::now())
        && let Ok(Ok(info)) =
            tokio::time::timeout_at(end, wire.rpc(4, "_x.ai/auth/info", json!({}))).await
        && let Some(email) = text(&info["result"]["email"])
    {
        match &mut snapshot {
            AccountUsageSnapshot::Available(q) => q.email = Some(email),
            AccountUsageSnapshot::ReauthRequired { last_account, .. }
                if info["result"]["methodId"].is_null() =>
            {
                *last_account = Some(LastAccount {
                    email: Some(email),
                    display_name: None,
                });
            }
            _ => {}
        }
    }
    Ok(snapshot)
}
