//! Native app-server protocol fixture. Never redeem a real credit.
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod server;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead},
    path::PathBuf,
};
use uuid::Uuid;
fn peer() {
    let root = PathBuf::from(std::env::var_os("FIXTURE_NATIVE_DB").unwrap());
    for line in io::stdin().lock().lines() {
        let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let r = match msg["method"].as_str().unwrap() {
            "initialize" => json!({}),
            "initialized" => continue,
            "account/read" => {
                json!({"account":{"type":"chatgpt","email":"kar@example.test"},"requiresOpenaiAuth":true})
            }
            "account/rateLimits/read" => {
                json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":30,"windowDurationMins":300}},"rateLimitResetCredits":{"availableCount":1}})
            }
            "account/rateLimitResetCredit/consume" => {
                let key = msg["params"]["idempotencyKey"].as_str().unwrap();
                Uuid::parse_str(key).unwrap();
                assert_eq!(msg["params"]["creditId"], "synthetic");
                let p = root.join(key);
                let seen = p.exists();
                fs::write(p, if seen { "replayed" } else { "spent" }).unwrap();
                if !seen && root.join("drop-reply").exists() {
                    return;
                }
                json!({"outcome":if seen{"alreadyRedeemed"}else{"reset"}})
            }
            _ => panic!("unexpected native request"),
        };
        println!("{}", json!({"id":msg["id"],"result":r}));
    }
}
async fn exercise() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("project");
    fs::create_dir(&root).unwrap();
    let state = d.path().join("state");
    let config = d.path().join("codex-home");
    fs::create_dir(&config).unwrap();
    let env = [
        ("HOME", d.path().as_os_str()),
        ("CODEX_HOME", config.as_os_str()),
    ];
    let (mut host, url) = server::serve_with_env(&state, &root, d.path(), &env).await;
    let c = server::client();
    let p = server::json(
        c.post(format!("{url}/projects"))
            .json(&json!({"root":root,"name":"resets"})),
    )
    .await;
    let id = p["id"].as_str().unwrap();
    let key = Uuid::new_v4();
    let spend = |url: &str, request: Uuid| {
        c.post(format!(
            "{url}/projects/{id}/accounts/codex/quota/resets/redeem"
        ))
        .header("kar-auth-client", "11111111111111111111111111111111")
        .json(&json!({"request_id":request,"reset_id":"synthetic"}))
    };
    fs::write(d.path().join("drop-reply"), b"").unwrap();
    assert!(!spend(&url, key).send().await.unwrap().status().is_success());
    assert!(d.path().join(key.to_string()).exists());
    assert!(
        !spend(&url, Uuid::new_v4())
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    host.kill().await.unwrap();
    host.wait().await.unwrap();
    let (mut host, url) = server::serve_with_env(&state, &root, d.path(), &env).await;
    let r = server::json(spend(&url, key)).await;
    assert_eq!(r["report"]["kind"], "already_redeemed");
    fs::remove_file(d.path().join(key.to_string())).unwrap();
    assert_eq!(server::json(spend(&url, key)).await, r);
    assert!(!d.path().join(key.to_string()).exists());
    host.kill().await.unwrap();
    host.wait().await.unwrap();
}
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a == ["--version"] {
        println!("codex-cli 0.143.0");
    } else if a == ["login", "status"] {
        println!("Logged in using ChatGPT");
    } else if a.first().map(String::as_str) == Some("app-server") {
        peer();
    } else if a.is_empty() {
        tokio::runtime::Runtime::new().unwrap().block_on(exercise());
    } else {
        panic!("unexpected arguments: {a:?}");
    }
}
