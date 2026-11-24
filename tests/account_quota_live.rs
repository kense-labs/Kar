//! Installed clients, synthetic accounts in temporary homes, and loopback billing.
#[path = "support/billing.rs"]
mod billing;
use billing::*;
use serde_json::json;
use std::{fs, path::PathBuf};

#[tokio::test]
#[ignore = "requires an installed KAR_FACTORY_BIN; synthetic Factory account and loopback only"]
async fn factory_limits_from_pinned_installation() {
    let exe = PathBuf::from(std::env::var_os("KAR_FACTORY_BIN").unwrap());
    let home = tempfile::tempdir().unwrap();
    let server = Server::start().await;
    let mut opts = options(home.path());
    set(&mut opts, "FACTORY_API_BASE_URL", &server.origin);
    assert_eq!(
        query("factory", &exe, &opts).await.unwrap()["reason"],
        "credentials_missing"
    );
    server.answer(200, factory_limits());
    set(&mut opts, "FACTORY_API_KEY", KEY);
    let result = query("factory", &exe, &opts).await.unwrap();
    assert_eq!(result["rateLimited"], true);
    assert_eq!(result["windows"][3]["scope"]["name"], "Droid Core");
    assert_eq!(
        server.seen.lock().unwrap().as_slice(),
        &[("/api/billing/limits".into(), true, false)]
    );
}

#[tokio::test]
#[ignore = "requires KAR_KIMI_BIN; managed Kimi provider with synthetic tokens and a loopback API"]
async fn managed_kimi_quota_in_an_isolated_profile() {
    let exe = PathBuf::from(std::env::var_os("KAR_KIMI_BIN").unwrap());
    let home = tempfile::tempdir().unwrap();
    let server = Server::start().await;
    let mut opts = options(home.path());
    let file = configure_kimi(home.path(), &server, &mut opts);
    let fresh = fs::read(&file).unwrap();
    server.answer(200, kimi_limits());
    server.identity(json!({"user_id":"42","nickname":"Sample","user_level_name":"Pro","memberships":[{"name":"Pro"}]}));
    set(&mut opts, "KIMI_DISABLE_OAUTH_LOCK", "1");
    let value = query("kimi", &exe, &opts).await.unwrap();
    assert_eq!(value["kind"], "available", "{value}");
    assert_eq!(value["windows"][1]["usedRatio"], 0.21);
    assert_eq!(value["displayName"], "Sample");
    assert_eq!(fs::read(&file).unwrap(), fresh);
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        let mut expired: serde_json::Value = serde_json::from_slice(&fresh).unwrap();
        expired["access_token"] = "expired-fixture".into();
        expired["expires_at"] = 1.into();
        fs::write(&file, expired.to_string()).unwrap();
        let n = server.seen.lock().unwrap().len();
        assert_eq!(
            query("kimi", &exe, &opts).await.unwrap()["reason"],
            "access_token_expired"
        );
        assert_eq!(server.seen.lock().unwrap().len(), n);
        set(&mut opts, "KIMI_DISABLE_OAUTH_LOCK", "0");
        let (a, b) = tokio::join!(query("kimi", &exe, &opts), query("kimi", &exe, &opts));
        let value = a.unwrap();
        let second = b.unwrap();
        assert_eq!(value["kind"], "available", "{value}");
        assert_eq!(second, value);
        assert_eq!(
            server.refreshes.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let stored: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert!(stored["refresh_token"] == "rotated-local-refresh");
        assert_eq!(value["windows"][0]["label"], "5h limit");
        assert_eq!(value["windows"][0]["usedRatio"], 0.21);
        assert!(
            value["windows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["label"] == "Weekly limit" && w["usedRatio"] == 1.0)
        );
    }
    assert!(server.seen.lock().unwrap().iter().all(|(_, auth, _)| *auth));
}
