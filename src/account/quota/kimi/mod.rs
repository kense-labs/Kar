mod projection;
use super::{billing, *};
use reqwest::Method;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use zeroize::Zeroizing;

const BASE: &str = "https://api.kimi.com/coding/v1";
const OAUTH: &str = "https://auth.kimi.com";

struct Config {
    base: String,
    home: PathBuf,
    credential: PathBuf,
}

fn config(probe: &Probe, provider: &Value) -> Result<Option<Config>> {
    let cfg = &provider["oauth"];
    let base_override = billing::env(probe, "KIMI_CODE_BASE_URL");
    let oauth_override = billing::env(probe, "KIMI_CODE_OAUTH_HOST")
        .or_else(|| billing::env(probe, "KIMI_OAUTH_HOST"));
    let configured_base = text(&provider["baseUrl"]);
    let configured_oauth = text(&cfg["oauthHost"]);
    let base = base_override
        .or(configured_base.as_deref())
        .unwrap_or(BASE)
        .trim_end_matches('/');
    let oauth = oauth_override
        .or(configured_oauth.as_deref())
        .unwrap_or(OAUTH)
        .trim_end_matches('/');
    billing::validate_endpoint(base)?;
    billing::validate_endpoint(oauth)?;
    let slot = if base == BASE && oauth == OAUTH {
        "kimi-code".into()
    } else {
        // The native slot uses JSON.stringify with this insertion order.
        let key = format!(
            "{{\"oauthHost\":{},\"baseUrl\":{}}}",
            serde_json::to_string(oauth)?,
            serde_json::to_string(base)?
        );
        let digest = format!("{:x}", Sha256::digest(key));
        format!("kimi-code-env-{}", &digest[..16])
    };
    if base_override.is_none()
        && oauth_override.is_none()
        && text(&cfg["key"]).as_deref() == Some(&format!("oauth/{slot}"))
        && text(&cfg["storage"]).is_some_and(|s| s != "file")
    {
        return Ok(None);
    }
    let home = super::super::profile::path(probe, "KIMI_CODE_HOME")
        .or_else(|| super::super::profile::home(probe).map(|p| p.join(".kimi-code")))
        .ok_or_else(|| protocol("Kimi home is unavailable"))?;
    let credential = home.join("credentials").join(format!("{slot}.json"));
    Ok(Some(Config {
        base: base.into(),
        home,
        credential,
    }))
}

#[derive(Deserialize)]
struct Stored {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_at: Option<f64>,
}
enum Token {
    Missing,
    Revoked,
    Active {
        access: Zeroizing<String>,
        refresh: Zeroizing<String>,
        expired: bool,
    },
}
async fn token(cfg: &Config) -> Result<Token> {
    let Some(raw) = billing::file(cfg.credential.clone())
        .await?
        .map(Zeroizing::new)
    else {
        return Ok(Token::Missing);
    };
    let v: Stored =
        serde_json::from_str(&raw).map_err(|_| protocol("Invalid Kimi account configuration"))?;
    if v.access_token.as_deref() == Some("")
        && v.refresh_token.as_deref() == Some("")
        && v.expires_at == Some(0.0)
    {
        return Ok(Token::Revoked);
    }
    let Some(access) = v.access_token.filter(|s| !s.trim().is_empty()) else {
        return Err(protocol("Kimi did not provide an access token"));
    };
    let expired = v
        .expires_at
        .is_some_and(|v| v <= jiff::Timestamp::now().as_millisecond() as f64 / 1000.0);
    Ok(Token::Active {
        access: Zeroizing::new(access),
        refresh: Zeroizing::new(v.refresh_token.unwrap_or_default()),
        expired,
    })
}

