use super::*;
use kar::forms::{ListingReason, ModelListing};

fn state(model: &str, effort: &str) -> Value {
    let levels: &[&str] = if model == "a" {
        &["high", "low"]
    } else {
        &["max"]
    };
    json!({"sessionId":"probe","configOptions":[
        {"id":"model","currentValue":model,"options":[{"value":"a"},{"value":"b"}]},
        {"id":"thinking","category":"thought_level","currentValue":effort,"options":levels.iter().map(|s|json!({"value":s})).collect::<Vec<_>>()}],
        "models":{"availableModels":[{"modelId":"b-old"},{"modelId":"unknown-id","name":"Legacy"}]}})
}

pub(super) fn peer(mode: &str, args: &[String]) {
    let log_path = std::env::var_os("KAR_AGY_PICKER_LOG").unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .unwrap();
    if args == ["--version"] {
        writeln!(file, "--version").unwrap();
        println!(
            "{}",
            match mode {
                "old" => "1.4.9",
                "prerelease" => "1.6.0-beta",
                "build" => "1.5.0+unverified",
                _ => "1.5.0",
            }
        );
        return;
    }
    assert_eq!(
        args[0..4],
        [
            "acp",
            "--dangerously-skip-permissions",
            "--process-per-session",
            "--gemini-dir"
        ]
    );
    assert_eq!(
        Some(std::ffi::OsStr::new(&args[4])),
        std::env::var_os("GEMINI_HOME").as_deref()
    );
    let mut calls = 0;
    for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
        let req: Value = serde_json::from_str(&line).unwrap();
        let method = req["method"].as_str().unwrap();
        writeln!(file, "{method}").unwrap();
        let body = match method {
            "initialize" => {
                json!({"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"close":{}}}})
            }
            "session/new" => {
                let mut result = state("a", "low");
                if mode == "unrestorable" {
                    result["configOptions"][1]["currentValue"] = "unknown".into();
                }
                result
            }
            "session/set_config_option" => {
                assert_eq!(req["params"]["sessionId"], "probe");
                calls += 1;
                let key = req["params"]["configId"].as_str().unwrap();
                let value = req["params"]["value"].as_str().unwrap();
                writeln!(file, "{key}={value}").unwrap();
                if mode == "missing" || (mode == "fail-restore" && calls == 3) {
                    emit(
                        json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":if mode=="missing"{-32601}else{-32000},"message":"refused"}}),
                    );
                    continue;
                }
                if mode == "wrong" && value == "b" {
                    state("a", "low")
                } else if key == "model" {
                    state(value, if value == "a" { "high" } else { "max" })
                } else {
                    assert_eq!((key, value), ("thinking", "low"));
                    state("a", value)
                }
            }
            "session/close" => json!({}),
            m => panic!("unexpected method in listing {m}"),
        };
        emit(json!({"jsonrpc":"2.0","id":req["id"],"result":body}));
    }
}

pub(super) async fn run() -> kar::Result<()> {
    let temp = TempDir::new()?;
    let mut rt = kar::runtime("antigravity")?;
    rt.program = std::env::current_exe()?;
    let mut opts = SessionOptions::new(temp.path().canonicalize()?);
    let log = temp.path().join("calls");
    opts.env
        .insert("KAR_AGY_PICKER_LOG".into(), log.clone().into_os_string());
    opts.env
        .insert("GEMINI_HOME".into(), temp.path().as_os_str().into());
    for mode in [
        "ok",
        "missing",
        "unrestorable",
        "wrong",
        "fail-restore",
        "old",
        "prerelease",
        "build",
    ] {
        opts.env.insert("KAR_TEST_AGY_PICKER".into(), mode.into());
        let result = rt.form_model_listing(&opts, vec![]).await;
        let calls = std::fs::read_to_string(&log)?;
        std::fs::remove_file(&log)?;
        if matches!(mode, "old" | "prerelease" | "build") {
            assert_eq!(
                result,
                ModelListing::Unavailable {
                    reason: ListingReason::Unsupported
                }
            );
            assert_eq!(calls, "--version\n");
            continue;
        }
        assert!(calls.ends_with("session/close\n"));
        if mode == "fail-restore" {
            assert_eq!(
                result,
                ModelListing::Unavailable {
                    reason: ListingReason::Failed
                }
            );
            continue;
        }
        let rows = result.models();
        assert_eq!(rows.len(), 3, "{mode}: {result:?}");
        assert!(rows[0].default);
        assert_eq!(rows[2].id, "unknown-id");
        assert!(rows[2].metadata.get("effortUnavailableReason").is_some());
        match mode {
            "ok" => {
                assert_eq!(rows[1].effort_levels.as_deref().unwrap(), ["max"]);
                assert!(!rows[1].metadata.contains_key("defaultEffort"));
                assert_eq!(rows[0].metadata["defaultEffort"], "low");
                assert!(calls.ends_with(
                    "model=a\nsession/set_config_option\nthinking=low\nsession/close\n"
                ));
            }
            "wrong" => {
                assert!(rows[1].effort_levels.is_none());
                assert!(rows[1].metadata.get("effortUnavailableReason").is_some());
            }
            "missing" | "unrestorable" => {
                assert_eq!(rows[1].effort_levels.as_deref().unwrap(), ["old"]);
                assert_eq!(
                    calls.matches("session/set_config_option\n").count(),
                    usize::from(mode == "missing")
                );
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}
