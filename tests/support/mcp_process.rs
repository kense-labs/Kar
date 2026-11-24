use std::time::Duration;

pub(super) async fn tracked_mcp_running(marker: &str) -> bool {
    let out = tokio::process::Command::new("ps")
        .args(["-axww", "-o", "command="])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.contains(marker))
}

pub(super) async fn assert_mcp_stopped(marker: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while tracked_mcp_running(marker).await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("owned stdio server still running");
}
