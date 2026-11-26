use super::super::{AuthStatus, profile};
use super::*;

fn bucket_label(raw: &Value) -> String {
    let Some(s) = text(raw) else {
        return "Limit".into();
    };
    if s == "Remaining" {
        return "Limit".into();
    }
    let cleaned = s
        .strip_suffix("Remaining")
        .filter(|prefix| prefix.ends_with(char::is_whitespace))
        .map(str::trim_end)
        .unwrap_or(&s);
    if cleaned.is_empty() {
        "Limit".into()
    } else {
        cleaned.to_owned()
    }
}

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
            let name = bucket_label(&bucket["name"]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bucket_labels_strip_trailing_remaining_without_mangling_other_names() {
        let payload = json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                {"name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 0.25},
                                {"name": "  5h Limit   Remaining  ", "window": "5h", "remaining_fraction": 0.6},
                                {"name": "Remaining", "remaining_fraction": 1.0},
                                {"name": "   ", "remaining_fraction": 0.5}
                            ]
                        },
                        {
                            "buckets": [
                                {"name": "Remaining Credits", "remaining_fraction": 0.4},
                                {"name": "Weekly Limit remaining", "remaining_fraction": 0.8}
                            ]
                        }
                    ]
                }
            }
        });
        let (quota, exhausted) = project(&payload).unwrap();
        assert!(!exhausted);
        let labels: Vec<&str> = quota.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Gemini Models · Weekly Limit",
                "Gemini Models · 5h Limit",
                "Gemini Models · Limit",
                "Gemini Models · Limit",
                "Remaining Credits",
                "Weekly Limit remaining",
            ]
        );
        assert_eq!(quota.windows[0].used_ratio, 0.75);
    }
}
