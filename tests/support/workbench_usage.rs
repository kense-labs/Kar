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
    let unknown = unknown.to_string();
    write!(f, "{}", &unknown[..unknown.len() - 1]).unwrap();
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
    let opencode = home.join("opencode");
    let antigravity = home.join("antigravity");
    let archived = codex_home.join("archived_sessions");
    fs::create_dir_all(&archived).unwrap();
    let compressed = ruzstd::encoding::compress_to_vec(
        fs::File::open(&codex).unwrap(),
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    fs::write(archived.join("copy.jsonl.zst"), compressed).unwrap();
    let env = [
        ("CURSOR_API_KEY", "".as_ref()),
        ("OPENCODE_DATA_DIR", opencode.as_os_str()),
        ("ANTIGRAVITY_DATA_DIR", antigravity.as_os_str()),
        ("KAR_AGENT_SESSION_CONFIG", "".as_ref()),
        ("KAR_PI_AGENT_DIR", pi_home.as_os_str()),
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
    write!(f, "}}").unwrap();
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
    native_databases(&opencode, &antigravity);
    let with_db = json(http.post(format!("{url}/usage/summary")).json(&request)).await;
    let buckets = with_db["buckets"].as_array().unwrap();
    let row = buckets
        .iter()
        .find(|r| r["provider"] == "opencode")
        .unwrap();
    assert_eq!(row["records"], 2, "{with_db}");
    assert_eq!(row["sessions"], 2);
    assert_eq!(row["totals"]["output"], 8);
    assert_eq!(row["cost_usd"], 0.25);
    let row = buckets
        .iter()
        .find(|r| r["provider"] == "antigravity")
        .unwrap();
    assert_eq!(row["records"], 1, "{with_db}");
    assert_eq!(row["model"], "claude-sonnet-4");
    assert_eq!(row["totals"]["output"], 6);
    assert_eq!(row["totals"]["reasoning"], 4);
    let settings = format!("{url}/usage/settings");
    let before = json(http.get(&settings)).await;
    assert_eq!(before["revision"], 0);
    assert_eq!(before["settings"]["history_retention_days"], 92);
    let update = json!({"request_id":uuid::Uuid::new_v4(),"expected_revision":0,"settings":{"history_retention_days":"unlimited","model_aliases":{"unknown":"first","first":"custom","cycle":"loop","loop":"cycle"},"price_overrides":{"custom":{"input_cost_per_million_tokens":50000,"output_cost_per_million_tokens":100000},"metered":{"input_cost_per_million_tokens":10000,"output_cost_per_million_tokens":50000}}}});
    let receipt = json(http.put(&settings).json(&update)).await;
    assert_eq!(receipt["revision"], 1);
    let mut edited = update.clone();
    edited["request_id"] = json!(uuid::Uuid::new_v4());
    edited["expected_revision"] = json!(1);
    let newer = json(http.put(&settings).json(&edited)).await;
    assert_eq!(newer["revision"], 2);
    assert_eq!(json(http.put(&settings).json(&update)).await, receipt);
    let summary = json(http.post(format!("{url}/usage/summary")).json(&filtered)).await;
    assert_eq!(summary["settings_revision"], 2);
    let sole = &summary["buckets"][0];
    assert_eq!(sole["model"], "custom");
    assert_eq!(sole["records"], 1);
    assert_eq!(sole["cost_usd"], 1.7);
    assert_eq!(
        json(http.get(format!("{settings}?revision=1"))).await,
        receipt
    );
    let changed = json(http.post(format!("{url}/usage/summary")).json(&request)).await;
    let oc = changed["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["provider"] == "opencode")
        .unwrap();
    assert_eq!(oc["cost_source"], "model_priced");
    assert_eq!(oc["cost_usd"], 0.64);
    let mut wrong = update.clone();
    wrong["settings"]["history_retention_days"] = json!(7);
    assert!(
        http.put(&settings)
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    wrong["request_id"] = json!(uuid::Uuid::new_v4());
    assert_eq!(
        http.put(&settings)
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    let (mut server, url) = serve_with_env(&state, &work, &home, &env).await;
    let settings = format!("{url}/usage/settings");
    assert_eq!(json(http.get(&settings)).await, newer);
    assert_eq!(json(http.put(settings).json(&update)).await, receipt);
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}

fn native_databases(opencode: &Path, antigravity: &Path) {
    use sqlite_core::{
        Value as S,
        rebuild::{RecoveredTable, build_recovered_db_tables},
    };
    use uuid::Uuid;
    fn table(name: &str, cols: &[&str], rows: Vec<Vec<S>>) -> RecoveredTable {
        RecoveredTable {
            name: name.into(),
            columns: cols.iter().map(|s| s.to_string()).collect(),
            rows,
        }
    }
    let text = |s: &str| S::Text(s.to_owned());
    let legacy = opencode.join("storage/message/s");
    fs::create_dir_all(&legacy).unwrap();
    fs::create_dir_all(antigravity).unwrap();
    let time = "2025-11-02T05:23:00Z"
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond();
    let message = json!({"role":"assistant","modelID":"metered","tokens":{"input":12,"output":3,"reasoning":1},"time":{"created":time},"cost":0.125});
    let rows = [
        ("parent", "a", 1, time),
        ("fork", "copy", 1, time),
        ("fork", "b", 2, time + 2000),
    ]
    .into_iter()
    .map(|(session, id, seq, at)| {
        let mut data = message.clone();
        data["time"]["created"] = json!(at);
        vec![
            text(session),
            text(id),
            S::Integer(seq),
            S::Integer(at),
            text("assistant"),
            text(&data.to_string()),
        ]
    })
    .collect();
    let messages = table(
        "session_message",
        &["session_id", "id", "seq", "time_created", "type", "data"],
        rows,
    );
    let forks = table(
        "session_v2",
        &["id", "fork_session_id", "fork_boundary", "time_created"],
        vec![vec![
            text("fork"),
            text("parent"),
            text(r#"{"type":"through","messageID":"a"}"#),
            S::Integer(time + 1000),
        ]],
    );
    fs::write(
        opencode.join("opencode.db"),
        build_recovered_db_tables(&[messages, forks]),
    )
    .unwrap();
    let mut copy = message;
    copy["id"] = json!("a");
    copy["sessionID"] = json!("parent");
    fs::write(legacy.join("old.json"), serde_json::to_vec(&copy).unwrap()).unwrap();
    fn var(mut v: u64) -> Vec<u8> {
        let mut a = vec![];
        while v >= 128 {
            a.push((v as u8 & 127) | 128);
            v >>= 7;
        }
        a.push(v as u8);
        a
    }
    fn int(k: u64, v: u64) -> Vec<u8> {
        [var(k << 3), var(v)].concat()
    }
    fn field(k: u64, b: &[u8]) -> Vec<u8> {
        [var((k << 3) | 2), var(b.len() as u64), b.to_vec()].concat()
    }
    let request = Uuid::new_v4().to_string();
    let info = [
        int(1, 281),
        int(2, 10),
        int(3, 2),
        int(9, 4),
        int(10, 2),
        field(11, request.as_bytes()),
    ]
    .concat();
    let meta = field(
        1,
        &[
            int(3, 281),
            field(4, &info),
            field(9, &field(4, &int(1, time as u64 / 1000))),
        ]
        .concat(),
    );
    let generation = table(
        "gen_metadata",
        &["idx", "data"],
        vec![vec![S::Integer(1), S::Blob(meta)]],
    );
    let step = [
        field(9, &info),
        field(24, &[int(1, 281), field(12, b"Claude 4 Sonnet")].concat()),
    ]
    .concat();
    let steps = table(
        "steps",
        &["idx", "metadata"],
        vec![vec![S::Integer(1), S::Blob(step)]],
    );
    fs::write(
        antigravity.join("native.db"),
        build_recovered_db_tables(&[generation, steps]),
    )
    .unwrap();
}

/// Read-only opt-in for a documented local billing period; prints aggregate evidence only.
#[tokio::main]
pub(super) async fn live() {
    let provider = std::env::var("KAR_USAGE_LIVE_PROVIDER")
        .expect("set KAR_USAGE_LIVE_PROVIDER to a provider with existing billing history");
    assert!(matches!(
        provider.as_str(),
        "claude" | "codex" | "pi" | "opencode" | "antigravity" | "cursor" | "grok" | "muse"
    ));
    let since = std::env::var("KAR_USAGE_LIVE_SINCE_DAY")
        .expect("supply the start of an observed billing period");
    let until = std::env::var("KAR_USAGE_LIVE_UNTIL_DAY").unwrap_or_else(|_| since.clone());
    if provider == "cursor" {
        assert!(
            std::env::var("CURSOR_API_KEY").is_ok_and(|s| !s.is_empty()),
            "the Cursor account key is required"
        );
    }
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let root = tmp.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let vars = if provider == "cursor" {
        vec![]
    } else {
        vec![("CURSOR_API_KEY", "".as_ref())]
    };
    let (mut server, url) = serve_with_env(&state, &root, tmp.path(), &vars).await;
    let result = json(client().post(format!("{url}/usage/summary")).json(
        &json!({"since_day":since,"until_day":until,"time_zone":"UTC","await_refresh":true}),
    ))
    .await;
    let rows = result["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["provider"] == provider)
        .collect::<Vec<_>>();
    let records = rows
        .iter()
        .map(|r| r["records"].as_u64().unwrap())
        .sum::<u64>();
    let tokens = rows
        .iter()
        .map(|r| {
            ["uncached_input", "cached_input", "cache_creation", "output"]
                .into_iter()
                .map(|k| r["totals"][k].as_u64().unwrap())
                .sum::<u64>()
        })
        .sum::<u64>();
    let cost = rows
        .iter()
        .map(|r| r["cost_usd"].as_f64().unwrap())
        .sum::<f64>();
    assert!(
        records > 0 && tokens > 0,
        "the selected provider has no billing evidence in the requested UTC interval"
    );
    let statuses = result["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["provider"] == provider)
        .map(|s| s["status"].clone())
        .collect::<Vec<_>>();
    assert!(
        statuses.iter().any(|s| s == "ok"),
        "no healthy source for the provider: {statuses:?}"
    );
    if let Ok(expected) = std::env::var("KAR_USAGE_LIVE_EXPECT_TOKENS") {
        assert_eq!(tokens, expected.parse::<u64>().unwrap());
    }
    println!(
        "{}",
        json!({"provider":provider,"platform":std::env::consts::OS,"since_day":since,"until_day":until,"sources":statuses,"records":records,"tokens":tokens,"cost_usd":cost,"pricing":result["pricing"]["status"],"contract_version":result["contract_version"]})
    );
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}
