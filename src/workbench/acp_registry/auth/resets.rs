//! Persist the owner's command before invoking a native operation that can spend a reset.
use super::*;
use crate::account::{
    AccountUsageOptions, AuthStatus, AuthStatusOptions, LimitResetInspectOptions,
    LimitResetOptions, LimitResetResult,
};
use crate::workbench::mcp_apps::record::{hash, read};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Redeem {
    request_id: Uuid,
    reset_id: Option<String>,
    #[serde(default)]
    five_hour_limit_consent: bool,
}
#[derive(Serialize, Deserialize)]
struct Claim {
    signature: String,
    account: Option<String>,
    result: Option<Value>,
}
fn settled(result: &Value) -> bool {
    result["report"]["kind"] != "unknown" || result["owner_acknowledged"] == true
}
async fn inspect(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, String)>) -> Reply {
    let (rt, installation, binding) = builtin::context(&app, p, &id)?;
    let _guard = app.0.chats.accounts.operation(binding)?;
    crate::workbench::chatgpt::native_quota(&app.0.chats, &id)?;
    let value = if rt.id == "claude" {
        // Legacy Claude can spend on entry. The native guard refuses that version here.
        serde_json::to_value(
            rt.inspect_limit_resets(&LimitResetInspectOptions {
                installation,
                include_five_hour: true,
                ..Default::default()
            })
            .await?,
        )
        .map_err(Error::from)?
    } else {
        serde_json::to_value(
            rt.account_usage(&AccountUsageOptions {
                installation,
                limit_reset_details: true,
                ..Default::default()
            })
            .await?,
        )
        .map_err(Error::from)?
    };
    Ok(Json(value))
}
async fn redeem(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    headers: HeaderMap,
    Json(q): Json<Redeem>,
) -> Reply {
    flow::client(&headers)?
        .ok_or_else(|| bad("Kar-Auth-Client is required to spend a reset credit"))?;
    if q.request_id.is_nil()
        || q.reset_id
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 200 || v.chars().any(char::is_control))
    {
        return Err(bad("invalid reset request or credit identifier").into());
    }
    let (rt, installation, binding) = builtin::context(&app, p, &id)?;
    if !matches!(rt.id.as_str(), "claude" | "codex") {
        return Err(Error::Unsupported(format!("quota reset for {id}")).into());
    }
    let profile = binding.fingerprint();
    let _guard = app.0.chats.accounts.operation(binding)?;
    crate::workbench::chatgpt::native_quota(&app.0.chats, &id)?;
    let dir = app.0.chats.directory().join("workbench/quota-resets");
    // Same login home through two configured instances maps to the same serialization point.
    let _profile_lock = lock_path(&dir.join("locks").join(&profile), true)?;
    let _command_lock = lock_path(&dir.join("commands").join(q.request_id.to_string()), true)?;
    let path = dir.join("receipts").join(q.request_id.to_string());
    let signature = hash(&json!([profile, q.reset_id, q.five_hour_limit_consent]));
    let previous = read::<Claim>(&path)?;
    if let Some(claim) = &previous {
        if claim.signature != signature {
            return Err(bad("request UUID already belongs to another credit or profile").into());
        }
        if let Some(result) = &claim.result {
            return Ok(Json(result.clone()));
        }
    }
    let pending = dir.join("pending").join(&profile);
    if let Some(other) = read::<Uuid>(&pending)?
        && other != q.request_id
    {
        let finished = read::<Claim>(&dir.join("receipts").join(other.to_string()))?
            .is_some_and(|c| c.result.as_ref().is_some_and(settled));
        if !finished {
            return Err(bad(&format!("an earlier quota claim is unconfirmed; retry request {other} before spending another credit")).into());
        }
    }
    let auth = rt
        .auth_status(&AuthStatusOptions {
            installation: installation.clone(),
            ..Default::default()
        })
        .await?;
    let mut account = match auth {
        AuthStatus::LoggedIn {
            account: Some(a), ..
        } => a.account_id.or(a.email).map(|s| hash(&json!(s))),
        _ => None,
    };
    if account.is_none() && rt.id == "codex" {
        // CLI login status can omit identity; app-server account/read supplies the ChatGPT email.
        if let Ok(crate::account::AccountUsageSnapshot::Available(usage)) = rt
            .account_usage(&AccountUsageOptions {
                installation: installation.clone(),
                limit_reset_details: false,
                ..Default::default()
            })
            .await
        {
            account = usage.email.map(|s| hash(&json!(s)));
        }
    }
    if let Some(claim) = &previous
        && (account.is_none() || claim.account != account)
    {
        return Err(bad("cannot retry this ambiguous spend against an unverified or changed account; check credit history on the original account").into());
    }
    let mut claim = Claim {
        signature,
        account,
        result: None,
    };
    record(&path, &claim)?;
    record(&pending, &q.request_id)?;
    let mut opts = LimitResetOptions::new(q.request_id.to_string());
    opts.installation = installation.clone();
    opts.reset_id = q.reset_id;
    opts.five_hour_limit_consent = q.five_hour_limit_consent;
    let report = rt.redeem_limit_reset_report(&opts).await?;
    let mut result = json!({"request_id":q.request_id,"report":report});
    // Persist the settled spend before a subsequent read, including when the client disconnects.
    claim.result = Some(result.clone());
    record(&path, &claim)?;
    if matches!(report.result, LimitResetResult::Reset) {
        if let Ok(fresh) = rt
            .account_usage(&AccountUsageOptions {
                installation,
                ..Default::default()
            })
            .await
        {
            result["usage"] = json!(fresh);
            claim.result = Some(result.clone());
            record(&path, &claim)?;
        }
    }
    Ok(Json(result))
}
// An owner who checked native account history can close an indeterminate action without issuing another spend.
async fn acknowledge(
    State(app): State<Workbench>,
    Param((p, id, request)): Param<(Uuid, String, Uuid)>,
    headers: HeaderMap,
    Json(ack): Json<Acknowledge>,
) -> Reply {
    flow::client(&headers)?
        .ok_or_else(|| bad("Kar-Auth-Client is required for quota acknowledgement"))?;
    if !ack.checked_native_history {
        return Err(
            bad("check native credit history before acknowledging an unknown outcome").into(),
        );
    }
    let (_, _, binding) = builtin::context(&app, p, &id)?;
    let profile = binding.fingerprint();
    let dir = app.0.chats.directory().join("workbench/quota-resets");
    let _lock = lock_path(&dir.join("locks").join(&profile), true)?;
    if read::<Uuid>(&dir.join("pending").join(profile))? != Some(request) {
        return Err(bad("no matching unresolved credit request on this profile").into());
    }
    let path = dir.join("receipts").join(request.to_string());
    let mut claim = read::<Claim>(&path)?.ok_or_else(|| bad("missing credit request"))?;
    if let Some(r) = &claim.result
        && settled(r)
    {
        return Ok(Json(r.clone()));
    }
    let result =
        json!({"request_id":request,"report":{"kind":"unknown"},"owner_acknowledged":true});
    claim.result = Some(result.clone());
    record(&path, &claim)?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledge {
    checked_native_history: bool,
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/accounts/{runtime}/quota/resets",
            post(inspect),
        )
        .route(
            "/projects/{project}/accounts/{runtime}/quota/resets/redeem",
            post(redeem),
        )
        .route(
            "/projects/{project}/accounts/{runtime}/quota/resets/{request}/acknowledge",
            post(acknowledge),
        )
}
