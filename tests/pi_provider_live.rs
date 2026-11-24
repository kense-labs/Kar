//! Pi native resolution and logout against an isolated auth.json.
use kar::{
    InstallationOptions, Result,
    account::LoginCancellation,
    providers::{Method, Prompt, ProviderAuthOptions, ProviderEvent, ProviderInteraction},
};
use serde_json::json;
use std::{collections::BTreeMap, ffi::OsString, fs, time::Duration};
#[path = "support/pi_model_catalog.rs"]
mod model_catalog;
fn options(home: &tempfile::TempDir) -> ProviderAuthOptions {
    let agent = home.path().join("agent");
    let mut env: BTreeMap<OsString, Option<OsString>> =
        std::env::vars_os().map(|(k, _)| (k, None)).collect();
    for k in [
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        env.insert(k.into(), Some(home.path().into()));
    }
    for k in ["KAR_PI_BIN", "KAR_PI_SDK_DIR", "PATH", "SystemRoot"] {
        env.insert(k.into(), std::env::var_os(k));
    }
    env.insert("KAR_PI_AGENT_DIR".into(), Some(agent.as_os_str().into()));
    env.insert(
        "PI_CODING_AGENT_DIR".into(),
        Some(home.path().join("other-profile").into()),
    );
    env.insert(
        "OPENAI_API_KEY".into(),
        Some("private-environment-only".into()),
    );
    ProviderAuthOptions {
        installation: InstallationOptions {
            cwd: Some(home.path().into()),
            env,
        },
        timeout: Duration::from_secs(10),
        login_timeout: Duration::from_secs(25),
        ..Default::default()
    }
}

fn alternate_paths(auth: &str, models: &str) -> kar::providers::PiModelRuntimePaths {
    kar::providers::PiModelRuntimePaths {
        auth_path: Some(auth.into()),
        models_path: kar::providers::PiModelsPath::File(models.into()),
    }
}

#[tokio::test]
#[ignore = "requires npm Pi with a native SDK host"]
async fn sdk_independent_configuration_files() -> Result<()> {
    use kar::providers::{PiModelRuntimePaths, PiModelsPath};
    let home = tempfile::tempdir()?;
    let agent = home.path().join("agent");
    fs::create_dir(&agent)?;
    let default_auth = r#"{"anthropic":{"type":"api_key","key":"default-fixture"}}"#;
    fs::write(agent.join("auth.json"), default_auth)?;
    let model = |id: &str| {
        json!({"providers": {id: {
            "baseUrl": "http://127.0.0.1:1", "api": "openai-completions",
            "models": [{"id":"fixture","name":"Fixture"}]
        }}})
    };
    fs::write(agent.join("models.json"), model("original").to_string())?;
    fs::create_dir(home.path().join("registries"))?;
    fs::write(
        home.path().join("registries/team.json"),
        model("team").to_string(),
    )?;
    let rt = kar::runtime("pi")?;
    let a = rt
        .pi_provider_auth_with_paths(
            options(&home),
            alternate_paths("keys/one.json", "registries/team.json"),
        )
        .await?;
    let b = rt
        .pi_provider_auth_with_paths(
            options(&home),
            alternate_paths("keys/two.json", "registries/team.json"),
        )
        .await?;
    let menu = a.login_providers().await?;
    assert!(menu.iter().any(|p| p.provider_id == "team"));
    assert!(menu.iter().all(|p| p.provider_id != "original"));
    assert!(a.list_providers(false).await?.is_empty());
    assert!(!home.path().join("keys/one.json").exists());
    let (_, _) = tokio::try_join!(
        a.set_api_key("team", "fixture-one"),
        b.set_api_key("team", "fixture-two")
    )?;
    assert!(a.list_providers(true).await?[0].configured);
    let no_models = rt
        .pi_provider_auth_with_paths(
            options(&home),
            PiModelRuntimePaths {
                auth_path: Some(
                    url::Url::from_file_path(home.path().join("keys/one.json"))
                        .unwrap()
                        .as_str()
                        .into(),
                ),
                models_path: PiModelsPath::Disabled,
            },
        )
        .await?;
    assert!(
        no_models
            .login_providers()
            .await?
            .iter()
            .all(|p| !matches!(p.provider_id.as_str(), "team" | "original"))
    );
    assert!(!no_models.status("team").await?.configured);
    assert_eq!(
        no_models.list_providers(false).await?[0].provider_id,
        "team"
    );

    // A fresh process must resolve relative filenames against --cwd.
    let child_cwd = tempfile::tempdir()?;
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kar"));
    command.current_dir(child_cwd.path()).env_clear();
    for (k, v) in options(&home).installation.env {
        if let Some(v) = v {
            command.env(k, v);
        }
    }
    let output = command
        .args(["providers", "pi", "--cwd"])
        .arg(home.path())
        .args([
            "--auth-path",
            "keys/one.json",
            "--models-path",
            "registries/team.json",
            "logout",
            "team",
        ])
        .output()?;
    assert!(output.status.success());
    assert!(a.list_providers(false).await?.is_empty());
    assert!(b.status("team").await?.configured);
    let key: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("keys/two.json"))?)?;
    assert_eq!(key["team"]["key"], "fixture-two");
    assert_eq!(fs::read_to_string(agent.join("auth.json"))?, default_auth);
    assert!(!child_cwd.path().join("keys").exists());
    assert!(!home.path().join("keys/models.json").exists());
    Ok(())
}

