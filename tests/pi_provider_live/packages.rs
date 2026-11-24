//! Installed Pi RPC + installed Pi SDK, using a private npm registry served on loopback.
use super::*;
use axum::{Json, Router, routing::get};
use std::{
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

const SOURCE: &str = "npm:kar-rpc-package@1.0.0";

async fn registry(root: &Path) -> (String, Arc<AtomicUsize>) {
    let package = root.join("package");
    fs::create_dir(&package).unwrap();
    let manifest = json!({"name":"kar-rpc-package","version":"1.0.0"});
    fs::write(package.join("package.json"), manifest.to_string()).unwrap();
    let tar = Command::new("tar")
        .args(["-czf", "-", "package"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(tar.status.success());
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let metadata = json!({"name":"kar-rpc-package","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"kar-rpc-package","version":"1.0.0","dist":{"tarball":format!("{url}/package.tgz")}}}});
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let api = Router::new()
        .route(
            "/kar-rpc-package",
            get(move || async move { Json(metadata) }),
        )
        .route("/package.tgz", get(move || async move { tar.stdout }))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                count.fetch_add(1, Ordering::SeqCst);
                async move { next.run(req).await }
            },
        ));
    tokio::spawn(async move { axum::serve(socket, api).await.unwrap() });
    (url, requests)
}

#[tokio::test]
#[ignore = "requires installed Pi 1.4+ with matching SDK, Node, npm and tar; no model turn, isolated loopback package registry"]
async fn native_rpc_package_preflight_and_install_opt_in() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let work = temp.path().join("work");
    let project = work.join(".pi");
    let agent = temp.path().join("agent");
    fs::create_dir_all(&project)?;
    fs::create_dir(&agent)?;
    let (registry, calls) = registry(temp.path()).await;
    let mut opts = kar::SessionOptions::new(&work);
    for (key, value) in options(&temp).installation.env {
        match value {
            Some(v) => {
                opts.env.insert(key, v);
            }
            None => {
                if let Some(k) = key.to_str() {
                    opts.remove_env(k)?;
                }
            }
        }
    }
    for (k, v) in [
        ("npm_config_registry", registry.as_str()),
        ("npm_config_audit", "false"),
        ("npm_config_fund", "false"),
        ("PI_SKIP_VERSION_CHECK", "1"),
    ] {
        opts.env.insert(k.into(), v.into());
    }
    opts.env.insert(
        "npm_config_cache".into(),
        temp.path().join("npm-cache").into(),
    );
    opts.env.insert(
        "npm_config_userconfig".into(),
        temp.path().join("npmrc").into(),
    );
    opts.env
        .insert("NO_PROXY".into(), "127.0.0.1,localhost".into());
    opts.model = Some("fixture/local".into());
    fs::write(agent.join("models.json"),json!({"providers":{"fixture":{"api":"openai-completions","baseUrl":format!("{registry}/model"),"apiKey":"local","models":[{"id":"local","name":"local"}]}}}).to_string())?;
    let packages = json!({"packages":[{"source":SOURCE}]}).to_string();
    let config = project.join("settings.json");
    let installed = project.join("npm/node_modules/kar-rpc-package/package.json");
    fs::write(&config, &packages)?;
    opts.settings.pi_project_trusted = Some(true);
    let rt = kar::runtime("pi")?;
    async fn blocked(rt: &kar::Runtime, opts: &kar::SessionOptions) -> Result<()> {
        match rt.open(opts.clone()).await {
            Err(kar::Error::Refused(reason))
                if reason.contains("project_packages_blocked") && reason.contains(SOURCE) =>
            {
                Ok(())
            }
            Ok(session) => {
                session.close().await?;
                panic!("RPC launched before checking its configured packages")
            }
            Err(e) => panic!("unexpected startup failure: {e}"),
        }
    }
    blocked(&rt, &opts).await?;
    assert!(!installed.exists());
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    opts.env.insert("PI_OFFLINE".into(), " true ".into());
    blocked(&rt, &opts).await?;
    opts.env.insert("PI_OFFLINE".into(), "YeS".into());
    rt.open(opts.clone()).await?.close().await?;
    assert!(!installed.exists());
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    opts.remove_env("PI_OFFLINE")?;
    fs::create_dir_all(installed.parent().unwrap())?;
    let wrong = json!({"name":"kar-rpc-package","version":"0.9.0"}).to_string();
    fs::write(&installed, &wrong)?;
    blocked(&rt, &opts).await?;
    assert_eq!(fs::read_to_string(&installed)?, wrong);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // The native resolver considers user packages even when project resources are excluded.
    fs::remove_file(&config)?;
    fs::write(agent.join("settings.json"), &packages)?;
    opts.settings.pi_project_trusted = Some(false);
    blocked(&rt, &opts).await?;
    fs::remove_file(agent.join("settings.json"))?;
    fs::write(&config, &packages)?;
    // An untrusted project cannot request an install through its settings.
    rt.open(opts.clone()).await?.close().await?;
    assert_eq!(fs::read_to_string(&installed)?, wrong);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    opts.settings.pi_project_trusted = Some(true);
    opts.settings.pi_allow_project_package_install = true;
    opts.env.insert("PI_OFFLINE".into(), "1".into());
    rt.open(opts.clone()).await?.close().await?;
    assert_eq!(fs::read_to_string(&installed)?, wrong);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    opts.remove_env("PI_OFFLINE")?;
    rt.open(opts.clone()).await?.close().await?;
    let version: serde_json::Value = serde_json::from_slice(&fs::read(&installed)?)?;
    assert_eq!(version["version"], "1.0.0");
    let downloaded = calls.load(Ordering::SeqCst);
    assert!(downloaded > 0);
    opts.settings.pi_allow_project_package_install = false;
    rt.open(opts).await?.close().await?;
    assert_eq!(calls.load(Ordering::SeqCst), downloaded);
    assert_eq!(fs::read_to_string(&config)?, packages);
    Ok(())
}
