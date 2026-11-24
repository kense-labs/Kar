use super::{alternate_paths, options};
use kar::{
    Result,
    providers::{CatalogCancellation, CatalogRefreshOptions, PiModelRuntimePaths, PiModelsPath},
};
use serde_json::{Value, json};
use std::{fs, io, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use tokio_rustls::{TlsAcceptor, rustls};

fn config(model: &str) -> Value {
    json!({"providers":{"local-catalog":{"name":"Local Catalog","api":"openai-completions","baseUrl":"http://127.0.0.1:1/v1","models":[{"id":model,"name":"Local","reasoning":true,"input":["text","image"],"contextWindow":65000,"maxTokens":5000}]}}})
}
fn scoped(provider: &str, network: bool) -> CatalogRefreshOptions {
    CatalogRefreshOptions {
        providers: Some(vec![provider.into()]),
        allow_network: network,
    }
}
#[tokio::test]
#[ignore = "requires npm Pi 1.6 with a native SDK host"]
async fn sdk_catalog_capabilities_and_scoped_reload() -> Result<()> {
    let home = tempfile::tempdir()?;
    let root = home.path();
    let paths = alternate_paths("profiles/auth.json", "config/models.json");
    fs::create_dir(root.join("config"))?;
    fs::create_dir(root.join("profiles"))?;
    fs::write(root.join("config/models.json"), config("first").to_string())?;
    fs::write(
        root.join("profiles/auth.json"),
        r#"{"local-catalog":{"type":"api_key","key":"!echo resolved > catalog-key-ran"}}"#,
    )?;
    let rt = kar::runtime("pi")?;
    let mut catalog = rt.pi_model_catalog(options(&home), paths).await?;
    let providers = catalog.providers();
    assert!(
        providers
            .iter()
            .any(|p| p.id == "local-catalog" && p.name == "Local Catalog" && p.configured)
    );
    assert!(providers.len() > 20 && catalog.models(None).count() > 100);
    assert_eq!(catalog.default_model("local-catalog"), Some("first"));
    assert_eq!(catalog.default_model("missing"), None);
    assert!(!root.join("catalog-key-ran").exists());
    let row = serde_json::to_value(catalog.models(Some("local-catalog")).next().unwrap())?;
    assert_eq!(
        row,
        json!({"id":"first","providerId":"local-catalog","wire":"openai-completions","baseUrl":"http://127.0.0.1:1/v1","contextWindow":65000,"maxTokens":5000,"reasoning":true,"input":["text","image"]})
    );
    fs::write(root.join("profiles/auth.json"), "{}")?;
    fs::write(
        root.join("config/models.json"),
        config("second").to_string(),
    )?;
    assert_eq!(catalog.default_model("local-catalog"), Some("first"));
    let done = catalog
        .refresh(
            scoped("local-catalog", false),
            &CatalogCancellation::default(),
        )
        .await?;
    assert!(!done.aborted && done.errors.is_empty());
    assert_eq!(catalog.default_model("local-catalog"), Some("second"));
    assert!(
        !catalog
            .providers()
            .iter()
            .find(|p| p.id == "local-catalog")
            .unwrap()
            .configured
    );
    assert!(!root.join("agent/auth.json").exists());
    let disabled = PiModelRuntimePaths {
        auth_path: Some("profiles/auth.json".into()),
        models_path: PiModelsPath::Disabled,
    };
    let other = rt.pi_model_catalog(options(&home), disabled).await?;
    assert_eq!(other.models(Some("local-catalog")).count(), 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires npm Pi; checks complete transfer of a multi-megabyte user catalog"]
async fn sdk_large_catalog_snapshot_is_complete() -> Result<()> {
    let home = tempfile::tempdir()?;
    let registry = home.path().join("registry.json");
    let mut cfg = config("unused");
    let count = 5000;
    cfg["providers"]["local-catalog"]["baseUrl"] =
        format!("http://127.0.0.1:1/{}", "x".repeat(1900)).into();
    cfg["providers"]["local-catalog"]["models"] = (0..count)
        .map(|i| json!({"id":format!("fixture-{i}"),"name":format!("Fixture {i}")}))
        .collect();
    fs::write(registry, serde_json::to_vec(&cfg)?)?;
    let mut catalog = kar::runtime("pi")?
        .pi_model_catalog(
            options(&home),
            alternate_paths("auth.json", "registry.json"),
        )
        .await?;
    let check = |c: &kar::providers::PiModelCatalog| {
        let mut rows = c.models(Some("local-catalog"));
        for i in 0..count {
            assert_eq!(rows.next().unwrap().id, format!("fixture-{i}"));
        }
        assert!(rows.next().is_none());
    };
    check(&catalog);
    let cancelled = CatalogCancellation::default();
    cancelled.cancel();
    assert!(
        catalog
            .refresh(scoped("local-catalog", false), &cancelled)
            .await?
            .aborted
    );
    let answer = catalog
        .refresh(
            scoped("local-catalog", false),
            &CatalogCancellation::default(),
        )
        .await?;
    assert!(!answer.aborted && answer.errors.is_empty());
    check(&catalog);
    Ok(())
}

struct Call {
    request: String,
    reply: oneshot::Sender<String>,
}
async fn headers(input: &mut (impl AsyncBufRead + Unpin)) -> io::Result<String> {
    let mut data = String::new();
    loop {
        if input.read_line(&mut data).await? == 0 || data.ends_with("\r\n\r\n") {
            return Ok(data);
        }
        if data.len() > 16 * 1024 {
            return Err(io::Error::other("oversized fixture header"));
        }
    }
}
// The proxy only terminates local fixture requests for pi.dev. No upstream
// connection is made. Tests control completion of each individual response.
async fn proxy(home: &Path) -> (String, mpsc::Receiver<Call>) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["pi.dev".into()]).unwrap();
    fs::write(home.join("catalog-ca.pem"), cert.pem()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    )
    .unwrap();
    let accept = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let accept = accept.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                let tunnel = headers(&mut stream).await?;
                if !tunnel.starts_with("CONNECT pi.dev:443 ") {
                    return Err(io::Error::other("unexpected proxy target"));
                }
                stream
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                let mut tls = BufReader::new(accept.accept(stream).await?);
                let request = headers(&mut tls).await?;
                let (reply, response) = oneshot::channel();
                if tx.send(Call { request, reply }).await.is_ok()
                    && let Ok(body) = response.await
                {
                    tls.write_all(body.as_bytes()).await?;
                    tls.shutdown().await?;
                }
                Ok(())
            });
        }
    });
    (format!("http://{addr}"), rx)
}
async fn next(rx: &mut mpsc::Receiver<Call>, id: &str) -> Call {
    let call = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        call.request
            .starts_with(&format!("GET /api/models/providers/{id}?")),
        "Unexpected Pi catalog route"
    );
    assert!(!call.request.contains("private-catalog-fixture"));
    call
}
fn response(code: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {code}\r\nConnection: close\r\nContent-Type: application/json\r\nETag: \"catalog-v1\"\r\nLast-Modified: Fri, 01 Jan 2094 00:00:00 GMT\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
#[tokio::test]
#[ignore = "requires npm Pi; all remote catalog requests use an isolated local TLS proxy"]
async fn sdk_catalog_remote_cache_errors_and_cancellation() -> Result<()> {
    let home = tempfile::tempdir()?;
    let root = home.path();
    let (url, mut requests) = proxy(root).await;
    for dir in ["agent", "profile", "config"] {
        fs::create_dir(root.join(dir))?;
    }
    fs::write(
        root.join("agent/settings.json"),
        json!({"httpProxy":url}).to_string(),
    )?;
    fs::write(
        root.join("profile/auth.json"),
        r#"{"anthropic":{"type":"api_key","key":"private-catalog-fixture"},"openai":{"type":"api_key","key":"private-catalog-fixture"}}"#,
    )?;
    let mut opts = options(&home);
    opts.installation.env.insert(
        "NODE_EXTRA_CA_CERTS".into(),
        Some(root.join("catalog-ca.pem").into_os_string()),
    );
    let rt = kar::runtime("pi")?;
    let paths = alternate_paths("profile/auth.json", "config/models.json");
    let mut catalog = rt.pi_model_catalog(opts.clone(), paths.clone()).await?;
    let remote=json!([{"id":"remote-fixture","name":"Remote Fixture","api":"anthropic-messages","baseUrl":"http://127.0.0.1:1","input":["text"],"reasoning":false,"contextWindow":12000,"maxTokens":1000}]).to_string();
    let idle = CatalogCancellation::default();
    let (updated, ()) = tokio::join!(catalog.refresh(scoped("anthropic", true), &idle), async {
        next(&mut requests, "anthropic")
            .await
            .reply
            .send(response("200 OK", &remote))
            .unwrap();
    });
    assert!(updated?.errors.is_empty());
    assert!(
        catalog
            .models(Some("anthropic"))
            .any(|m| m.id == "remote-fixture")
    );
    let (failed, ()) = tokio::join!(catalog.refresh(scoped("openai", true), &idle), async {
        next(&mut requests, "openai")
            .await
            .reply
            .send(response("418 Fixture Error", "{}"))
            .unwrap();
    });
    assert!(
        failed?
            .errors
            .get("openai")
            .is_some_and(|e| e.contains("418"))
    );
    assert!(
        catalog
            .models(Some("anthropic"))
            .any(|m| m.id == "remote-fixture")
    );
    let cancel = CatalogCancellation::default();
    let (aborted, held) = tokio::join!(catalog.refresh(scoped("openai", true), &cancel), async {
        let call = next(&mut requests, "openai").await;
        cancel.cancel();
        call
    });
    assert!(aborted?.aborted);
    drop(held);
    // Dropping the caller while a fetch is pending must also preserve framing.
    let held = {
        let refresh = catalog.refresh(scoped("openai", true), &idle);
        tokio::pin!(refresh);
        tokio::select! { reply=&mut refresh=>panic!("refresh completed before response: {reply:?}"), call=next(&mut requests,"openai")=>call }
    };
    assert!(
        catalog
            .refresh(scoped("anthropic", false), &idle)
            .await?
            .errors
            .is_empty()
    );
    drop(held);
    let cache = root.join("config/models-store.json");
    let mut saved: Value = serde_json::from_slice(&fs::read(&cache)?)?;
    assert_eq!(saved["anthropic"]["etag"], "\"catalog-v1\"");
    saved["anthropic"]["checkedAt"] = 0.into();
    fs::write(cache, serde_json::to_vec(&saved)?)?;
    let mut reopened = rt.pi_model_catalog(opts, paths).await?;
    let (result, ()) = tokio::join!(reopened.refresh(scoped("anthropic", true), &idle), async {
        let call = next(&mut requests, "anthropic").await;
        assert!(
            call.request
                .to_lowercase()
                .contains("if-none-match: \"catalog-v1\"")
        );
        call.reply.send(response("304 Not Modified", "")).unwrap();
    });
    assert!(result?.errors.is_empty());
    assert!(
        reopened
            .models(Some("anthropic"))
            .any(|m| m.id == "remote-fixture")
    );
    assert!(requests.try_recv().is_err());
    assert!(!root.join("agent/auth.json").exists());
    Ok(())
}
