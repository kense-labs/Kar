//! Normal workbench HTTP routes, with external collectors and the relay binary explicitly controlled.
use super::server;
use reqwest::{Client, StatusCode, header};
use serde_json::json;
use std::{fs, path::Path};

pub(super) async fn exercise() {
    let tmp = tempfile::tempdir().unwrap();
    let path = |name: &str| tmp.path().join(name);
    let site = path("client");
    let root = path("projects");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir(&site).unwrap();
    fs::write(site.join("index.html"), "Kar web client").unwrap();
    fs::write(site.join("main.js"), "abcdef").unwrap();
    let missing = path("missing-cloudflared");
    let state = path("state");
    let (mut process, base) = server::serve_with_env(
        &state,
        &root,
        &path("native"),
        &[
            ("KAR_STATIC_DIR", site.as_os_str()),
            ("KAR_CLOUDFLARED_PATH", missing.as_os_str()),
        ],
    )
    .await;
    let public = Client::new();
    let host = server::client();
    let asset = format!("{base}/main.js");
    let full = public.get(&asset).send().await.unwrap();
    assert_eq!(full.status(), StatusCode::OK);
    let etag = full.headers()[header::ETAG].clone();
    let range = public
        .get(&asset)
        .header(header::RANGE, "bytes=1-3")
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.text().await.unwrap(), "bcd");
    assert_eq!(
        public
            .get(&asset)
            .header(header::IF_NONE_MATCH, etag)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_MODIFIED
    );
    assert_eq!(
        public
            .get(format!("{base}/ui/history"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "Kar web client"
    );
    let storage = format!("{base}/storage");
    assert_eq!(
        public.get(&storage).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let trace=host.post(format!("{base}/telemetry/v1/traces")).json(&json!({"resourceSpans":[{"scopeSpans":[{"spans":[{"traceId":"trace-one","spanId":"one","name":"ui.render","startTimeUnixNano":"1000000","endTimeUnixNano":"3000000"}]}]}]})).send().await.unwrap();
    assert_eq!(trace.status(), StatusCode::NO_CONTENT);
    let logs = state.join("workbench/traces");
    let active = logs.join("workbench.jsonl");
    assert!(fs::read_to_string(&active).unwrap().contains("ui.render"));
    fs::write(logs.join("workbench.jsonl.2"), "expired").unwrap();
    let preview = server::json(host.get(&storage).query(&[("days", "0")])).await;
    assert_eq!(preview["rotated_logs"]["bytes"], 7);
    assert!(Path::new(&logs.join("workbench.jsonl.2")).is_file());
    let result = server::json(host.post(&storage).json(&json!({"days":0}))).await;
    assert_eq!(result["rotated_logs"]["files"], 1);
    assert!(active.is_file());
    assert!(!logs.join("workbench.jsonl.2").exists());
    let relay = format!("{base}/access/relay-client");
    assert_eq!(
        public.get(&relay).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let err = host.get(&relay).send().await.unwrap();
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    assert!(err.text().await.unwrap().contains("KAR_CLOUDFLARED_PATH"));
    process.kill().await.unwrap();
}
