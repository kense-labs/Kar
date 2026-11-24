//! Real macOS host sampling and a dedicated Kar-owned PTY. No model credentials.
use super::workbench_server::{client, json, serve_with_env};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

#[tokio::main]
pub(super) async fn run() {
    assert!(
        cfg!(target_os = "macos"),
        "host telemetry acceptance requires macOS"
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let state = temp.path().join("state");
    let native = temp.path().join("unused");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&native).unwrap();
    let repo = temp.path().join("fixture-source");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.name=Kar Fixture",
                "-c",
                "user.email=kar@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "tag.gpgSign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "--allow-empty", "-qm", "release"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    git(&["tag", "-a", &tag, "-m", "release"]);
    git(&["commit", "--allow-empty", "-qm", "main advances"]);
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = format!("http://{}", socket.local_addr().unwrap());
    let sink = axum::Router::new()
        .route(
            "/traces",
            axum::routing::post(
                |headers: axum::http::HeaderMap, axum::Json(v): axum::Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer check-secret");
                    assert_eq!(v, json!({"resourceSpans":[]}));
                    axum::http::StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/other",
            axum::routing::post(|headers: axum::http::HeaderMap| async move {
                assert!(headers.get("authorization").is_none());
                axum::http::StatusCode::TEMPORARY_REDIRECT
            }),
        )
        .route(
            "/logs",
            axum::routing::post(
                |headers: axum::http::HeaderMap, bytes: axum::body::Bytes| async move {
                    assert_eq!(headers["content-type"], "application/x-protobuf");
                    assert!(bytes.is_empty());
                    axum::http::StatusCode::OK
                },
            ),
        )
        .route(
            "/timeout",
            axum::routing::post(|| async {
                tokio::time::sleep(Duration::from_secs(20)).await;
            }),
        );
    tokio::spawn(async move {
        axum::serve(socket, sink).await.unwrap();
    });
    let endpoint = format!("{peer}/traces");
    let (mut server, url) = serve_with_env(
        &state,
        &root,
        &native,
        &[
            ("KAR_TRIAGE_REPOSITORY", repo.as_os_str()),
            ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json".as_ref()),
            ("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/protobuf".as_ref()),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", endpoint.as_ref()),
            (
                "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
                "authorization=Bearer%20check-secret".as_ref(),
            ),
        ],
    )
    .await;
    let sid = server.id().unwrap();
    let http = client();
    let resources = json(http.get(format!("{url}/diagnostics/host-resources"))).await;
    assert!(resources["cpu_count"].as_u64().unwrap() > 0);
    assert!(
        resources["total_memory_bytes"].as_u64().unwrap()
            >= resources["available_memory_bytes"].as_u64().unwrap()
    );
    assert!((0.0..=1.0).contains(&resources["cpu_utilization"].as_f64().unwrap()));
    let p = json(
        http.post(format!("{url}/projects"))
            .json(&json!({"name":"metrics","root":root})),
    )
    .await;
    let terminal = json(
        http.post(format!(
            "{url}/projects/{}/terminals",
            p["id"].as_str().unwrap()
        ))
        .json(&json!({"id":Uuid::new_v4()})),
    )
    .await;
    let pid = terminal["process"]["pid"]
        .as_u64()
        .expect("terminal shell has an identity");
    let first = json(http.get(format!("{url}/telemetry"))).await;
    assert_eq!(first["health"]["native"]["status"], "healthy");
    let rows = first["processes"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|p| p["pid"] == sid && p["category"] == "server")
    );
    let shell = rows
        .iter()
        .find(|p| p["pid"] == pid && p["category"] == "terminal-root")
        .unwrap();
    let born = shell["start_time_ms"].as_u64().unwrap();
    assert_eq!(born, terminal["process"]["born"].as_u64().unwrap() / 1000);
    let diag = json(http.get(format!("{url}/diagnostics/processes"))).await;
    assert!(
        !diag["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["pid"] == sid)
    );
    assert!(!diag.to_string().contains("kar-workbench-fixture"));
    let mut a = http
        .get(format!("{url}/telemetry/events"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut b = http
        .get(format!("{url}/telemetry/events"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let before = next_frame(&mut a).await;
    assert_eq!(next_frame(&mut b).await["sample_interval_ms"], 1000);
    let after = next_frame(&mut a).await;
    assert_ne!(before["read_at"], after["read_at"]);
    drop(a);
    drop(b);
    for endpoint in ["telemetry/history", "diagnostics/process-history"] {
        let h = json(http.get(format!("{url}/{endpoint}?window_ms=5000&bucket_ms=1000"))).await;
        assert_eq!(h["buckets"].as_array().unwrap().len(), 5);
        assert!(h["retained_sample_count"].as_u64().unwrap() >= 2);
        assert!(
            h["top_processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["pid"] == pid)
        );
    }
    let retry = json(http.post(format!("{url}/telemetry/retry"))).await;
    assert_eq!(retry["accepted"], false);
    for (target, start) in [
        (sid as u64, born),
        (pid, born + 1),
        (std::process::id() as u64, born),
    ] {
        let result = json(
            http.post(format!("{url}/diagnostics/processes/signal"))
                .json(&json!({"pid":target,"start_time_ms":start,"signal":"SIGINT"})),
        )
        .await;
        assert_eq!(result["signaled"], false);
    }
    let stopped = json(
        http.post(format!("{url}/diagnostics/processes/signal"))
            .json(&json!({"pid":pid,"start_time_ms":born,"signal":"SIGKILL"})),
    )
    .await;
    assert_eq!(stopped["signaled"], true);
    for (signal, path, expected) in [
        ("traces", "traces", "accepted"),
        ("logs", "logs", "accepted"),
        ("traces", "other", "rejected"),
        ("metrics", "timeout", "unreachable"),
    ] {
        let result = json(
            http.post(format!("{url}/diagnostics/otlp"))
                .json(&json!({"signal":signal,"url":format!("{peer}/{path}")})),
        )
        .await;
        assert_eq!(result["type"], expected);
        if expected == "rejected" {
            assert_eq!(result["status"], 307);
        }
        if expected == "unreachable" {
            assert_eq!(result["timed_out"], true);
        }
    }
    let invalid = http
        .post(format!("{url}/diagnostics/processes/signal"))
        .json(&json!({"pid":pid,"start_time_ms":born,"signal":"invalid"}))
        .send()
        .await
        .unwrap();
    assert!(!invalid.status().is_success());
    let trace = json(http.get(format!("{url}/diagnostics/traces"))).await;
    assert!(
        trace["record_count"].as_u64().unwrap() > 0 && trace["failure_count"].as_u64().unwrap() > 0
    );
    assert!(!trace.to_string().contains("check-secret"));
    assert!(
        trace["latest_warning_and_error_logs"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
    );
    let path = std::path::Path::new(trace["trace_file_path"].as_str().unwrap());
    std::fs::write(path.with_extension("jsonl.1"), "broken record\n").unwrap();
    assert!(
        json(http.get(format!("{url}/diagnostics/traces"))).await["parse_error_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    let prepare = || {
        http.post(format!("{url}/diagnostics/triage"))
            .json(&json!({"client":{"label":"Kar acceptance","os":"macOS"}}))
    };
    let first = json(prepare()).await;
    assert_eq!(first["source"]["commit"], commit);
    assert!(
        std::fs::read_to_string(first["prompt_path"].as_str().unwrap())
            .unwrap()
            .contains("Kar triage")
    );
    assert!(
        std::fs::read_to_string(first["context_path"].as_str().unwrap())
            .unwrap()
            .contains("Kar acceptance")
    );
    let checkout = std::path::PathBuf::from(first["source"]["directory"].as_str().unwrap());
    tokio::time::timeout(Duration::from_secs(15), async {
        while !checkout.join(".git").is_dir() {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let next = json(prepare()).await;
    assert_eq!(next["source"]["status"], "cloned");
    assert_ne!(next["context_path"], first["context_path"]);
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}
async fn next_frame(stream: &mut reqwest::Response) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut data = vec![];
        loop {
            data.extend_from_slice(&stream.chunk().await.unwrap().unwrap());
            if let Some(end) = data.windows(2).position(|w| w == b"\n\n") {
                let frame = std::str::from_utf8(&data[..end]).unwrap();
                if let Some(json) = frame.lines().find_map(|l| l.strip_prefix("data: ")) {
                    return serde_json::from_str(json).unwrap();
                }
                data.drain(..end + 2);
            }
        }
    })
    .await
    .unwrap()
}
