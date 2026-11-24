//! Provider management through the real OpenCode 2 service, using disposable homes.
use kar::{
    InstallationOptions, Result,
    account::LoginCancellation,
    providers::{Method, Prompt, ProviderAuthOptions, ProviderEvent, ProviderInteraction},
};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    fs,
    time::Duration,
};
struct Input(VecDeque<&'static str>);
struct Browser {
    cancel: LoginCancellation,
    shown: bool,
}
impl ProviderInteraction for Browser {
    fn event(&mut self, event: ProviderEvent) -> Result<()> {
        let ProviderEvent::AuthUrl { url, .. } = event else {
            panic!("expected browser sign-in")
        };
        let target = reqwest::Url::parse(&url).unwrap();
        assert!(target.query_pairs().any(|(k, v)| {
            matches!(k.as_ref(), "redirect" | "redirect_uri")
                && reqwest::Url::parse(&v)
                    .is_ok_and(|u| matches!(u.host_str(), Some("localhost" | "127.0.0.1")))
        }));
        self.shown = true;
        self.cancel.cancel();
        Ok(())
    }
    async fn prompt(&mut self, _: Prompt) -> Result<String> {
        panic!("unexpected browser login prompt")
    }
}
impl ProviderInteraction for Input {
    fn event(&mut self, _: ProviderEvent) -> Result<()> {
        panic!("unexpected API-key event")
    }
    async fn prompt(&mut self, p: Prompt) -> Result<String> {
        let s = self.0.pop_front().expect("unexpected prompt");
        if s == "local-private-key" {
            assert!(matches!(p, Prompt::Secret { .. }));
        } else {
            assert!(matches!(p, Prompt::Text { .. }));
        }
        Ok(s.into())
    }
}
#[tokio::test]
#[ignore = "requires KAR_OPENCODE_BIN with OpenCode 2 integrations"]
async fn real_provider_registry_key_replacement_forms_and_logout() {
    let bin = std::env::var_os("KAR_OPENCODE_BIN").expect("set KAR_OPENCODE_BIN");
    let dir = tempfile::tempdir().unwrap();
    let mut env: BTreeMap<OsString, Option<OsString>> =
        std::env::vars_os().map(|(k, _)| (k, None)).collect();
    for n in [
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TEMP",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        env.insert(n.into(), Some(dir.path().into()));
    }
    for name in [
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        let p = dir.path().join(name);
        fs::create_dir_all(&p).unwrap();
        env.insert(name.into(), Some(p.into_os_string()));
    }
    for name in ["PATH", "SystemRoot"] {
        env.insert(name.into(), std::env::var_os(name));
    }
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        env.insert(name.into(), Some("http://127.0.0.1:1".into()));
    }
    env.insert("KAR_OPENCODE_BIN".into(), Some(bin));
    env.insert(
        "ANTHROPIC_API_KEY".into(),
        Some("private-environment-fixture".into()),
    );
    let opts = ProviderAuthOptions {
        installation: InstallationOptions {
            cwd: Some(dir.path().into()),
            env,
        },
        idle_timeout: Duration::from_millis(50),
        ..Default::default()
    };
    let rt = kar::runtime("opencode").unwrap();
    let auth = rt.opencode_provider_auth(opts.clone()).await.unwrap();
    for refuse_loopback in [false, true] {
        let scoped = rt
            .opencode_provider_auth(ProviderAuthOptions {
                refuse_loopback,
                ..opts.clone()
            })
            .await
            .unwrap();
        let cancel = LoginCancellation::default();
        let mut browser = Browser {
            cancel: cancel.clone(),
            shown: false,
        };
        let r = scoped
            .login("digitalocean", Method::Oauth, &mut browser, &cancel)
            .await;
        let Err(kar::Error::Provider(error)) = r else {
            panic!("expected a cancelled or loopback-only sign-in");
        };
        use kar::providers::ProviderErrorKind;
        assert_eq!(
            error.kind,
            if refuse_loopback {
                ProviderErrorKind::LoopbackRequired
            } else {
                ProviderErrorKind::Cancelled
            }
        );
        assert_eq!(browser.shown, !refuse_loopback);
        assert!(!scoped.status("digitalocean").await.unwrap().configured);
        scoped.close().await;
    }
    let menu = auth.login_providers();
    let ai = menu.iter().find(|p| p.provider_id == "openai").unwrap();
    assert_eq!(ai.methods[0].method, Method::Oauth);
    assert!(
        ai.methods
            .iter()
            .any(|m| m.method == Method::ApiKey && m.ambient)
    );
    assert!(auth.list_providers().await.unwrap().is_empty());
    let (env, missing) = tokio::join!(auth.status("anthropic"), auth.status("absent/encoded"));
    assert_eq!(env.unwrap().label.as_deref(), Some("ANTHROPIC_API_KEY"));
    assert!(!missing.unwrap().configured);
    auth.set_api_key("anthropic", "fixture-first")
        .await
        .unwrap();
    auth.set_api_key("anthropic", "fixture-second")
        .await
        .unwrap();
    assert_eq!(auth.list_providers().await.unwrap().len(), 1);
    let form_id = "cloudflare-workers-ai";
    assert!(matches!(
        auth.set_api_key(form_id, "unused").await,
        Err(kar::Error::Provider(kar::providers::ProviderError {
            kind: kar::providers::ProviderErrorKind::MethodUnavailable,
            ..
        }))
    ));
    assert!(!auth.status(form_id).await.unwrap().configured);
    let mut input = Input(["local-private-key", "fixture-account"].into());
    let connected = auth
        .login(
            form_id,
            Method::ApiKey,
            &mut input,
            &LoginCancellation::default(),
        )
        .await
        .unwrap();
    assert!(input.0.is_empty());
    assert!(connected.configured);
    assert_eq!(connected.method, Some(Method::ApiKey));
    auth.close().await;
    let reopened = rt.opencode_provider_auth(opts).await.unwrap();
    let rows = reopened.list_providers().await.unwrap();
    assert_eq!(rows.len(), 2);
    let labels = serde_json::to_string(&rows).unwrap();
    for secret in [
        "fixture-first",
        "fixture-second",
        "local-private-key",
        "private-environment-fixture",
    ] {
        assert!(!labels.contains(secret));
    }
    reopened.logout(form_id).await.unwrap();
    reopened.logout("anthropic").await.unwrap();
    assert!(auth.list_providers().await.unwrap().is_empty());
    assert_eq!(
        reopened.status("anthropic").await.unwrap().label.as_deref(),
        Some("ANTHROPIC_API_KEY")
    );
    reopened.close().await;
    auth.close().await;
}
