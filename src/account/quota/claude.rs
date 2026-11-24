use super::super::{AuthStatus, native::Wire, profile};
use super::*;
use serde_json::json;

fn window(
    v: &Value,
    id: &str,
    label: &str,
    hours: Option<u64>,
    scope: Option<QuotaScope>,
) -> Option<QuotaWindow> {
    let percent = v["utilization"].as_f64().filter(|p| *p >= 0.0)?;
    Some(QuotaWindow {
        label: label.into(),
        used_ratio: ratio(percent / 100.0),
        resets_at: instant(&v["resets_at"]),
        id: Some(id.into()),
        duration_ms: hours.map(|h| h * 3_600_000),
        scope,
    })
}
pub(super) fn project(v: &Value, email: Option<String>) -> Result<AccountUsageSnapshot> {
    if v["rate_limits_available"] == false {
        return Ok(unsupported(UnsupportedReason::QuotaUnavailable));
    }
    let limits = &v["rate_limits"];
    if v["rate_limits_available"] != true || !limits.is_object() {
        return Err(protocol("Claude did not return account rate limits"));
    }
    let mut windows: Vec<_> = [
        window(
            &limits["five_hour"],
            "five_hour",
            "Current session",
            Some(5),
            Some(QuotaScope::Account),
        ),
        window(
            &limits["seven_day"],
            "seven_day",
            "Current week (all models)",
            Some(168),
            Some(QuotaScope::Account),
        ),
        window(
            &limits["seven_day_oauth_apps"],
            "seven_day_oauth_apps",
            "Current week (OAuth apps)",
            Some(168),
            None,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut model = |row: &Value, id: &str, name: &str| {
        windows.extend(window(
            row,
            id,
            &format!("Current week ({name})"),
            Some(168),
            Some(QuotaScope::Model { name: name.into() }),
        ));
    };
    if let Some(rows) = limits["model_scoped"].as_array() {
        for row in rows {
            if let Some(name) = text(&row["display_name"]) {
                model(row, &format!("model_scoped:{name}"), &name);
            }
        }
    } else {
        model(&limits["seven_day_opus"], "seven_day_opus", "Opus");
        model(&limits["seven_day_sonnet"], "seven_day_sonnet", "Sonnet");
    }
    let included_exhausted = windows
        .iter()
        .any(|w| w.scope == Some(QuotaScope::Account) && w.used_ratio >= 1.0);
    let extra = &limits["extra_usage"];
    let mut extra_headroom = false;
    if extra["is_enabled"] == true {
        extra_headroom =
            extra.get("monthly_limit") == Some(&Value::Null) && extra["utilization"].is_null();
        if let Some(w) = window(
            extra,
            "extra_usage",
            "Extra usage",
            None,
            Some(QuotaScope::Account),
        ) {
            extra_headroom = w.used_ratio < 1.0;
            windows.push(w);
        }
    }
    if windows.is_empty() {
        return Err(protocol("Claude returned no usable quota windows"));
    }
    Ok(AccountUsageSnapshot::Available(AccountQuota {
        windows,
        rate_limited: included_exhausted && !extra_headroom,
        email,
        plan: text(&v["subscription_type"]),
        ..Default::default()
    }))
}
async fn failure(
    reply: &Value,
    email: Option<String>,
    probe: &Probe,
) -> Result<AccountUsageSnapshot> {
    let msg = reply["error"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if [
        "unsupported control request subtype",
        "unknown control request",
        "not supported in this context",
        "not available on this connection",
    ]
    .iter()
    .any(|s| msg.contains(s))
    {
        return Ok(unsupported(UnsupportedReason::EndpointUnavailable));
    }
    if [
        "not logged in",
        "authentication required",
        "please /login",
        "please run /login",
    ]
    .iter()
    .any(|s| msg.contains(s))
    {
        let last = if let Some(email) = email {
            Some(LastAccount {
                email: Some(email),
                display_name: None,
            })
        } else {
            let mut status = Reader::Claude.logged_out();
            profile::enrich(Reader::Claude, probe, "", &mut status).await;
            match status {
                AuthStatus::LoggedOut { last_account, .. } => last_account,
                _ => None,
            }
        };
        return Ok(signed_out(last));
    }
    Err(protocol("Claude refused the account usage control request"))
}
pub(super) async fn read(probe: &Probe, path: &Path) -> Result<AccountUsageSnapshot> {
    let help = output(Reader::Claude, probe, path, &["--help"]).await?;
    if !help.status.success()
        || !String::from_utf8_lossy(&help.stdout)
            .split_whitespace()
            .any(|s| s == "--safe-mode")
    {
        return Ok(unsupported(UnsupportedReason::UnsupportedInstallation));
    }
    let mut cmd = native::command(Reader::Claude, probe, path);
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--safe-mode",
    ]);
    let mut wire = Wire::open(cmd)?;
    let init = wire.control_reply(json!({"subtype":"initialize"})).await?;
    if init["subtype"] != "success" {
        return failure(&init, None, probe).await;
    }
    let account = &init["response"]["account"];
    let email = text(&account["email"]);
    let reply = wire
        .control_reply(json!({"subtype":"get_usage","skip_behaviors":true}))
        .await?;
    if reply["subtype"] != "success" {
        return failure(&reply, email, probe).await;
    }
    let mut snapshot = project(&reply["response"], email.clone())?;
    if let AccountUsageSnapshot::Available(q) = &mut snapshot {
        let mut status = Reader::Claude.logged_in(super::super::LoginAccount {
            email,
            method: Some("claude.ai".into()),
            ..Default::default()
        });
        profile::enrich(
            Reader::Claude,
            probe,
            &json!({"email":account["email"],"orgName":account["organization"]}).to_string(),
            &mut status,
        )
        .await;
        if let AuthStatus::LoggedIn {
            account: Some(a), ..
        } = status
            && let Some(plan) = a.plan
        {
            q.plan = Some(plan);
        }
    }
    Ok(snapshot)
}
