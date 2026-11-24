use super::*;
use kar::forms::{ListingReason, ModelListing};

fn log(line: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var_os("KAR_TEST_MENU_LOG").unwrap())
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

pub(super) fn peer(vendor: &str, args: &[String]) {
    let mode = std::env::var("KAR_TEST_MENU_MODE").unwrap();
    log(&args.join(" "));
    if args == ["auth", "status"] {
        match mode.as_str() {
            "empty-in" => println!("Logged in (via Devin)."),
            "empty-out" => println!("Not logged in."),
            _ => println!("Failed to load credentials: invalid TOML"),
        }
        return;
    }
    assert_eq!(
        args,
        if vendor == "codebuddy" {
            vec!["--acp", "--permission-mode", "bypassPermissions"]
        } else {
            vec!["acp"]
        }
    );
    for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
        let q: Value = serde_json::from_str(&line).unwrap();
        let method = q["method"].as_str().unwrap();
        log(method);
        let reply = match method {
            "initialize" => json!({"protocolVersion":1,"authMethods":[{"id":"login"}],
                "agentCapabilities":{"sessionCapabilities":{"delete":{},"close":{}}}}),
            "authenticate" => {
                assert_eq!(vendor, "kimi");
                assert_eq!(q["params"]["methodId"], "login");
                json!({})
            }
            "session/new" if mode == "auth" || mode == "missing-method" => {
                let error = if mode == "auth" {
                    json!({"code":-32603,"message":"Authentication required: sensitive"})
                } else {
                    json!({"code":-32601,"message":"Method not found"})
                };
                emit(json!({"jsonrpc":"2.0","id":q["id"],"error":error}));
                continue;
            }
            "session/new" => {
                assert_eq!(
                    q["params"]["cwd"].as_str().unwrap(),
                    std::env::current_dir().unwrap().to_str().unwrap()
                );
                assert_eq!(q["params"]["mcpServers"], json!([]));
                let levels = if vendor == "kimi" {
                    json!([{"value":"off"},{"value":{"effort":"high"}}])
                } else if vendor == "codebuddy" {
                    json!([{"value":"enabled"},{"value":"high"},{"value":"disabled"}])
                } else {
                    json!([{"value":"low"},{"value":"high"}])
                };
                let options = if mode.starts_with("empty") {
                    json!([])
                } else {
                    json!([{"value":"a","name":"a"},{"value":"b","name":"Bee"}])
                };
                let menus = if mode == "no-menu" {
                    json!([])
                } else {
                    json!([{"id":"model","currentValue":"a","options":options},{"category":"thought_level","id":"vendor-thought","currentValue":"high","options":levels}])
                };
                json!({"sessionId":"temporary","configOptions":menus})
            }
            "session/close" | "session/delete" => {
                assert_eq!(q["params"]["sessionId"], "temporary");
                json!({})
            }
            other => panic!("unexpected query {other}"),
        };
        emit(json!({"jsonrpc":"2.0","id":q["id"],"result":reply}));
    }
}

pub(super) async fn run() -> kar::Result<()> {
    let tmp = TempDir::new()?;
    for vendor in ["kimi", "codebuddy", "devin"] {
        let mut runtime = kar::runtime(vendor)?;
        runtime.program = std::env::current_exe()?;
        let mut opts = SessionOptions::new(tmp.path().canonicalize()?);
        let log = tmp.path().join("calls");
        opts.env.insert("KAR_TEST_MENU".into(), vendor.into());
        opts.env
            .insert("KAR_TEST_MENU_LOG".into(), log.clone().into_os_string());
        opts.unset_env.insert("WINDSURF_API_KEY".into());
        opts.env.insert("KAR_TEST_MENU_MODE".into(), "ok".into());
        let menu = runtime.form_model_listing(&opts, vec![]).await;
        let rows = menu.models();
        assert_eq!(rows.len(), 2, "{vendor}: {menu:?}");
        assert!(rows[0].default);
        assert_eq!(rows[0].metadata["defaultEffort"], "high");
        assert_eq!(
            rows[0].metadata.contains_key("displayName"),
            vendor == "kimi"
        );
        assert_eq!(
            rows[0].effort_levels.as_ref().unwrap().len(),
            if vendor == "devin" { 2 } else { 1 }
        );
        assert_eq!(rows[1].metadata["displayName"], "Bee");
        assert!(rows[1].effort_levels.is_none());
        let calls = std::fs::read_to_string(&log)?;
        let expected = match vendor {
            "kimi" => "acp\ninitialize\nauthenticate\nsession/new\nsession/close\n",
            "devin" => "acp\ninitialize\nsession/new\nsession/delete\n",
            _ => "--acp --permission-mode bypassPermissions\ninitialize\nsession/new\n",
        };
        assert_eq!(calls, expected);
        for (mode, expected) in [
            ("auth", ListingReason::Unauthenticated),
            ("no-menu", ListingReason::Unsupported),
        ] {
            opts.env.insert("KAR_TEST_MENU_MODE".into(), mode.into());
            assert_eq!(
                runtime.form_model_listing(&opts, vec![]).await,
                ModelListing::Unavailable { reason: expected }
            );
        }
        if vendor == "devin" {
            for (mode, reason) in [
                ("empty-in", ListingReason::Empty),
                ("empty-out", ListingReason::Unauthenticated),
                ("empty-unknown", ListingReason::Failed),
            ] {
                opts.env.insert("KAR_TEST_MENU_MODE".into(), mode.into());
                assert_eq!(
                    runtime.form_model_listing(&opts, vec![]).await,
                    ModelListing::Unavailable { reason }
                );
            }
            opts.env
                .insert("WINDSURF_API_KEY".into(), "unverified-key".into());
            assert_eq!(
                runtime.form_model_listing(&opts, vec![]).await,
                ModelListing::Unavailable {
                    reason: ListingReason::Unsupported
                }
            );
        }
        if vendor == "kimi" {
            opts.env
                .insert("KAR_TEST_MENU_MODE".into(), "missing-method".into());
            assert_eq!(
                runtime.form_model_listing(&opts, vec![]).await,
                ModelListing::Unavailable {
                    reason: ListingReason::Unsupported
                }
            );
        }
        std::fs::remove_file(log)?;
    }
    Ok(())
}
