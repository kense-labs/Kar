use super::{billing, *};
use aes_gcm::{
    AesGcm, KeyInit,
    aead::{AeadInPlace, consts::U16},
    aes::Aes256,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const API: &str = "https://api.factory.ai";
const EU_API: &str = "https://api.eu.factory.ai";

fn project(root: &Value, now: jiff::Timestamp) -> Result<AccountUsageSnapshot> {
    let mut quota = AccountQuota::default();
    let mut any_group = false;
    let mut unlimited = false;
    let mut exhausted = false;
    for (group, prefix, scope) in [
        ("standard", "", QuotaScope::Account),
        (
            "core",
            "Droid Core ",
            QuotaScope::Models {
                name: "Droid Core".into(),
            },
        ),
    ] {
        let buckets = &root["limits"][group];
        if !buckets.is_object() {
            continue;
        }
        any_group = true;
        for (key, label, duration_ms) in [
            ("fiveHour", "5-hour", Some(18_000_000)),
            ("weekly", "Weekly", Some(604_800_000)),
            ("monthly", "Monthly", None),
        ] {
            let bucket = &buckets[key];
            if bucket["unlimited"] == true {
                unlimited = true;
                continue;
            }
            let Some(percent) = bucket["usedPercent"].as_f64().filter(|v| *v >= 0.0) else {
                continue;
            };
            let end = match &bucket["windowEnd"] {
                Value::Null => None,
                v => {
                    let Some(end) = v.as_str().and_then(|s| s.parse::<jiff::Timestamp>().ok())
                    else {
                        continue;
                    };
                    (end > now).then_some(end)
                }
            };
            let used_ratio = if end.is_some() {
                ratio(percent / 100.0)
            } else {
                0.0
            };
            exhausted |= group == "standard" && used_ratio >= 1.0;
            quota.windows.push(QuotaWindow {
                id: Some(format!("{group}.{key}")),
                label: format!("{prefix}{label}"),
                used_ratio,
                resets_at: end.map(|t| format!("{t:.3}")),
                duration_ms,
                scope: Some(scope.clone()),
            });
        }
    }
    if !any_group && root["hasActiveSubscription"] == false {
        return Ok(unsupported(UnsupportedReason::QuotaUnavailable));
    }
    if quota.windows.is_empty() && !unlimited {
        return Err(protocol("Factory returned no usable quota windows"));
    }
    let extra = root["overagePreference"] == "extraUsage"
        && root["extraUsageAllowed"] == true
        && root["extraUsageBalanceCents"]
            .as_f64()
            .is_some_and(|v| v > 0.0);
    quota.rate_limited = exhausted && !extra;
    Ok(AccountUsageSnapshot::Available(quota))
}

#[derive(serde::Deserialize)]
struct Login {
    access_token: String,
    active_organization_id: Option<String>,
    region: Option<String>,
    whoami: Option<Routing>,
}
#[derive(serde::Deserialize)]
struct Routing {
    region: Option<String>,
}
fn stored_login(sealed: &str, key: &str) -> Result<Login> {
    // Droid's keyfile backend uses 16-byte nonces and a detached GCM tag.
    let invalid = || protocol("Invalid Factory keyfile login");
    let decode = |s: &str| STANDARD.decode(s).map_err(|_| invalid());
    let mut parts = sealed.trim().split(':');
    let nonce: [u8; 16] = decode(parts.next().ok_or_else(invalid)?)?
        .try_into()
        .map_err(|_| invalid())?;
    let tag: [u8; 16] = decode(parts.next().ok_or_else(invalid)?)?
        .try_into()
        .map_err(|_| invalid())?;
    let mut data = Zeroizing::new(decode(parts.next().ok_or_else(invalid)?)?);
    if parts.next().is_some() {
        return Err(invalid());
    }
    let key = Zeroizing::new(decode(key.trim())?);
    AesGcm::<Aes256, U16>::new_from_slice(&key)
        .map_err(|_| invalid())?
        .decrypt_in_place_detached((&nonce).into(), &[], data.as_mut_slice(), (&tag).into())
        .map_err(|_| invalid())?;
    serde_json::from_slice(&data).map_err(|_| invalid())
}
pub(super) async fn read(probe: &Probe, version: Option<&str>) -> Result<AccountUsageSnapshot> {
    let base = billing::env(probe, "FACTORY_API_BASE_URL")
        .unwrap_or(API)
        .trim_end_matches('/');
    billing::validate_endpoint(base)?;
    let home = super::super::profile::path(probe, "FACTORY_HOME_OVERRIDE")
        .or_else(|| super::super::profile::home(probe))
        .ok_or_else(|| protocol("Factory home is unavailable"))?;
    let dir = home.join(".factory");
    let (token, eu, org) = if let Some(key) = billing::env(probe, "FACTORY_API_KEY") {
        let seed = billing::file(dir.join("api-key-identity/seed.json"))
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or_default();
        let eu = seed["version"] == 2
            && seed["apiBaseUrl"] == base
            && seed["credentialId"] == format!("{:x}", Sha256::digest(key))
            && seed["routing"]["region"] == "eu";
        (Zeroizing::new(key.to_owned()), eu, None)
    } else {
        let disabled = billing::env(probe, "FACTORY_DISABLE_KEYRING")
            .is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"));
        if !disabled
            && (tokio::fs::try_exists(dir.join("auth.v2.keyring"))
                .await
                .unwrap_or(false)
                || tokio::fs::try_exists(dir.join("auth.v2.loginkeychain"))
                    .await
                    .unwrap_or(false))
        {
            return Ok(unsupported(UnsupportedReason::UnsupportedAuthStorage));
        }
        let Some(file) = billing::file(dir.join("auth.v2.file")).await? else {
            return Ok(billing::reauth(ReauthReason::CredentialsMissing));
        };
        let Some(key) = billing::file(dir.join("auth.v2.key"))
            .await?
            .map(Zeroizing::new)
        else {
            return Ok(billing::reauth(ReauthReason::CredentialsMissing));
        };
        let login = stored_login(&file, &key)?;
        let eu = login
            .whoami
            .and_then(|r| r.region)
            .or(login.region)
            .as_deref()
            == Some("eu");
        (
            Zeroizing::new(login.access_token),
            eu,
            login.active_organization_id,
        )
    };
    let base = if eu {
        billing::env(probe, "FACTORY_API_BASE_URL_EU")
            .unwrap_or(EU_API)
            .trim_end_matches('/')
    } else {
        base
    };
    billing::validate_endpoint(base)?;
    let mut request = billing::get(
        &billing::client()?,
        base,
        "api/billing/limits",
        token.trim(),
    )?
    .header("x-factory-client", "cli");
    if let Some(org) = org.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        request = request.header("x-factory-org-id", billing::valid_header(org)?);
    }
    if let Some(version) = version {
        request = request.header("x-client-version", version);
    }
    billing::json_response(request)
        .await?
        .project(|v| project(&v, jiff::Timestamp::now()))
}
