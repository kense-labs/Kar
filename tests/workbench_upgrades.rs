//! End-to-end integration tests for Kar's macOS Pure-Rust Oar v0.136.0 & T3 Code parity upgrades:
//! - Framed CRC32 WAL (`projects.wal` / `workflows.wal`), `/storage` compaction & `WorktreeCleanupRules`,
//!   and transparent missing-worktree re-materialization (`restore_removed_worktree`).
//! - Unified encrypted `SecretService` (`/server/secrets`, batched `op inject`, encrypted Forge PATs,
//!   zero plaintext secrets on disk, and automatic self-hosted Forgejo discovery).
//! - Lightweight HTTP `TextGeneration` (`/text-generation/*`, `sourceControlWritingStyle`, PR templates).
//! - SEP-3415 Outbound MCP Event Webhooks (`events/subscribe`, IANA SSRF guard, `whsec_` HMAC-SHA256,
//!   secret rotation dual-signature, live milestone delivery, and `events/unsubscribe`).
//! - Complete MCP `annotations`, `_meta`, typed JSON Schemas, stacked PR merge (`stack: true`),
//!   and cross-environment `environment_id` forwarding across paired Kar workbenches.
//! - macOS operational routes (`/server/policy`, `/server/service`, `/server/doctor`,
//!   `/environments/tailscale`, `/environments/ssh/hosts`), GB18030 file decoding,
//!   and SDK `PiDurable`, `VoyageRecorder` (`oar-voyage/3` compatibility), and `scripted_runtime`.

#[allow(dead_code)]
#[path = "support/codex_chat_peer.rs"]
mod peer;
#[allow(dead_code)]
#[path = "support/workbench_server.rs"]
mod server;

use axum::{
    Json, Router,
    http::HeaderMap,
    routing::{get, post, put},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpListener;
use uuid::Uuid;

fn git_init_with_commit(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Kar Test")
            .env("GIT_AUTHOR_EMAIL", "test@kar.local")
            .env("GIT_COMMITTER_NAME", "Kar Test")
            .env("GIT_COMMITTER_EMAIL", "test@kar.local")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["init", "-b", "main"]);
    fs::write(dir.join("README.md"), "# Kar Upgrade Suite\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial commit"]);
}

async fn mcp_call(c: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
    let mut enriched = params.clone();
    if enriched.is_null() || !enriched.is_object() {
        enriched = json!({});
    }
    enriched["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {
            "extensions": {"io.modelcontextprotocol/events": {}}
        }
    });
    let mut req = c
        .post(format!("{url}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method);
    if let Some(name) = enriched.get("name").and_then(Value::as_str) {
        req = req.header("mcp-name", name);
    }
    server::json(req.json(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": enriched,
    })))
    .await
}