#[tokio::test]
#[ignore = "requires the standalone Pi distribution without an SDK"]
async fn standalone_independent_files_never_target_the_default_profile() -> Result<()> {
    use kar::{Error, providers::ProviderErrorKind};
    let home = tempfile::tempdir()?;
    let agent = home.path().join("agent");
    fs::create_dir(&agent)?;
    let marker = r#"{"anthropic":{"type":"api_key","key":"default-fixture"}}"#;
    fs::write(agent.join("auth.json"), marker)?;
    let path = home.path().join("separate.json");
    fs::write(&path, marker)?;
    let mut opts = options(&home);
    opts.installation.env.insert("KAR_PI_SDK_DIR".into(), None);
    let pi = kar::runtime("pi")?
        .pi_provider_auth_with_paths(opts, alternate_paths("separate.json", "elsewhere.json"))
        .await?;
    for attempt in [
        pi.set_api_key("anthropic", "other").await,
        pi.logout("anthropic").await,
    ] {
        assert!(
            matches!(attempt,Err(Error::Provider(e)) if e.kind==ProviderErrorKind::UnsupportedInstallation)
        );
    }
    assert_eq!(fs::read_to_string(agent.join("auth.json"))?, marker);
    assert_eq!(fs::read_to_string(path)?, marker);
    Ok(())
}
#[tokio::test]
#[ignore = "requires standalone Pi 1.6 and its matching KAR_PI_SDK_DIR"]
async fn unusable_companion_leaves_credentials_intact() -> Result<()> {
    use kar::providers::ProviderErrorKind;
    let home = tempfile::tempdir()?;
    let agent = home.path().join("agent");
    fs::create_dir(&agent)?;
    let auth = agent.join("auth.json");
    let saved = r#"{"anthropic":{"type":"api_key","key":"before"}}"#;
    fs::write(&auth, saved)?;
    let old = home.path().join("old");
    fs::create_dir_all(old.join("dist/core"))?;
    for path in ["dist/index.js", "dist/cli.js", "dist/core/agent-session.js"] {
        fs::write(old.join(path), "")?;
    }
    fs::write(
        old.join("package.json"),
        r#"{"name":"@earendil-works/pi-coding-agent","version":"1.4.0","bin":{"pi":"dist/cli.js"}}"#,
    )?;
    let paired = std::env::var_os("KAR_PI_SDK_DIR").expect("official matching companion");
    for (sdk, opts, expected) in [
        (
            old.into_os_string(),
            None,
            ProviderErrorKind::VersionUnsupported,
        ),
        (
            home.path().join("absent").into_os_string(),
            None,
            ProviderErrorKind::UnsupportedInstallation,
        ),
        (
            paired,
            Some("--no-addons".into()),
            ProviderErrorKind::UnsupportedInstallation,
        ),
    ] {
        let mut cfg = options(&home);
        cfg.installation
            .env
            .insert("KAR_PI_SDK_DIR".into(), Some(sdk));
        cfg.installation.env.insert("NODE_OPTIONS".into(), opts);
        let pi = kar::runtime("pi")?.pi_provider_auth(cfg).await?;
        for result in [
            pi.set_api_key("anthropic", "changed").await,
            pi.logout("anthropic").await,
        ] {
            assert!(matches!(result, Err(kar::Error::Provider(e)) if e.kind==expected));
        }
        assert_eq!(fs::read_to_string(&auth)?, saved);
    }
    Ok(())
}
#[tokio::test]
#[ignore = "requires KAR_PI_BIN with auth check --json"]
async fn pi_metadata_resolution_and_native_logout() {
    let home = tempfile::tempdir().unwrap();
    let agent = home.path().join("agent");
    fs::create_dir(&agent).unwrap();
    let options = options(&home);
    let pi = kar::runtime("pi")
        .unwrap()
        .pi_provider_auth(options)
        .await
        .unwrap();
    assert!(pi.list_providers(false).await.unwrap().is_empty());
    assert!(!agent.join("auth.json").exists());
    let token = "fixture-placeholder";
    let contents = json!({"anthropic":{"type":"api_key","key":token},"custom-acme":{"type":"api_key","key":"$UNKNOWN_LOCAL_VAR"}});
    fs::write(agent.join("auth.json"), contents.to_string()).unwrap();
    let meta = pi.list_providers(false).await.unwrap();
    assert_eq!(meta.len(), 2);
    assert!(
        meta.iter()
            .all(|s| s.method == Some(Method::ApiKey) && s.configured)
    );
    assert!(pi.status("openai").await.unwrap().configured);
    let resolved = pi.list_providers(true).await.unwrap();
    assert!(
        resolved
            .iter()
            .find(|s| s.provider_id == "anthropic")
            .unwrap()
            .configured
    );
    assert!(
        !resolved
            .iter()
            .find(|s| s.provider_id == "custom-acme")
            .unwrap()
            .configured
    );
    for s in [token, "private-environment-only", "$UNKNOWN_LOCAL_VAR"] {
        assert!(!serde_json::to_string(&resolved).unwrap().contains(s));
    }
    pi.logout("anthropic").await.unwrap();
    assert!(!pi.status("anthropic").await.unwrap().configured);
    pi.logout("anthropic").await.unwrap();
    assert_eq!(
        pi.list_providers(false).await.unwrap()[0].provider_id,
        "custom-acme"
    );
    pi.logout("custom-acme").await.unwrap();
    assert!(pi.list_providers(false).await.unwrap().is_empty());
}

