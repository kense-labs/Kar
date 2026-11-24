//! Qwen owns the test settings and selects user or workspace scope.
use kar::{
    Error, InstallationOptions,
    providers::{ProviderAuthOptions, QwenConnect, QwenProtocol, QwenScope, QwenWireApi},
};
use serde_json::Value;
use std::{collections::BTreeMap, ffi::OsString, fs};
#[tokio::test]
#[ignore = "requires OAR_QWEN_BIN with native qwen/providers/connect"]
async fn qwen_settings_scope_and_stored_key_reuse() {
    let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", model.local_addr().unwrap());
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().fallback(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { axum::http::StatusCode::UNAUTHORIZED }
        });
        axum::serve(model, app).await.unwrap()
    });
    for scope in [QwenScope::User, QwenScope::Workspace] {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path().join("workspace");
        let profile = d.path().join(".qwen");
        for p in [&cwd, &profile] {
            fs::create_dir(p).unwrap();
        }
        let secret = "private-fixture-scope-key";
        let mut env: BTreeMap<OsString, Option<OsString>> =
            std::env::vars_os().map(|(k, _)| (k, None)).collect();
        for k in [
            "HOME",
            "USERPROFILE",
            "TMPDIR",
            "TEMP",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ] {
            env.insert(k.into(), Some(d.path().as_os_str().into()));
        }
        for k in ["OAR_QWEN_BIN", "PATH", "SystemRoot"] {
            env.insert(k.into(), std::env::var_os(k));
        }
        env.insert("QWEN_HOME".into(), Some(profile.as_os_str().into()));
        env.insert(
            "QWEN_CODE_SYSTEM_SETTINGS_PATH".into(),
            Some(d.path().join("sys.json").into_os_string()),
        );
        env.insert(
            "QWEN_CODE_SYSTEM_DEFAULTS_PATH".into(),
            Some(d.path().join("defaults.json").into_os_string()),
        );
        env.insert("QWEN_CODE_TELEMETRY_ENABLED".into(), Some("false".into()));
        env.insert("DEEPSEEK_API_KEY".into(), Some("private-env-only".into()));
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            env.insert(name.into(), Some("http://127.0.0.1:1".into()));
        }
        env.insert("NO_PROXY".into(), Some("127.0.0.1,localhost".into()));
        let auth = kar::runtime("qwen")
            .unwrap()
            .qwen_provider_auth(ProviderAuthOptions {
                installation: InstallationOptions {
                    env,
                    cwd: Some(cwd.clone()),
                },
                ..Default::default()
            })
            .await
            .unwrap();
        let list = auth.list_providers().await.unwrap();
        assert!(
            list.iter()
                .find(|p| p.id == "deepseek")
                .unwrap()
                .existing_config
                .is_none()
        );
        let mut config = QwenConnect {
            provider_id: "custom-openai-compatible".into(),
            api_key: Some(secret.into()),
            base_url: Some(endpoint.clone()),
            protocol: Some(QwenProtocol::Openai),
            wire_api: Some(QwenWireApi::ChatCompletions),
            model_ids: Some(vec!["scope-model".into()]),
            scope: Some(scope),
        };
        let r = auth.connect(&config).await.unwrap();
        assert_eq!(r.model_id.as_deref(), Some("scope-model"));
        assert!(r.success);
        let list = auth.list_providers().await.unwrap();
        let visible = serde_json::to_string(&list).unwrap();
        assert!(!visible.contains(secret));
        assert!(!visible.contains("private-env-only"));
        let summary = list
            .iter()
            .find(|p| p.id == config.provider_id)
            .unwrap()
            .existing_config
            .as_ref()
            .unwrap();
        assert_eq!(summary["hasApiKey"], true);
        assert_eq!(summary["baseUrl"], endpoint);
        config.api_key = None;
        assert!(auth.connect(&config).await.unwrap().success);
        let contains = |p: &std::path::Path| {
            fs::read(p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| {
                    v["env"]
                        .as_object()
                        .map(|m| m.values().any(|k| k == secret))
                })
                .unwrap_or(false)
        };
        assert_eq!(
            contains(&profile.join("settings.json")),
            matches!(scope, QwenScope::User)
        );
        assert_eq!(
            contains(&cwd.join(".qwen/settings.json")),
            matches!(scope, QwenScope::Workspace)
        );
        config.provider_id = format!("unregistered-{secret}");
        config.api_key = Some(secret.into());
        let error = auth.connect(&config).await.unwrap_err();
        assert!(matches!(error, Error::ProviderRpc { code: -32602, .. }));
        assert!(!error.to_string().contains(secret));
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    server.abort();
}