async fn test_sdk_durable_voyage_and_scripted() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();

    // 1. Zero-token `scripted_runtime`
    let rt = kar::testing::scripted_runtime(
        "scripted",
        vec![kar::testing::ScriptedStep::reply(
            "hello from scripted runtime",
        )],
    );
    let session = rt.open(kar::SessionOptions::new(&cwd)).await.unwrap();
    let turn = session.prompt("ping").await.unwrap();
    assert_eq!(turn.wait().await.unwrap(), kar::Outcome::Completed);

    // 2. Crash-safe `VoyageRecorder` + `"oar-voyage/3"` compatibility
    let voyage_path = tmp.path().join("run.voyage.jsonl");
    let mut recorder =
        kar::voyage::VoyageRecorder::for_session(&voyage_path, "scripted", &session).unwrap();
    for rec in session.records() {
        recorder.record(&rec).unwrap();
    }
    session.close().await.unwrap();
    assert!(recorder.finish().unwrap() > 0);

    let mut replayed = Vec::new();
    let summary = kar::voyage::import(
        std::io::BufReader::new(fs::File::open(&voyage_path).unwrap()),
        |r| {
            replayed.push(r);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(summary.header["runtime"], "scripted");
    assert!(!replayed.is_empty());

    // Verify `"oar-voyage/3"` header import parity
    let oar_raw = fs::read_to_string(&voyage_path).unwrap().replacen(
        "\"kar-voyage/3\"",
        "\"oar-voyage/3\"",
        1,
    );
    let mut oar_replayed = Vec::new();
    let oar_summary =
        kar::voyage::import_records_or_voyage(std::io::Cursor::new(oar_raw.into_bytes()), |r| {
            oar_replayed.push(r);
            Ok(())
        })
        .unwrap();
    assert_eq!(oar_summary.records, summary.records);
    assert_eq!(oar_replayed.len(), replayed.len());

    // 3. Pure-Rust `PiDurable` engine
    let storage = kar::pi_durable::FileStorage::new(tmp.path().join("durable")).unwrap();
    let provider = kar::pi_durable::DurableProvider::scripted("pi-test-model", |msgs, _model| {
        Ok(kar::pi_durable::DurableResponderOutput::chunks([
            "durable:",
            msgs.last().unwrap().content.as_str(),
        ]))
    });
    let engine = kar::pi_durable::PiDurable::new(storage, provider);
    let d_sess = engine.open(kar::SessionOptions::new(&cwd)).await.unwrap();
    let conv_id = d_sess.id().to_owned();
    let report = d_sess
        .prompt("turn-one")
        .await
        .unwrap()
        .report()
        .await
        .unwrap();
    assert_eq!(report.text, "durable:turn-one");
    d_sess.close().await.unwrap();

    let mut resume_opts = kar::SessionOptions::new(&cwd);
    resume_opts.resume = Some(conv_id);
    let resumed = engine.open(resume_opts).await.unwrap();
    assert_eq!(resumed.durable_state().unwrap().unwrap().messages.len(), 2);
    resumed.close().await.unwrap();
}

async fn test_workbench_wave2_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let roots = base.join("projects");
    let repo_dir = roots.join("demo");
    git_init_with_commit(&repo_dir);

    // Write a PR template + a GB18030-encoded Chinese C file in the repo!
    fs::create_dir_all(repo_dir.join(".github")).unwrap();
    fs::write(
        repo_dir.join(".github/PULL_REQUEST_TEMPLATE.md"),
        "## Summary\n- \n## Verification\n- ",
    )
    .unwrap();
    let chinese_src = "// 中文GB18030编码源码测试\nint add(int a, int b) { return a + b; }\n";
    let (gbk_bytes, _, _) = encoding_rs::GB18030.encode(chinese_src);
    fs::write(repo_dir.join("legacy_gbk.c"), &gbk_bytes).unwrap();

    // Mock OpenAI-compatible TextGeneration + Self-Hosted Forgejo + Stacked GitHub PR server
    let merged_prs: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let merged_clone = merged_prs.clone();
    let mock_llm_and_forge = Router::new()
        .route(
            "/v1/chat/completions",
            post(|Json(body): Json<Value>| async move {
                let sys = body["messages"][0]["content"].as_str().unwrap_or("");
                let usr = body["messages"][1]["content"].as_str().unwrap_or("");
                let reply = if sys.contains("pull request title") {
                    assert!(
                        usr.contains("## Verification"),
                        "PR template was not injected into prompt: {usr}"
                    );
                    r###"{"title":"feat: add pure-rust upgrades","body":"## Summary\n- Upgraded WAL & secrets\n## Verification\n- cargo test"}"###
                } else if sys.contains("branch name") {
                    "feat/pure-rust-wal"
                } else if sys.contains("chat title") {
                    "Pure Rust Backend Audit"
                } else {
                    "feat(wal): add framed CRC32 write-ahead log"
                };
                Json(json!({
                    "choices": [{"message": {"content": reply}}]
                }))
            }),
        )
        // Self-hosted Forgejo discovery endpoint (`/api/v1/version`)
        .route(
            "/api/v1/version",
            get(|| async { Json(json!({"version": "9.0.0+gitea-1.22.0"})) }),
        )
        .route(
            "/api/v1/repos/team/selfhosted",
            get(|headers: HeaderMap| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                assert!(
                    auth.contains("secret-forgejo-pat-123"),
                    "missing encrypted Forgejo PAT header: {auth}"
                );
                Json(json!({"default_branch": "main", "full_name": "team/selfhosted"}))
            }),
        )
        .route(
            "/api/v1/user",
            get(|| async { Json(json!({"login": "forgejo-bot"})) }),
        )
        // Stacked GitHub PR merge endpoints
        .route(
            "/repos/kar/stacked/stacks",
            get(|| async {
                Json(json!([{
                    "id": "stk-1",
                    "number": 10,
                    "base": "main",
                    "html_url": "https://github.com/kar/stacked/stacks/10",
                    "pull_requests": [
                        {"number": 1, "state": "open", "title": "Layer 1", "draft": false, "head": {"ref": "feat/l1", "sha": "sha-layer-1"}},
                        {"number": 2, "state": "open", "title": "Layer 2", "draft": false, "head": {"ref": "feat/l2", "sha": "sha-layer-2"}}
                    ]
                }]))
            }),
        )
        .route(
            "/repos/kar/stacked/stacks/10",
            get(|| async {
                Json(json!({
                    "id": "stk-1",
                    "number": 10,
                    "base": "main",
                    "html_url": "https://github.com/kar/stacked/stacks/10",
                    "pull_requests": [
                        {"number": 1, "state": "open", "title": "Layer 1", "draft": false, "head": {"ref": "feat/l1", "sha": "sha-layer-1"}},
                        {"number": 2, "state": "open", "title": "Layer 2", "draft": false, "head": {"ref": "feat/l2", "sha": "sha-layer-2"}}
                    ]
                }))
            }),
        )
        .route(
            "/repos/kar/stacked/pulls/1",
            get(|| async {
                Json(json!({
                    "number": 1,
                    "node_id": "PR_1",
                    "state": "open",
                    "title": "Layer 1",
                    "html_url": "https://github.com/kar/stacked/pull/1",
                    "head": {"ref": "feat/l1", "sha": "sha-layer-1"},
                    "base": {"ref": "main", "sha": "sha-main"}
                }))
            }),
        )
        .route(
            "/repos/kar/stacked/pulls/2",
            get(|| async {
                Json(json!({
                    "number": 2,
                    "node_id": "PR_2",
                    "state": "open",
                    "title": "Layer 2",
                    "html_url": "https://github.com/kar/stacked/pull/2",
                    "head": {"ref": "feat/l2", "sha": "sha-layer-2"},
                    "base": {"ref": "feat/l1", "sha": "sha-layer-1"}
                }))
            })
            .patch(|Json(body): Json<Value>| async move {
                assert_eq!(body["base"], "main");
                Json(json!({"number": 2}))
            }),
        )
        .route(
            "/repos/kar/stacked/pulls/{num}/merge",
            put(move |axum::extract::Path(num): axum::extract::Path<u64>| {
                let merged_clone = merged_clone.clone();
                async move {
                    merged_clone.lock().unwrap().push(num);
                    Json(json!({"merged": true, "sha": format!("merged-{num}")}))
                }
            }),
        );
    let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_llm_and_forge).await;
    });

    // Mock Outbound MCP Webhook receiver
    let deliveries: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let deliveries_clone = deliveries.clone();
    let hook_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_addr = hook_listener.local_addr().unwrap();
    let hook_app = Router::new().route(
        "/mcp-hook",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let deliveries_clone = deliveries_clone.clone();
            async move {
                let sig = headers
                    .get("webhook-signature")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                if body["type"] == "verification" {
                    return Json(json!({"challenge": body["challenge"]}));
                }
                deliveries_clone.lock().unwrap().push((sig, body));
                Json(json!({"ok": true}))
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(hook_listener, hook_app).await;
    });

    // Forge config for stacked GitHub PR fixture (leaving `127.0.0.1:{port}` unconfigured for self-hosted Forgejo auto-discovery!)
    let forge_cfg = base.join("forge.toml");
    fs::write(
        &forge_cfg,
        format!(
            "[[hosts]]\nhost = \"stacked.github.example\"\nkind = \"github\"\napi_base = \"http://{mock_addr}\"\n"
        ),
    )
    .unwrap();

    let state_a = base.join("state-a");
    let native_a = base.join("native-a");
    fs::create_dir_all(&native_a).unwrap();
    let llm_url = format!("http://{mock_addr}/v1");
    let env_a = [
        (
            "KAR_TEXT_GENERATION_API_KEY",
            std::ffi::OsStr::new("test-llm-key"),
        ),
        ("KAR_TEXT_GENERATION_URL", std::ffi::OsStr::new(&llm_url)),
        (
            "KAR_MCP_WEBHOOK_ALLOWED_HOSTS",
            std::ffi::OsStr::new("127.0.0.1"),
        ),
        ("KAR_FORGE_CONFIG", forge_cfg.as_os_str()),
        ("GH_TOKEN", std::ffi::OsStr::new("gh-stack-token")),
    ];
    let (mut host_a, url_a) = server::serve_with_env(&state_a, &roots, &native_a, &env_a).await;
    let c = server::client();

    // 1. Register project & verify GB18030 file decoding + WAL persistence + `/storage` compaction
    let proj = server::json(
        c.post(format!("{url_a}/projects"))
            .json(&json!({"root": repo_dir, "name": "demo"})),
    )
    .await;
    let pid = proj["id"].as_str().unwrap().to_owned();

    // Trigger hot catalog mutation so `projects.wal` has frames, then inspect & compact via `/storage`
    server::json(c.patch(format!("{url_a}/server/settings")).json(&json!({
        "sourceControlWritingStyle": {
            "mode": "conventional_commits",
            "follow_change_request_templates": true,
            "custom_instructions": "Use imperative mood."
        },
        "worktreeCleanup": {
            "enabled": true,
            "afterDays": 14,
            "settledAfterDays": 7,
            "onMerge": true,
            "onDelete": true,
            "unchanged": true
        }
    })))
    .await;
    let wb_dir = state_a.join("workbench");
    assert!(wb_dir.join("projects.wal").exists());
    let storage_before = server::json(c.get(format!("{url_a}/storage"))).await;
    assert!(
        storage_before["storage"]["wal"]["catalog"]["frames"]
            .as_u64()
            .unwrap()
            >= 1
    );
    let storage_compacted = server::json(
        c.post(format!("{url_a}/storage"))
            .json(&json!({"days": 30, "compactWal": true})),
    )
    .await;
    assert!(
        storage_compacted["storage"]["wal"]["reclaimed_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );

    // Verify GB18030 file decoding via `/projects/{id}/file`
    let gbk_meta =
        server::json(c.get(format!("{url_a}/projects/{pid}/file?path=legacy_gbk.c"))).await;
    assert_eq!(gbk_meta["encoding"], "gb18030");
    assert_eq!(gbk_meta["text"], chinese_src);

    // Verify removed worktree transparent re-materialization (`restore_removed_worktree`)
    {
        let repo = kar::workbench_git::Repository::open(&repo_dir).unwrap();
        let wt_id = Uuid::new_v4();
        let wt_path = roots.join("restorable-wt");
        repo.create_worktree(wt_id, &wt_path, "feat/restorable", "HEAD")
            .unwrap();
        assert!(wt_path.exists());
        repo.remove_worktree(wt_id).unwrap();
        assert!(!wt_path.exists());
        let restored = repo.restore_removed_worktree(wt_id).unwrap().unwrap();
        assert_eq!(restored.branch, "feat/restorable");
        assert!(wt_path.join("README.md").exists());
        repo.remove_worktree(wt_id).unwrap();
    }

    // 2. Verify `/server/secrets` encryption + `/forge/credentials/{host}` + Self-Hosted Forgejo Auto-Discovery
    let put_sec = server::json(
        c.put(format!("{url_a}/server/secrets/my_api_token"))
            .json(&json!({"backend": "encrypted", "value": "super-secret-plaintext-999"})),
    )
    .await;
    assert_eq!(put_sec["backend"], "encrypted");
    let raw_vault = fs::read_to_string(wb_dir.join("secrets.enc")).unwrap();
    assert!(!raw_vault.contains("super-secret-plaintext-999"));
    let resolved_sec = server::json(
        c.post(format!("{url_a}/server/secrets/resolve-test"))
            .json(&json!({"names": ["my_api_token"]})),
    )
    .await;
    assert_eq!(resolved_sec["results"][0]["ok"], true);
    assert_eq!(resolved_sec["results"][0]["byte_length"], 26);
    assert_eq!(resolved_sec["results"][0]["redacted"], "••••••");

    // Configure self-hosted Forgejo repo with ONLY an encrypted PAT (no `KAR_FORGE_CONFIG` entry!)
    let selfhosted_dir = roots.join("selfhosted");
    git_init_with_commit(&selfhosted_dir);
    Command::new("git")
        .current_dir(&selfhosted_dir)
        .args([
            "remote",
            "add",
            "origin",
            &format!("http://{mock_addr}/team/selfhosted.git"),
        ])
        .output()
        .unwrap();
    let sh_proj = server::json(
        c.post(format!("{url_a}/projects"))
            .json(&json!({"root": selfhosted_dir, "name": "selfhosted"})),
    )
    .await;
    let sh_pid = sh_proj["id"].as_str().unwrap();
    server::json(
        c.put(format!("{url_a}/forge/credentials/{mock_addr}"))
            .json(&json!({"token": "secret-forgejo-pat-123"})),
    )
    .await;
    let sh_identity = server::json(
        c.post(format!("{url_a}/projects/{sh_pid}/forge/query"))
            .json(&json!({"op": "identity"})),
    )
    .await;
    assert_eq!(sh_identity["kind"], "forgejo");
    assert_eq!(sh_identity["default_branch"], "main");
    assert_eq!(sh_identity["account"], "forgejo-bot");

    // 3. Verify `TextGeneration` endpoints (`/text-generation/*`)
    let commit_gen = server::json(
        c.post(format!("{url_a}/text-generation/commit-message"))
            .json(&json!({"project_id": pid, "diff": "+pub mod persistence;"})),
    )
    .await;
    assert_eq!(
        commit_gen["commit_message"],
        "feat(wal): add framed CRC32 write-ahead log"
    );
    let pr_gen = server::json(c.post(format!("{url_a}/text-generation/pr-content")).json(
        &json!({"project_id": pid, "commit_message": "feat: upgrades", "diff": "+mod wal;"}),
    ))
    .await;
    assert_eq!(pr_gen["title"], "feat: add pure-rust upgrades");
    assert!(pr_gen["body"].as_str().unwrap().contains("## Verification"));
    let branch_gen = server::json(
        c.post(format!("{url_a}/text-generation/branch-name"))
            .json(&json!({"context": "Add pure rust WAL"})),
    )
    .await;
    assert_eq!(branch_gen["branch_name"], "feat/pure-rust-wal");
    let title_gen = server::json(
        c.post(format!("{url_a}/text-generation/thread-title"))
            .json(&json!({"message": "Audit Kar backend against Oar and T3"})),
    )
    .await;
    assert_eq!(title_gen["title"], "Pure Rust Backend Audit");
    let voice_clean = server::json(
        c.post(format!("{url_a}/text-generation/transcription"))
            .json(&json!({
                "text": "呃，um update karchat today",
                "custom_words": ["KarChat"],
                "strip_fillers": true,
                "llm_polish": false
            })),
    )
    .await;
    assert_eq!(voice_clean["text"], "update KarChat today");

    // 4. Verify SEP-3415 Outbound MCP Event Webhooks + SSRF Guard + Secret Rotation
    // Non-allowlisted private IP (`10.0.0.1`) MUST be rejected immediately with -32602!
    let ssrf_blocked = mcp_call(
        &c,
        &url_a,
        "events/subscribe",
        json!({
            "name": "workflow.run_ended",
            "delivery": {
                "mode": "webhook",
                "url": "https://10.0.0.1/hook",
                "secret": format!("whsec_{}", STANDARD.encode(b"0123456789abcdef0123456789abcdef"))
            }
        }),
    )
    .await;
    assert_eq!(ssrf_blocked["error"]["code"], -32027);

    // Subscribe allowlisted `127.0.0.1` receiver, rotate secret, run a workflow, and verify dual-signed delivery!
    let wh_secret_1 = format!(
        "whsec_{}",
        STANDARD.encode(b"11111111111111111111111111111111")
    );
    let wh_secret_2 = format!(
        "whsec_{}",
        STANDARD.encode(b"22222222222222222222222222222222")
    );
    let hook_url = format!("http://{hook_addr}/mcp-hook");
    let sub1 = mcp_call(
        &c,
        &url_a,
        "events/subscribe",
        json!({
            "name": "workflow.run_ended",
            "delivery": {"mode": "webhook", "url": hook_url, "secret": wh_secret_1}
        }),
    )
    .await;
    assert_eq!(sub1["result"]["deliveryStatus"]["active"], true);

    // Rotate secret on same `(name, url)` -> both old and new `v1,` signatures emitted during grace window!
    let sub2 = mcp_call(
        &c,
        &url_a,
        "events/subscribe",
        json!({
            "name": "workflow.run_ended",
            "delivery": {"mode": "webhook", "url": hook_url, "secret": wh_secret_2}
        }),
    )
    .await;
    assert_eq!(sub2["result"]["id"], sub1["result"]["id"]);

    let wf_id = Uuid::new_v4();
    server::json(
        c.put(format!("{url_a}/projects/{pid}/workflows/{wf_id}"))
            .json(&json!({
                "name": "Webhook Smoke",
                "enabled": true,
                "schedule": {"type": "manual"},
                "steps": [{"type": "script", "command": "echo ok"}]
            })),
    )
    .await;
    server::json(
        c.post(format!("{url_a}/projects/{pid}/workflows/{wf_id}/runs"))
            .json(&json!({"operation": Uuid::new_v4(), "input": {}})),
    )
    .await;

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !deliveries.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting for outbound MCP webhook delivery");
    {
        let got = deliveries.lock().unwrap();
        let (sig_header, payload) = &got[0];
        assert_eq!(payload["type"], "event");
        assert_eq!(payload["name"], "workflow.run_ended");
        assert_eq!(
            sig_header.split_whitespace().count(),
            2,
            "rotated secret must sign with both previous and current keys: {sig_header}"
        );
    }

    let unsub = mcp_call(
        &c,
        &url_a,
        "events/unsubscribe",
        json!({"name": "workflow.run_ended", "delivery": {"mode": "webhook", "url": hook_url}}),
    )
    .await;
    assert_eq!(unsub["result"]["unsubscribed"], true);

    // 5. Verify MCP `annotations`, `_meta`, Stacked PR Merge, and Cross-Environment `environment_id` Forwarding
    let listed_tools = mcp_call(&c, &url_a, "tools/list", json!({})).await;
    let tools_arr = listed_tools["result"]["tools"].as_array().unwrap();
    let cap_tool = tools_arr
        .iter()
        .find(|t| t["name"] == "orchestrator_capabilities")
        .unwrap();
    assert_eq!(cap_tool["annotations"]["readOnlyHint"], true);
    assert_eq!(cap_tool["_meta"]["anthropic/alwaysLoad"], true);

    // Stacked PR merge (`stack: true`) merges Layer 1 then retargets & merges Layer 2!
    let stack_dir = roots.join("stacked");
    git_init_with_commit(&stack_dir);
    Command::new("git")
        .current_dir(&stack_dir)
        .args([
            "remote",
            "add",
            "origin",
            "https://stacked.github.example/kar/stacked.git",
        ])
        .output()
        .unwrap();
    let st_proj = server::json(
        c.post(format!("{url_a}/projects"))
            .json(&json!({"root": stack_dir, "name": "stacked"})),
    )
    .await;
    let st_pid = st_proj["id"].as_str().unwrap();
    let st_chat = Uuid::new_v4();
    server::json(
        c.post(format!("{url_a}/projects/{st_pid}/chats"))
            .json(&json!({"id": st_chat, "runtime": "codex"})),
    )
    .await;
    // Verify First-Turn Auto Thread Title Hook!
    server::json(
        c.post(format!(
            "{url_a}/projects/{st_pid}/chats/{st_chat}/messages"
        ))
        .json(&json!({"id": Uuid::new_v4(), "text": "Audit Kar backend against Oar and T3"})),
    )
    .await;
    let st_meta = server::json(c.get(format!(
        "{url_a}/projects/{st_pid}/chats/{st_chat}/metadata"
    )))
    .await;
    assert_eq!(st_meta["title"], "Pure Rust Backend Audit");

    let merge_res = server::json(
        c.post(format!("{url_a}/projects/{st_pid}/forge/operations"))
            .json(&json!({
                "id": Uuid::new_v4(),
                "chat": st_chat,
                "op": "pull_command",
                "number": 2,
                "command": {
                    "action": "merge",
                    "expected_head_sha": "sha-layer-2",
                    "method": "squash",
                    "stack": true
                }
            })),
    )
    .await;
    assert_eq!(merge_res["stack"], true);
    assert_eq!(merge_res["merged_layers"], json!([1, 2]));
    assert_eq!(*merged_prs.lock().unwrap(), vec![1, 2]);

    // Pair a second Kar workbench (`host_b`) and test cross-environment `environment_id` forwarding
    let state_b = base.join("state-b");
    let roots_b = base.join("projects-b");
    let repo_b = roots_b.join("remote-service");
    git_init_with_commit(&repo_b);
    let native_b = base.join("native-b");
    fs::create_dir_all(&native_b).unwrap();
    let (mut host_b, url_b) = server::serve(&state_b, &roots_b, &native_b).await;
    let proj_b = server::json(
        c.post(format!("{url_b}/projects"))
            .json(&json!({"root": repo_b, "name": "remote-service"})),
    )
    .await;
    let pid_b = proj_b["id"].as_str().unwrap().to_owned();
    let chat_b = Uuid::new_v4();
    server::json(
        c.post(format!("{url_b}/projects/{pid_b}/chats"))
            .json(&json!({"id": chat_b, "runtime": "codex"})),
    )
    .await;
    let env_b_info = server::json(c.get(format!("{url_b}/environment"))).await;
    let env_b_id = env_b_info["environment_id"].as_str().unwrap().to_owned();

    // Issue operate-scoped peer-link pairing grant on B and link A -> B directly
    let pair_grant = server::json(
        c.post(format!("{url_b}/api/auth/pairing-token"))
            .json(&json!({
                "purpose": "peer-link",
                "access": "approval-required",
                "peer_tasks": {(pid_b.as_str()): chat_b}
            })),
    )
    .await;
    let pair_secret = pair_grant["credential"].as_str().unwrap();
    let linked_ab = server::json(c.post(format!("{url_a}/environment/links")).json(&json!({
        "url": url_b,
        "credential": pair_secret,
        "environment_id": env_b_id
    })))
    .await;
    assert_eq!(linked_ab["status"], "linked");
    assert_eq!(linked_ab["environment_id"], env_b_id);

    let links_list = server::json(c.get(format!("{url_a}/environment/links"))).await;
    assert_eq!(links_list["links"][0]["online"], true);

    // Forward `kar_project_list` across `environment_id` from A -> B!
    let remote_projs = mcp_call(
        &c,
        &url_a,
        "tools/call",
        json!({"name": "kar_project_list", "arguments": {"environment_id": env_b_id}}),
    )
    .await;
    assert_eq!(
        remote_projs["result"]["structuredContent"]["projects"][0]["id"],
        pid_b
    );

    // Forward `kar_thread_send` across `environment_id` from A -> B and wait for turn completion!
    let remote_cmd = Uuid::new_v4();
    let remote_send = mcp_call(
        &c,
        &url_a,
        "tools/call",
        json!({
            "name": "kar_thread_send",
            "arguments": {
                "environment_id": env_b_id,
                "project_id": pid_b,
                "thread_id": chat_b,
                "command_id": remote_cmd,
                "text": "hello across paired workbenches"
            }
        }),
    )
    .await;
    assert_eq!(
        remote_send["result"]["structuredContent"]["command_id"],
        remote_cmd.to_string(),
        "{remote_send}"
    );

    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let remote_thread = mcp_call(
                &c,
                &url_a,
                "tools/call",
                json!({
                    "name": "kar_thread_read",
                    "arguments": {
                        "environment_id": env_b_id,
                        "project_id": pid_b,
                        "thread_id": chat_b
                    }
                }),
            )
            .await;
            let text = remote_thread["result"]["structuredContent"].to_string();
            if text.contains("hello across paired workbenches") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting for cross-environment kar_thread_send turn");

    // Verify operational endpoints on A
    let doctor = server::json(c.get(format!("{url_a}/server/doctor"))).await;
    assert_eq!(doctor["platform"], "macos");
    let policy = server::json(c.get(format!("{url_a}/server/policy"))).await;
    assert!(policy.is_object());
    let svc_status = server::json(c.get(format!("{url_a}/server/service"))).await;
    assert_eq!(svc_status["label"], "dev.kar.workbench");
    let ts_status = server::json(c.get(format!("{url_a}/environments/tailscale"))).await;
    assert!(ts_status["tailscale"].is_object());
    let ssh_hosts = server::json(c.get(format!("{url_a}/environments/ssh/hosts"))).await;
    assert!(ssh_hosts["hosts"].is_array());

    host_b.kill().await.unwrap();
    host_b.wait().await.unwrap();
    host_a.kill().await.unwrap();
    host_a.wait().await.unwrap();
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("codex 0.143.0");
            return;
        }
        Some("app-server") => {
            peer::agent();
            return;
        }
        _ => {}
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        test_sdk_durable_voyage_and_scripted().await;
        test_workbench_wave2_end_to_end().await;
    });
}
