use super::super::{AuthStatus, profile};
use super::*;

pub(super) fn project(root: &Value) -> Result<(AccountQuota, bool)> {
    if root["status"] != "SUCCESS" || root["command"]["name"] != "usage" {
        return Err(protocol(
            "Antigravity did not return a successful usage command",
        ));
    }
    let groups = root
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol("Antigravity omitted quota groups"))?;
    let mut windows = vec![];
    let mut all_exhausted = true;
    for group in groups.iter().filter(|g| g.is_object()) {
        let group_name = text(&group["name"]);
        let mut exhausted = false;
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            let Some(remaining) = bucket["remaining_fraction"]
                .as_f64()
                .filter(|r| (0.0..=1.0).contains(r))
            else {
                continue;
            };
            let name = text(&bucket["name"]).unwrap_or_else(|| "Limit".into());
            exhausted |= remaining == 0.0;
            windows.push(QuotaWindow {
                label: group_name
                    .as_ref()
                    .map(|g| format!("{g} · {name}"))
                    .unwrap_or(name),
                used_ratio: if remaining == 0.0 {
                    1.0
                } else {
                    ratio(1.0 - remaining).min(0.999999)
                },
                duration_ms: match bucket["window"].as_str() {
                    Some("5h") => Some(18_000_000),
                    Some("weekly") => Some(604_800_000),
                    _ => None,
                },
                scope: group_name.clone().map(|name| QuotaScope::Models { name }),
                resets_at: instant(&bucket["reset_time"]),
                id: bucket["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            });
        }
        all_exhausted &= exhausted;
    }
    if windows.is_empty() {
        return Err(protocol("Antigravity returned no usable quota buckets"));
    }
    Ok((
        AccountQuota {
            windows,
            ..Default::default()
        },
        all_exhausted,
    ))
}
pub(super) async fn read(
    probe: &Probe,
    path: &Path,
    until: Instant,
) -> Result<AccountUsageSnapshot> {
    if !profile::default_gemini_directory(probe) {
        return Ok(unsupported(UnsupportedReason::UnsupportedAuthStorage));
    }
    let account = match native::query(Reader::Antigravity, probe, path).await? {
        AuthStatus::LoggedIn { account, .. } => account.unwrap_or_default(),
        AuthStatus::LoggedOut { last_account, .. } => return Ok(signed_out(last_account)),
        _ => return Err(protocol("Antigravity did not confirm a signed-in account")),
    };
    let result = output(
        Reader::Antigravity,
        probe,
        path,
        &["-p", "/usage", "--output-format", "json"],
    )
    .await?;
    let value: Value = serde_json::from_slice(&result.stdout)
        .map_err(|_| protocol("Antigravity omitted its usage JSON"))?;
    let (mut quota, all_exhausted) = project(&value)?;
    quota.email = account.email;
    quota.plan = account.plan;
    if all_exhausted {
        let credits = output(
            Reader::Antigravity,
            probe,
            path,
            &["-p", "/credits", "--output-format", "json"],
        )
        .await?;
        let report: Value = serde_json::from_slice(&credits.stdout).unwrap_or_default();
        if report["status"] == "SUCCESS"
            && report["command"]["name"] == "credits"
            && let Some(balance) = report
                .pointer("/command/data/remaining_credits")
                .and_then(Value::as_f64)
        {
            quota.rate_limited = balance <= 0.0;
            quota.credits = Some(Credits {
                available: balance > 0.0,
                balance: Some(balance.to_string()),
                unlimited: None,
            });
        }
    }
    quota.plan = super::super::antigravity::plan(probe, path, until)
        .await
        .or(quota.plan);
    Ok(AccountUsageSnapshot::Available(quota))
}