async fn read_direct(cfg: &Config, access: &str, until: Instant) -> Result<AccountUsageSnapshot> {
    let client = billing::client()?;
    let usage = billing::get(&client, &cfg.base, "usages", access)?;
    let profile = billing::get(&client, &cfg.base, "me", access)?;
    let profile_deadline = until
        .checked_sub(Duration::from_millis(100))
        .unwrap_or(until);
    let (usage, identity) = tokio::join!(
        billing::json_response(usage),
        tokio::time::timeout_at(profile_deadline, billing::json_response(profile))
    );
    let mut result = usage?.project(|v| projection::direct(&v))?;
    if let AccountUsageSnapshot::Available(q) = &mut result
        && let Ok(Ok(billing::Reply::Data(v))) = identity
        && text(&v["user_id"]).is_some()
    {
        q.email = text(&v["email"]);
        q.display_name = text(&v["nickname"]);
        q.plan = text(&v["user_level_name"]);
    }
    Ok(result)
}
async fn native(
    web: &crate::kimi::QueryWeb,
    cfg: &Config,
    until: Instant,
) -> Result<AccountUsageSnapshot> {
    let reply = web
        .request(Method::GET, "/oauth/usage", None)
        .await
        .map_err(|_| protocol("Kimi Web usage query failed"))?;
    if reply["kind"] == "error" {
        return match reply["status"].as_u64() {
            Some(401 | 403) => Ok(billing::reauth(ReauthReason::CredentialsRejected)),
            Some(404) => Ok(unsupported(UnsupportedReason::EndpointUnavailable)),
            _ => match token(cfg).await? {
                Token::Missing => Ok(billing::reauth(ReauthReason::CredentialsMissing)),
                Token::Revoked => Ok(billing::reauth(ReauthReason::CredentialsRejected)),
                _ => Err(protocol("Kimi native usage query failed")),
            },
        };
    }
    if reply["kind"] != "ok" || !reply["quota"].is_object() {
        return Err(protocol("Invalid Kimi Web usage response"));
    }
    let mut result = projection::native(&reply["quota"])?;
    if let AccountUsageSnapshot::Available(q) = &mut result
        && let Some(t) = until
            .checked_sub(Duration::from_millis(150))
            .filter(|t| *t > Instant::now())
        && let Ok(Ok(info)) =
            tokio::time::timeout_at(t, web.request(Method::GET, "/oauth/userinfo", None)).await
        && info["kind"] == "ok"
    {
        let user = &info["userInfo"];
        q.email = text(&user["email"]);
        q.display_name = text(&user["nickname"]);
        q.plan = text(&user["userLevelName"]);
    }
    Ok(result)
}

pub(super) async fn read(
    probe: &Probe,
    path: &Path,
    until: Instant,
) -> Result<AccountUsageSnapshot> {
    let list = output(Reader::Kimi, probe, path, &["provider", "list", "--json"]).await?;
    let providers: Value = serde_json::from_slice(&list.stdout).unwrap_or_default();
    let provider = &providers["providers"]["managed:kimi-code"];
    if !list.status.success() || provider["type"] != "kimi" {
        return Ok(unsupported(UnsupportedReason::AuthConfigurationUnavailable));
    }
    let Some(cfg) = config(probe, provider)? else {
        return Ok(unsupported(UnsupportedReason::UnsupportedAuthStorage));
    };
    let (access, refresh, expired) = match token(&cfg).await? {
        Token::Missing => return Ok(billing::reauth(ReauthReason::CredentialsMissing)),
        Token::Revoked => return Ok(billing::reauth(ReauthReason::CredentialsRejected)),
        Token::Active {
            access,
            refresh,
            expired,
        } => (access, refresh, expired),
    };
    if expired && refresh.trim().is_empty() {
        return Ok(billing::reauth(ReauthReason::CredentialsMissing));
    }
    let locked = cfg!(any(target_os = "linux", target_os = "macos"))
        && probe.get("KIMI_DISABLE_OAUTH_LOCK") != Some(std::ffi::OsStr::new("1"));
    let mut snapshot = if locked {
        let mut web = crate::kimi::QueryWeb::for_query(
            probe,
            path,
            &cfg.home,
            until.saturating_duration_since(Instant::now()),
        )
        .await?;
        let outcome = native(&web, &cfg, until).await;
        // Allow bounded graceful shutdown even when a later cancellation drops the query.
        let stop_by = until
            .checked_sub(Duration::from_millis(25))
            .unwrap_or(until);
        let _ = tokio::time::timeout_at(stop_by, web.stop()).await;
        outcome?
    } else if expired {
        AccountUsageSnapshot::Unavailable {
            reason: UnavailableReason::AccessTokenExpired,
            last_account: None,
        }
    } else {
        read_direct(&cfg, access.trim(), until).await?
    };
    let mut secrets = AccountSecrets(vec![access.trim()]);
    if !refresh.trim().is_empty() {
        secrets.0.push(refresh.trim());
    }
    snapshot.omit_secret_fields(&secrets);
    Ok(snapshot)
}
