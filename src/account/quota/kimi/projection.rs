use super::super::*;

fn window(label: String, used_ratio: f64, resets_at: Option<String>, account: bool) -> QuotaWindow {
    QuotaWindow {
        label,
        used_ratio,
        resets_at,
        scope: account.then_some(QuotaScope::Account),
        id: None,
        duration_ms: None,
    }
}

fn cap_window(quota: &mut AccountQuota, cap: Option<f64>, used: Option<f64>) {
    if let Some((limit, used)) = cap.zip(used) {
        quota.windows.push(window(
            "Extra Usage monthly limit".into(),
            ratio(used / limit),
            None,
            true,
        ));
    }
}

// Native Web handles the service's snake-case keys before returning the same buckets.
const BUCKETS: [(&str, &str, &str); 4] = [
    ("limit5h", "limit_5h", "5h limit"),
    ("limit7d", "limit_7d", "Weekly limit"),
    ("monthTotal", "limit_month_total", "Monthly limit"),
    ("monthCode", "limit_month_code", "Monthly code limit"),
];

pub(super) fn native(v: &Value) -> Result<AccountUsageSnapshot> {
    let buckets = &v["usages"];
    if v.get("usages").is_none() || buckets.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(unsupported(UnsupportedReason::QuotaUnavailable));
    }
    let mut q = AccountQuota::default();
    for (key, _, label) in BUCKETS {
        let row = &buckets[key];
        if let Some(r) = row["usedRatio"].as_f64() {
            q.windows.push(window(
                label.into(),
                ratio(r),
                instant(&row["resetAt"]),
                true,
            ));
        }
    }
    if q.windows.is_empty() {
        return Err(protocol("Kimi returned no usable native quota windows"));
    }
    let exhausted = q.windows.iter().any(|w| w.used_ratio == 1.0);
    let extra = &v["extraUsage"];
    let enabled = extra["monthlyChargeLimitEnabled"].as_bool();
    let cap = extra["monthlyChargeLimitCents"]
        .as_f64()
        .filter(|c| enabled == Some(true) && *c > 0.0);
    let used = extra["monthlyUsedCents"].as_f64().filter(|c| *c >= 0.0);
    let headroom = extra["balanceCents"].as_f64().is_some_and(|b| b > 0.0)
        && (enabled == Some(false) || cap.zip(used).is_some_and(|(c, u)| u < c));
    cap_window(&mut q, cap, used);
    q.rate_limited = exhausted && !headroom;
    Ok(AccountUsageSnapshot::Available(q))
}

fn count(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.trim().parse().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(f64::trunc)
}

fn timed_label(spec: &Value) -> Option<String> {
    let mut n = count(&spec["duration"]).filter(|v| *v > 0.0)?;
    let unit = match spec["timeUnit"].as_str()? {
        "TIME_UNIT_WEEK" => return Some("Weekly limit".into()),
        "TIME_UNIT_MINUTE" if n >= 60.0 && n % 60.0 == 0.0 => {
            n /= 60.0;
            "h"
        }
        "TIME_UNIT_MINUTE" => "m",
        "TIME_UNIT_HOUR" => "h",
        "TIME_UNIT_DAY" => "d",
        _ => return None,
    };
    Some(format!("{n}{unit} limit"))
}
fn row(detail: &Value, timed: Option<String>, name: Option<String>) -> Option<QuotaWindow> {
    let limit = count(&detail["limit"]).filter(|n| *n > 0.0)?;
    let used = match detail.get("used") {
        Some(v) => count(v)?,
        None => 0.0,
    };
    let account = timed.is_some();
    let label = timed
        .or(name)
        .or_else(|| text(&detail["name"]))
        .unwrap_or_else(|| "Limit".into());
    Some(window(
        label,
        ratio(used / limit),
        instant(&detail["resetTime"]),
        account,
    ))
}
struct Wallet {
    balance: f64,
    cap: Option<f64>,
    used: f64,
}
fn wallet(extra: &Value) -> Option<Wallet> {
    let balance = &extra["balance"];
    if balance["type"] != "BOOSTER" || count(&balance["amount"])? <= 0.0 {
        return None;
    }
    let remaining = if let Some(v) = balance.get("amountLeft") {
        count(v)?
    } else {
        0.0
    };
    let used = if let Some(v) = extra.get("monthlyUsed") {
        count(&v["priceInCents"])?
    } else {
        0.0
    };
    let cap = match extra.get("monthlyChargeLimitEnabled") {
        None | Some(Value::Bool(false)) => None,
        Some(Value::Bool(true)) => {
            let c = count(&extra["monthlyChargeLimit"]["priceInCents"])?;
            (c > 0.0).then_some(c)
        }
        _ => return None,
    };
    Some(Wallet {
        balance: remaining,
        cap,
        used,
    })
}
pub(super) fn direct(v: &Value) -> Result<AccountUsageSnapshot> {
    if !v.is_object() {
        return Err(protocol("Invalid Kimi quota response"));
    }
    let mut q = AccountQuota::default();
    q.windows
        .extend(row(&v["usage"], Some("Weekly limit".into()), None));
    for limit in v["limits"].as_array().into_iter().flatten() {
        q.windows.extend(row(
            &limit["detail"],
            timed_label(&limit["window"]),
            text(&limit["name"]),
        ));
    }
    if q.windows.is_empty() {
        for (_, key, label) in BUCKETS {
            let bucket = &v["usages"][key];
            let value = &bucket["used_ratio"];
            let used = value
                .as_f64()
                .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
                .filter(|r| r.is_finite());
            if let Some(used) = used {
                q.windows.push(window(
                    label.into(),
                    ratio(used),
                    instant(&bucket["reset_time"]),
                    true,
                ));
            }
        }
    }
    let exhausted = q.windows.iter().any(|w| w.used_ratio == 1.0);
    let extra = wallet(&v["boosterWallet"]);
    if let Some(e) = &extra {
        cap_window(&mut q, e.cap, Some(e.used));
    }
    if q.windows.is_empty() {
        let no_native = v
            .get("usages")
            .is_none_or(|r| r.as_object().is_some_and(|o| o.is_empty()));
        let no_legacy = v.get("usage").is_none()
            && v.get("limits")
                .is_none_or(|r| r.as_array().is_some_and(|a| a.is_empty()));
        if no_native && no_legacy {
            return Ok(unsupported(UnsupportedReason::QuotaUnavailable));
        }
        return Err(protocol("Kimi returned no usable quota windows"));
    }
    q.rate_limited =
        exhausted && !extra.is_some_and(|w| w.balance > 0.0 && w.cap.is_none_or(|c| w.used < c));
    Ok(AccountUsageSnapshot::Available(q))
}
