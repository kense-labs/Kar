use super::{SECRET, opts};
use kar::account::AccountUsageOptions;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead},
    path::Path,
    time::Duration,
};

fn rpc_reply(req: &Value, result: Value) {
    println!(
        "{}",
        json!({"jsonrpc":"2.0","id":req["id"],"result":result})
    );
}
fn rpc_error(req: &Value, code: i32, msg: &str) {
    println!(
        "{}",
        json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":code,"message":msg}})
    );
}
fn control_reply(req: &Value, response: Value) {
    assert_eq!(req["type"], "control_request");
    println!(
        "{}",
        json!({"type":"control_response","response":{"request_id":req["request_id"],"subtype":"success","response":response}})
    );
}

// Only these account commands and frames are accepted. A session or prompt fails the test.
pub(super) fn peer(id: &str, case: &str, args: &[&str]) {
    let stdin = io::stdin();
    let mut frames = stdin.lock().lines();
    let mut next = || serde_json::from_str::<Value>(&frames.next().unwrap().unwrap()).unwrap();
    match id {
        "codex" => {
            assert_eq!(args, ["app-server", "--listen", "stdio://"]);
            let req = next();
            assert_eq!(req["method"], "initialize");
            assert_eq!(req["params"]["capabilities"]["experimentalApi"], true);
            rpc_reply(&req, json!({}));
            assert_eq!(next()["method"], "initialized");
            let req = next();
            assert_eq!(req["method"], "account/read");
            if case == "key" {
                rpc_reply(&req, json!({"account":{"type":"apiKey"}}));
                return;
            }
            rpc_reply(
                &req,
                json!({"account":{"type":"chatgpt","email":"quota@example.org"}}),
            );
            let mut req = next();
            if case.starts_with("reset_") {
                assert_eq!(req["method"], "account/rateLimitResetCredit/consume");
                assert_eq!(
                    req["params"],
                    json!({"idempotencyKey":"stable-attempt","creditId":"credit-1"})
                );
                if case == "reset_drop" {
                    return;
                }
                if case == "reset_old" {
                    rpc_error(&req, -32601, "Method not found");
                    return;
                }
                let first = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open("synthetic-reset-receipt")
                    .is_ok();
                rpc_reply(
                    &req,
                    json!({"outcome":if first{"reset"}else{"alreadyRedeemed"}}),
                );
                return;
            }
            assert_eq!(req["method"], "account/rateLimits/read");
            if case == "legacy" {
                assert_eq!(req["params"], json!({"excludeResetCreditDetails":true}));
                rpc_error(&req, -32602, "invalid type: map, expected unit");
                req = next();
                assert_eq!(req["method"], "account/rateLimits/read");
                assert!(req["params"].is_null());
            }
            let b = json!({"limitId":"codex","planType":"pro","primary":{"usedPercent":99.99999,"resetsAt":1700000000,"windowDurationMins":300}});
            let credits = if case == "legacy" {
                Value::Null
            } else {
                json!([{"id":"r1","resetType":"codexRateLimits","title":SECRET,"status":"available"}])
            };
            rpc_reply(
                &req,
                json!({"rateLimits":b,"rateLimitsByLimitId":{"codex":b,"model":{"limitId":"some-model","limitName":"One model","primary":{"usedPercent":100},"rateLimitReachedType":"limit"}},"rateLimitResetCredits":{"availableCount":1,"credits":credits}}),
            );
        }
        "claude" => {
            assert_eq!(
                args,
                [
                    "-p",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--no-session-persistence",
                    "--safe-mode"
                ]
            );
            assert!(std::env::var_os("CLAUDECODE").is_none());
            let init = next();
            assert_eq!(init["request"]["subtype"], "initialize");
            control_reply(
                &init,
                json!({"account":{"email":"quota@example.org","organization":"Test Org"}}),
            );
            let req = next();
            assert_eq!(
                req["request"],
                json!({"subtype":"get_usage","skip_behaviors":true})
            );
            if case == "out" {
                println!(
                    "{}",
                    json!({"type":"control_response","response":{"request_id":req["request_id"],"subtype":"error","error":"Please run /login"}})
                );
                return;
            }
            control_reply(
                &req,
                json!({"rate_limits_available":true,"rate_limits":{"seven_day":{"utilization":100},"model_scoped":[{"display_name":"Opus","utilization":12}],"extra_usage":{"is_enabled":true,"monthly_limit":null,"utilization":null}},"subscription_type":"native"}),
            );
        }
        "grok" => {
            assert_eq!(args, ["agent", "--no-leader", "stdio"]);
            let init = next();
            assert_eq!(init["method"], "initialize");
            rpc_reply(
                &init,
                json!({"protocolVersion":1,"authMethods":[{"id":"cached_token"}]}),
            );
            let auth = next();
            assert_eq!(auth["method"], "authenticate");
            assert_eq!(auth["params"]["methodId"], "cached_token");
            rpc_reply(&auth, json!({}));
            let req = next();
            assert_eq!(req["method"], "_x.ai/billing");
            match case {
                "out" => rpc_error(&req, -32000, "authentication required"),
                "error" => {
                    rpc_error(&req, -32603, SECRET);
                    return;
                }
                _ => rpc_reply(
                    &req,
                    json!({"subscription_tier":"team","config":{"creditUsagePercent":100,"onDemandCap":{"val":200},"onDemandUsed":{"val":100}}}),
                ),
            }
            let info = next();
            assert_eq!(info["method"], "_x.ai/auth/info");
            if case == "slow" {
                std::thread::sleep(Duration::from_secs(30));
                return;
            }
            rpc_reply(
                &info,
                json!({"methodId":null,"email":"quota@example.org","token":SECRET}),
            );
        }
        "antigravity" => match args {
            ["acp"] if case == "tier" => {
                assert!(std::env::var_os("DISPLAY").is_none());
                assert!(std::env::var_os("WAYLAND_DISPLAY").is_none());
                assert!(std::env::var_os("SSH_CONNECTION").is_some());
                let req = next();
                assert_eq!(req["method"], "initialize");
                rpc_reply(
                    &req,
                    json!({"protocolVersion":1,"agentCapabilities":{"_meta":{"antigravity":{"tier":true}}},"authMethods":[{"id":"oauth-business"}]}),
                );
                let req = next();
                assert_eq!(req["method"], "authenticate");
                assert_eq!(req["params"]["methodId"], "oauth-business");
                rpc_reply(
                    &req,
                    json!({"_meta":{"antigravity":{"tier":{"name":"Business Plan"}}}}),
                );
            }
            ["models"] if case == "out" => {
                eprintln!("Please sign in.");
                std::process::exit(1);
            }
            ["models"] => println!("model"),
            ["-p", "/usage", "--output-format", "json"] => {
                let left = if matches!(case, "mixed" | "tier") {
                    1e-16
                } else {
                    0.0
                };
                println!(
                    "{}",
                    json!({"command":{"name":"usage","data":{"groups":[{"name":"First","buckets":[{"name":"Weekly","window":"weekly","remaining_fraction":0}]},{"name":"Second","buckets":[{"id":"s","window":"5h","remaining_fraction":left}]}]}},"status":"SUCCESS"})
                );
            }
            ["-p", "/credits", "--output-format", "json"] if case == "exhausted" => println!(
                "{}",
                json!({"status":"SUCCESS","command":{"name":"credits","data":{"remaining_credits":0}}})
            ),
            _ => panic!("unexpected Antigravity quota operation"),
        },
        _ => panic!("unexpected quota reader"),
    }
}