struct Form {
    answers: std::collections::VecDeque<&'static str>,
    prompts: Vec<serde_json::Value>,
    events: Vec<serde_json::Value>,
}
impl Form {
    fn new(values: &[&'static str]) -> Self {
        Self {
            answers: values.iter().copied().collect(),
            prompts: vec![],
            events: vec![],
        }
    }
}
impl ProviderInteraction for Form {
    fn event(&mut self, event: ProviderEvent) -> Result<()> {
        self.events.push(serde_json::to_value(event)?);
        Ok(())
    }
    async fn prompt(&mut self, p: Prompt) -> Result<String> {
        self.prompts.push(serde_json::to_value(p)?);
        Ok(self.answers.pop_front().expect("unexpected prompt").into())
    }
}

#[tokio::test]
#[ignore = "requires native Pi 1.6 provider CLI"]
async fn native_login_registry_and_keys() -> Result<()> {
    let home = tempfile::tempdir()?;
    let agent = home.path().join("agent");
    fs::create_dir(&agent)?;
    let opts = options(&home);
    let pi = kar::runtime("pi")?.pi_provider_auth(opts.clone()).await?;
    let builtin = pi.login_providers().await?;
    assert_eq!(builtin.len(), 43);
    let openai = builtin.iter().find(|p| p.provider_id == "openai").unwrap();
    assert_eq!(
        openai.methods[0].login_label.as_deref(),
        Some("Sign in with ChatGPT")
    );
    assert_eq!(openai.methods[0].subscription, Some(true));
    assert_eq!(openai.methods[1].method, Method::ApiKey);
    assert!(!agent.join("auth.json").exists());
    let path = agent.join("models.json");
    fs::write(
        &path,
        r#"{// custom provider
        "providers":{"local-test":{"name":"A local test","baseUrl":"http://127.0.0.1:1","api":"openai-completions","models":[{"id":"local","name":"Local"}]},"anthropic":{"name":"Renamed","baseUrl":"http://127.0.0.1:1"}}
    }"#,
    )?;
    let menu = pi.login_providers().await?;
    assert_eq!(menu.len(), 44);
    assert_eq!(menu[0].provider_id, "local-test");
    assert_eq!(
        menu.iter()
            .find(|p| p.provider_id == "anthropic")
            .unwrap()
            .name,
        "Renamed"
    );
    let key = "!literal-$FIXTURE";
    pi.set_api_key("local-test", key).await?;
    assert_eq!(pi.status("local-test").await?.method, Some(Method::ApiKey));
    let auth = agent.join("auth.json");
    let data: serde_json::Value = serde_json::from_slice(&fs::read(&auth)?)?;
    assert_eq!(data["local-test"]["key"], "$!literal-$$FIXTURE");
    let mut native = std::process::Command::new(std::env::var_os("KAR_PI_BIN").unwrap());
    native
        .args(["auth", "print-api-key", "--provider", "local-test"])
        .current_dir(home.path())
        .env_clear();
    for (k, v) in &opts.installation.env {
        if let Some(v) = v {
            native.env(k, v);
        }
    }
    native.env("PI_CODING_AGENT_DIR", &agent);
    let resolved = native.output()?;
    assert!(
        resolved.status.success() && String::from_utf8_lossy(&resolved.stdout).trim_end() == key
    );
    let before = fs::read(&auth)?;
    for id in [
        "openai-codex",
        "cloudflare-ai-gateway",
        "amazon-bedrock",
        "unknown-pi",
    ] {
        assert!(pi.set_api_key(id, "never-stored").await.is_err());
        assert_eq!(fs::read(&auth)?, before);
    }
    let mut cloud = Form::new(&["private-cloud-key", "account-id", "gateway-id"]);
    assert!(
        pi.login(
            "cloudflare-ai-gateway",
            Method::ApiKey,
            &mut cloud,
            &LoginCancellation::default()
        )
        .await?
        .configured
    );
    assert!(cloud.answers.is_empty());
    assert_eq!(
        cloud
            .prompts
            .iter()
            .map(|p| p["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["secret", "text", "text"]
    );
    let mut aws = Form::new(&["aws-profile", "fixture-profile"]);
    assert!(
        pi.login(
            "amazon-bedrock",
            Method::ApiKey,
            &mut aws,
            &LoginCancellation::default()
        )
        .await?
        .configured
    );
    assert_eq!(aws.prompts[0]["options"][1]["id"], "aws-profile");
    let mut vertex = Form::new(&["api-key", "private-google-key"]);
    assert!(
        pi.login(
            "google-vertex",
            Method::ApiKey,
            &mut vertex,
            &LoginCancellation::default()
        )
        .await?
        .configured
    );
    assert_eq!(vertex.prompts[0]["options"][2]["id"], "service-account");
    let data: serde_json::Value = serde_json::from_slice(&fs::read(&auth)?)?;
    assert_eq!(
        data["amazon-bedrock"]["env"]["AWS_PROFILE"],
        "fixture-profile"
    );
    assert_eq!(
        data["cloudflare-ai-gateway"]["env"]["CLOUDFLARE_GATEWAY_ID"],
        "gateway-id"
    );
    assert!(!home.path().join("other-profile/auth.json").exists());
    Ok(())
}

#[tokio::test]
#[ignore = "requires native Pi 1.6 and uses a loopback Radius device flow"]
async fn native_oauth_device() -> Result<()> {
    use axum::{Json, Router, routing::post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let uri = format!("{base}/verify");
    let check_uri = uri.clone();
    let server=Router::new().route("/v1/oauth/device",post(async move||Json(json!({"user_code":"LOCAL123","device_code":"fixture-device-secret","verification_uri":uri,"expires_in":60,"interval":1}))))
        .route("/v1/oauth/token",post(async |body:String| {
            assert!(body.contains("device_code=fixture-device-secret"));
            Json(json!({"access_token":"fixture-access-private","refresh_token":"fixture-refresh-private","expires_in":7200,"scope":"gateway"}))
        }));
    tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
    let home = tempfile::tempdir()?;
    let agent = home.path().join("agent");
    fs::create_dir(&agent)?;
    fs::write(
        agent.join("models.json"),
        serde_json::to_vec(
            &json!({"providers":{"fixture-radius":{"name":"Fixture Radius","oauth":"radius","baseUrl":format!("{base}/v1")}}}),
        )?,
    )?;
    let pi = kar::runtime("pi")?.pi_provider_auth(options(&home)).await?;
    let menu = pi.login_providers().await?;
    let radius = menu
        .iter()
        .find(|p| p.provider_id == "fixture-radius")
        .unwrap();
    assert_eq!(radius.methods[0].subscription, Some(false));
    // The SDK offers both flows. The standalone CLI selects device-code
    // directly when invoked with --no-browser.
    let mut host = Form::new(&["device-code"]);
    let status = pi
        .login(
            "fixture-radius",
            Method::Oauth,
            &mut host,
            &LoginCancellation::default(),
        )
        .await?;
    assert_eq!(status.method, Some(Method::Oauth));
    assert_eq!(status.subscription, Some(false));
    assert_eq!(pi.list_providers(false).await?[0].subscription, Some(false));
    assert!(host.events.iter().any(|e| e["kind"] == "device_code"
        && e["userCode"] == "LOCAL123"
        && e["verificationUri"] == check_uri));
    assert!(!serde_json::to_string(&host.events)?.contains("fixture-device-secret"));
    if !host.prompts.is_empty() {
        assert!(host.answers.is_empty());
        assert!(
            host.events
                .iter()
                .any(|e| e["intervalSeconds"] == 1 && e["expiresInSeconds"] == 60)
        );
    }
    pi.logout("fixture-radius").await?;
    assert!(!pi.status("fixture-radius").await?.configured);
    Ok(())
}

#[tokio::test]
#[ignore = "requires native Pi 1.6, with a local callback and no external token exchange"]
async fn cancels_browser_prompt_and_times_out_key_prompt() -> Result<()> {
    use kar::{Error, providers::ProviderErrorKind};
    struct Pending {
        cancel: Option<LoginCancellation>,
        callback: Option<std::net::SocketAddr>,
        shown: bool,
    }
    impl ProviderInteraction for Pending {
        fn event(&mut self, event: ProviderEvent) -> Result<()> {
            if let ProviderEvent::AuthUrl { url, .. } = event {
                let url = reqwest::Url::parse(&url).unwrap();
                let callback = url
                    .query_pairs()
                    .find(|(k, _)| k == "callback_url")
                    .unwrap()
                    .1;
                let url = reqwest::Url::parse(&callback).unwrap();
                self.callback = Some(([127, 0, 0, 1], url.port().unwrap()).into());
            }
            Ok(())
        }
        async fn prompt(&mut self, prompt: Prompt) -> Result<String> {
            self.shown = true;
            if let Some(cancel) = &self.cancel {
                assert!(matches!(
                    prompt,
                    Prompt::ManualCode {
                        placeholder: Some(_),
                        ..
                    }
                ));
                assert!(self.callback.is_some());
                cancel.cancel();
            } else {
                assert!(matches!(prompt, Prompt::Secret { .. }));
            }
            std::future::pending().await
        }
    }
    let home = tempfile::tempdir()?;
    let cancel = LoginCancellation::default();
    let mut host = Pending {
        cancel: Some(cancel.clone()),
        callback: None,
        shown: false,
    };
    let pi = kar::runtime("pi")?.pi_provider_auth(options(&home)).await?;
    let result = pi
        .login("openrouter", Method::Oauth, &mut host, &cancel)
        .await;
    assert!(matches!(result,Err(Error::Provider(e)) if e.kind==ProviderErrorKind::Cancelled));
    assert!(host.shown);
    let address = host.callback.unwrap();
    // Cancellation also tears down Pi's native callback listener.
    tokio::time::timeout(Duration::from_secs(3), async {
        while tokio::net::TcpStream::connect(address).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    let mut opts = options(&home);
    opts.login_timeout = Duration::from_secs(2);
    let pi = kar::runtime("pi")?.pi_provider_auth(opts).await?;
    let mut host = Pending {
        cancel: None,
        callback: None,
        shown: false,
    };
    let result = pi
        .login(
            "anthropic",
            Method::ApiKey,
            &mut host,
            &LoginCancellation::default(),
        )
        .await;
    assert!(matches!(result,Err(Error::Provider(e)) if e.kind==ProviderErrorKind::TimedOut));
    assert!(host.shown && pi.list_providers(false).await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires Pi 1.6; exercises ChatGPT identity on its native loopback port"]
async fn host_identity_is_lazy_and_reaches_chatgpt() -> Result<()> {
    use kar::{
        Error,
        providers::{PiLoginOptions, ProviderErrorKind},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Browser(LoginCancellation, Option<reqwest::Url>);
    impl ProviderInteraction for Browser {
        fn event(&mut self, event: ProviderEvent) -> Result<()> {
            if let ProviderEvent::AuthUrl { url, .. } = event {
                self.1 = Some(reqwest::Url::parse(&url).unwrap());
                self.0.cancel();
            }
            Ok(())
        }
        async fn prompt(&mut self, _: Prompt) -> Result<String> {
            std::future::pending().await
        }
    }

    let home = tempfile::tempdir()?;
    let pi = kar::runtime("pi")?.pi_provider_auth(options(&home)).await?;
    let calls = AtomicUsize::new(0);
    let id = uuid::Uuid::new_v4();
    let get_id = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(id)
    };
    let options = PiLoginOptions {
        agent_name: Some("fixture host"),
        get_device_id: Some(&get_id),
    };
    let cancel = LoginCancellation::default();
    let result = pi
        .login_with_options(
            "anthropic",
            Method::ApiKey,
            &mut Form::new(&["fixture"]),
            &cancel,
            &options,
        )
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    if matches!(result, Err(Error::Provider(ref e)) if e.kind == ProviderErrorKind::UnsupportedInstallation)
    {
        // Standalone Pi cannot accept an installation identity.
        assert!(pi.list_providers(false).await?.is_empty());
        return Ok(());
    }
    assert!(result?.configured);
    let mut browser = Browser(cancel.clone(), None);
    let result = pi
        .login_with_options("openai", Method::Oauth, &mut browser, &cancel, &options)
        .await;
    assert!(matches!(result, Err(Error::Provider(e)) if e.kind == ProviderErrorKind::Cancelled));
    let url = browser.1.unwrap();
    let params: BTreeMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["agent_name_hint"], "fixture host");
    assert_eq!(params["ext_agent_host_id"], format!("urn:uuid:{id}"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(pi.list_providers(false).await?.len(), 1);
    let redirect = reqwest::Url::parse(&params["redirect_uri"]).unwrap();
    assert!(
        tokio::net::TcpStream::connect((redirect.host_str().unwrap(), redirect.port().unwrap()))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires native Pi 1.6 provider CLI"]
async fn product_cli_roundtrip() -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let home = tempfile::tempdir()?;
    let opts = options(&home);
    let invoke = |args: &[&str], input: &str| -> serde_json::Value {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
        cmd.args(["providers", "pi"])
            .args(args)
            .env_clear()
            .current_dir(home.path());
        for (name, value) in &opts.installation.env {
            if let Some(value) = value {
                cmd.env(name, value);
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "Pi provider CLI refused {args:?}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("private-key")
                && !String::from_utf8_lossy(&out.stderr).contains("private-key")
        );
        serde_json::from_slice(
            out.stdout
                .split(|b| *b == b'\n')
                .rfind(|s| !s.is_empty())
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(invoke(&["menu"], "").as_array().unwrap().len(), 43);
    let key = format!("private-key-{}", "x".repeat(10000));
    assert_eq!(invoke(&["set-key", "anthropic"], &key)["stored"], true);
    let auth: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("agent/auth.json"))?)?;
    assert!(auth["anthropic"]["key"] == key);
    let done = invoke(
        &["login", "cloudflare-workers-ai", "api-key"],
        "\"private-key-cli\"\n\"fixture-account\"\n",
    );
    assert_eq!(done["status"]["configured"], true);
    assert_eq!(
        invoke(&["logout", "cloudflare-workers-ai"], "")["removed"],
        true
    );
    Ok(())
}
