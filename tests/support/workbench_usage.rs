//! Authenticated Kar HTTP contract, using isolated provider transcript and rate table fixtures.
use super::workbench_server::{client, json, serve_with_env};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path};
fn append(path: &Path, rows: &[Value]) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for row in rows {
        writeln!(f, "{row}").unwrap();
    }
}
#[tokio::main]
pub(super) async fn run() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    let state = tmp.path().join("state");
    let projects = home.join(".claude/projects");
    let sessions = home.join(".codex/sessions");
    let pi = home.join(".pi/agent/sessions");
    for path in [&home, &work, &state, &projects, &sessions, &pi] {
        fs::create_dir_all(path).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let price_url = format!("http://{}/rates", listener.local_addr().unwrap());
    let router=axum::Router::new().route("/rates",axum::routing::get(||async{axum::Json(json!({"metered":{"input_cost_per_token":0.01,"output_cost_per_token":0.05,"cache_read_input_token_cost":0.002,"cache_creation_input_token_cost":0.0125,"cache_creation_input_token_cost_above_1hr":0.02,"provider_specific_entry":{"fast":2}}}))}));
    let price_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let claude = projects.join("billing.jsonl");
    let mut partial = json!({"type":"assistant","timestamp":"2025-11-02T05:23:00Z","sessionId":"native-c","requestId":"req","message":{"model":"metered","id":"msg","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"cache_creation":{"ephemeral_1h_input_tokens":1},"speed":"fast"}}});
    append(&claude, &[partial.clone()]);
    partial["message"]["usage"]["output_tokens"] = json!(6);
    append(&claude, &[partial.clone()]);
    let unknown = json!({"type":"assistant","timestamp":"2025-11-02T06:23:00Z","sessionId":"native-c","message":{"model":"unknown","usage":{"input_tokens":30,"output_tokens":2}}});
    let mut f = fs::OpenOptions::new().append(true).open(&claude).unwrap();
    write!(f, "{unknown}").unwrap();
    let codex = sessions.join("rollout.jsonl");
    let mut ledger = json!({"timestamp":"2025-11-02T06:23:00Z","type":"token_usage_record","payload":{"response_id":"r1","usage":{"input_tokens":15,"cached_input_tokens":5,"output_tokens":4,"reasoning_output_tokens":1}}});
    let context = json!({"type":"turn_context","payload":{"model":"metered","turn_id":"t"}});
    let meta = json!({"type":"session_meta","payload":{"id":"native-code"}});
    let token = ledger["payload"]["usage"].clone();
    let snapshot = json!({"timestamp":"2025-11-02T06:23:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":token,"total_token_usage":token}}});
    append(
        &codex,
        &[meta, context, ledger.clone(), ledger.clone(), snapshot],
    );
    let claude_home = home.join(".claude");
    let codex_home = home.join(".codex");
    let pi_home = home.join(".pi/agent");
    let env = [
        ("HOME", home.as_os_str()),
        ("KAR_USAGE_RATES_URL", price_url.as_ref()),
        ("CLAUDE_CONFIG_DIR", claude_home.as_os_str()),
        ("CODEX_HOME", codex_home.as_os_str()),
        ("PI_CODING_AGENT_DIR", pi_home.as_os_str()),
        ("GROK_HOME", home.as_os_str()),
        ("MUSE_HOME", home.as_os_str()),
    ];
    let request = json!({"since_day":"2025-11-02","until_day":"2025-11-02","time_zone":"America/New_York","resolution":"hour","since_time":"2025-11-02T05:00:00Z","until_time":"2025-11-02T07:00:00Z","await_refresh":true});
    let (mut server, url) = serve_with_env(&state, &work, &home, &env).await;
    let http = client();
    let endpoint = format!("{url}/usage/summary");
    let result = json(http.post(&endpoint).json(&request)).await;
    assert_eq!(result["contract_version"], 6);
    assert_eq!(result["pricing"]["status"], "fresh");
    let rows = result["buckets"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{result}");
    let row = &rows[0];
    assert_eq!(row["provider"], "claude");
    assert_eq!(row["records"], 1);
    assert_eq!(row["totals"]["output"], 6);
    assert_eq!(row["cost_usd"], 0.885);
    assert_eq!(row["speed_premium_usd"], 0.4425);
    assert_eq!(row["hour_start"], "2025-11-02T05:00:00Z");
    assert_eq!(rows[1]["records"], 1);
    assert_eq!(rows[1]["totals"]["uncached_input"], 10);
    assert_eq!(rows[1]["hour_start"], "2025-11-02T06:00:00Z");
    writeln!(f).unwrap();
    drop(f);
    let next = json(http.post(&endpoint).json(&request)).await;
    let rows = next["buckets"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let unpriced = rows.iter().find(|r| r["model"] == "unknown").unwrap();
    assert_eq!(unpriced["unpriced_tokens"], 32);
    assert!(
        next["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["provider"] == "claude" && s["distinct_sessions"] == 1)
    );
    let mut filtered = request.clone();
    filtered["model_filter"] = json!({"include":[{"provider":"claude","model":"unknown"}]});
    assert_eq!(
        json(http.post(&endpoint).json(&filtered)).await["buckets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut bad = request.clone();
    bad["until_time"] = json!("2025-11-04T00:00:00Z");
    assert!(
        http.post(&endpoint)
            .json(&bad)
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    ledger["payload"]["response_id"] = json!("r2");
    append(&codex, &[ledger]);
    let updated = json(http.post(&endpoint).json(&request)).await;
    let rows = updated["buckets"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["provider"] == "codex" && r["records"] == 2)
    );
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    fs::remove_file(&claude).unwrap();
    price_task.abort();
    let (mut server, url) = serve_with_env(&state, &work, &home, &env).await;
    let result = json(http.post(format!("{url}/usage/summary")).json(&request)).await;
    assert_eq!(result["buckets"], updated["buckets"]);
    assert_eq!(result["pricing"]["status"], "cached");
    assert_eq!(
        json(http.post(format!("{url}/usage/rates/refresh"))).await["known_models"],
        1
    );
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}