async fn read(id: &str, case: &str, home: &Path) -> kar::Result<Value> {
    let mut rt = kar::runtime(id)?;
    rt.program = std::env::current_exe().unwrap();
    let mut installation = opts(home, id, &format!("quota_{case}")).installation;
    if case == "storage" {
        installation.env.insert(
            "GEMINI_HOME".into(),
            Some(home.join("other").into_os_string()),
        );
    }
    if id == "codex" {
        installation
            .env
            .insert("OPENAI_API_KEY".into(), Some(SECRET.into()));
    }
    let timeout = if case == "slow" {
        Duration::from_millis(900)
    } else {
        Duration::from_secs(8)
    };
    let options = AccountUsageOptions {
        installation,
        timeout: Some(timeout),
        limit_reset_details: case != "legacy",
    };
    Ok(serde_json::to_value(rt.account_usage(&options).await?).unwrap())
}
pub(super) async fn exercise() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let mut rt = kar::runtime("codex").unwrap();
    rt.program = std::env::current_exe().unwrap();
    let mut reset = kar::account::LimitResetOptions::new("stable-attempt");
    reset.reset_id = Some("credit-1".into());
    reset.installation = opts(h, "codex", "quota_reset_once").installation;
    use kar::account::LimitResetResult;
    assert_eq!(
        rt.redeem_limit_reset(&reset).await.unwrap(),
        LimitResetResult::Reset
    );
    assert_eq!(
        rt.redeem_limit_reset(&reset).await.unwrap(),
        LimitResetResult::AlreadyRedeemed
    );
    reset.installation = opts(h, "codex", "quota_reset_old").installation;
    assert!(matches!(
        rt.redeem_limit_reset(&reset).await.unwrap(),
        LimitResetResult::Unsupported { .. }
    ));
    reset.installation = opts(h, "codex", "quota_reset_drop").installation;
    assert!(
        rt.redeem_limit_reset(&reset)
            .await
            .unwrap_err()
            .to_string()
            .contains("same idempotency key")
    );
    reset.idempotency_key.clear();
    assert!(
        rt.redeem_limit_reset(&reset)
            .await
            .unwrap_err()
            .to_string()
            .contains("empty")
    );
    for case in ["ok", "legacy"] {
        let v = read("codex", case, h).await.unwrap();
        assert!(!v.to_string().contains(SECRET));
        assert_eq!(v["kind"], "available");
        assert_eq!(v["plan"], "pro");
        assert_eq!(v["windows"].as_array().unwrap().len(), 2);
        assert_eq!(v["rateLimited"], false);
        assert_eq!(v["windows"][0]["usedRatio"], 0.999999);
        assert_eq!(v["windows"][0]["resetsAt"], "2023-11-14T22:13:20.000Z");
        assert_eq!(
            v["windows"][1]["scope"],
            json!({"kind":"model","name":"One model"})
        );
        assert_eq!(v["limitResets"]["available"], 1);
        if case == "legacy" {
            assert!(v["limitResets"].get("resets").is_none());
        } else {
            assert_eq!(
                v["limitResets"]["resets"],
                json!([{"id":"r1","status":"available"}])
            );
        }
    }
    assert_eq!(
        read("codex", "key", h).await.unwrap()["reason"],
        "unsupported_auth_mode"
    );
    fs::write(h.join(".claude.json"),json!({"oauthAccount":{"emailAddress":"QUOTA@example.org","organizationName":"Test Org","planDisplayName":"Team Plus"}}).to_string()).unwrap();
    let v = read("claude", "ok", h).await.unwrap();
    assert_eq!(v["plan"], "Team Plus");
    assert_eq!(v["rateLimited"], false);
    assert_eq!(v["windows"][1]["scope"]["name"], "Opus");
    assert_eq!(
        read("claude", "out", h).await.unwrap()["lastAccount"]["email"],
        "quota@example.org"
    );
    assert_eq!(
        read("claude", "old", h).await.unwrap()["reason"],
        "unsupported_installation"
    );
    for case in ["ok", "slow"] {
        let v = read("grok", case, h).await.unwrap();
        assert_eq!(v["plan"], "team");
        assert_eq!(v["rateLimited"], false);
        assert_eq!(v["windows"][1]["usedRatio"], 0.5);
        assert_eq!(v["email"].is_null(), case == "slow");
    }
    assert_eq!(
        read("grok", "out", h).await.unwrap()["lastAccount"]["email"],
        "quota@example.org"
    );
    let err = read("grok", "error", h).await.unwrap_err();
    assert!(!err.to_string().contains(SECRET));
    let state = h.join(".gemini/antigravity-cli/cache");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("acp-auth-state.json"),
        r#"{"methodId":"oauth-business"}"#,
    )
    .unwrap();
    let tier = read("antigravity", "tier", h).await.unwrap();
    assert_eq!(tier["plan"], "Business Plan");
    fs::remove_file(state.join("acp-auth-state.json")).unwrap();
    let v = read("antigravity", "mixed", h).await.unwrap();
    assert!(v.get("credits").is_none());
    assert_eq!(v["windows"][1]["usedRatio"], 0.999999);
    let v = read("antigravity", "exhausted", h).await.unwrap();
    assert_eq!(v["rateLimited"], true);
    assert_eq!(v["credits"]["balance"], "0");
    assert_eq!(
        read("antigravity", "storage", h).await.unwrap()["reason"],
        "unsupported_auth_storage"
    );
    assert_eq!(
        read("antigravity", "out", h).await.unwrap()["kind"],
        "reauth_required"
    );
    assert_eq!(
        read("qwen", "unsupported", h).await.unwrap()["reason"],
        "capability_unavailable"
    );
}
