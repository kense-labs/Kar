//! Explicit live simulator acceptance. A private state directory survives an unconfirmed stop.
use super::workbench_server::{client, json, serve};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing {name} for cloud device acceptance"))
}
#[tokio::main]
pub(super) async fn run(expo: bool) {
    let (host, catalog) = if expo {
        assert_eq!(
            required("KAR_EXPO_LIVE_ALLOW_BILLING"),
            "1",
            "Expo charges the selected app until its simulator stops"
        );
        let _ = required("EXPO_TOKEN");
        (
            json!({"id":"live","kind":"eas","label":"Expo acceptance","app_id":required("KAR_EXPO_LIVE_APP"),"max_sessions":1,"max_minutes":10,"idle_minutes":2}),
            "eas-ios".to_owned(),
        )
    } else {
        (
            json!({"id":"live","kind":"simlock","label":"Simlock acceptance","token":required("KAR_SIMLOCK_LIVE_TOKEN"),"gateway_url":required("KAR_SIMLOCK_LIVE_GATEWAY"),"max_sessions":1,"max_minutes":5}),
            required("KAR_SIMLOCK_LIVE_CATALOG"),
        )
    };
    let dir = tempfile::tempdir().unwrap().keep();
    let state = dir.join("state");
    let work = dir.join("workspace");
    let native = dir.join("chat-fixture");
    for p in [&native, &work] {
        std::fs::create_dir(p).unwrap();
    }
    eprintln!(
        "Cloud acceptance state is {}. If interrupted, restart kar workbench --state {} --root {} to reconcile.",
        dir.display(),
        state.display(),
        work.display()
    );
    let (mut server, url) = serve(&state, &work, &native).await;
    let http = client();
    let settings = json(
        http.post(format!("{url}/devices/configure"))
            .json(&json!({"enabled":true,"agent_access_enabled":true,"cloud_hosts":[host]})),
    )
    .await;
    let d = settings["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == catalog && d["host_id"] == "live")
        .expect("selected catalog entry is unavailable");
    let project = json(
        http.post(format!("{url}/projects"))
            .json(&json!({"name":"cloud devices","root":work})),
    )
    .await;
    let p = project["id"].as_str().unwrap();
    let chat = Uuid::new_v4();
    json(
        http.post(format!("{url}/projects/{p}/chats"))
            .json(&json!({"id":chat,"runtime":"codex"})),
    )
    .await;
    let open=json(http.post(format!("{url}/devices/open")).json(&json!({"project":p,"chat":chat,"platform":d["platform"],"host_id":"live","device_id":catalog}))).await;
    let id = open["device_id"].as_str().unwrap();
    // Capture errors now, then issue the stop even if an assertion about the image or agent fails.
    let shot = http
        .get(format!("{url}/devices/screenshot"))
        .query(&[("host_id", "live"), ("device_id", id)])
        .send()
        .await;
    let image = match shot {
        Ok(r) if r.status().is_success() => r.bytes().await.ok(),
        _ => None,
    };
    let agent = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .arg("device-agent")
        .arg("--config")
        .arg(open["agent_config"].as_str().unwrap())
        .arg("action")
        .arg("--json")
        .arg(r#"{"type":"agentCommand","command":"snapshot","arguments":[]}"#)
        .output()
        .await;
    let renewed = if expo {
        true
    } else {
        tokio::time::timeout(Duration::from_secs(175), async {
            loop {
                let leases = http
                    .get(format!("{url}/devices/leases"))
                    .send()
                    .await
                    .ok()?
                    .json::<Value>()
                    .await
                    .ok()?;
                if let Some(l) = leases
                    .as_array()?
                    .iter()
                    .find(|l| l["device_id"] == id && l["last_confirmed_at_ms"].is_i64())
                {
                    eprintln!("Simlock renewal confirmed at {}", l["last_confirmed_at_ms"]);
                    return Some(());
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        })
        .await
        .ok()
        .flatten()
        .is_some()
    };
    json(
        http.post(format!("{url}/devices/shutdown"))
            .json(&json!({"host_id":"live","device_id":id})),
    )
    .await;
    let leases = json(http.get(format!("{url}/devices/leases"))).await;
    assert!(
        leases.as_array().unwrap().is_empty(),
        "provider stop is pending, retain the printed state directory"
    );
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let image = image.expect("cloud screenshot failed");
    assert!(image.starts_with(b"\x89PNG") && image.ends_with(b"IEND\xaeB\x60\x82"));
    assert!(
        agent.is_ok_and(|a| a.status.success()),
        "scoped cloud device snapshot failed"
    );
    assert!(
        renewed,
        "no native renewal acknowledgement before the deadline"
    );
    eprintln!(
        "{} {} target={} bytes={} stop confirmed",
        if expo { "Expo" } else { "Simlock" },
        std::env::consts::OS,
        id,
        image.len()
    );
}
