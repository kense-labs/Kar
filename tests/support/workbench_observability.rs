//! Real macOS host sampling and a dedicated Kar-owned PTY. No model credentials.
use super::workbench_server::{client, json, serve};
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
    let (mut server, url) = serve(&state, &root, &native).await;
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
