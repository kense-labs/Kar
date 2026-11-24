use super::*;
use kar::forms::{ListingReason, ModelListing};

pub(super) fn peer(mode: &str) {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("codex-cli 0.171.0");
        return;
    }
    if mode == "stalled" {
        std::thread::sleep(std::time::Duration::from_secs(8));
        return;
    }
    for line in std::io::stdin()
        .lock()
        .lines()
        .map_while(std::result::Result::ok)
    {
        let request: Value = serde_json::from_str(&line).unwrap();
        let result = match request["method"].as_str().unwrap() {
            "initialize" => json!({}),
            "initialized" => continue,
            "model/list" if request["params"]["cursor"].is_null() => {
                json!({"data":[{"id":"legacy-id","model":"a","isDefault":true}, {"model":"hidden","hidden":true}],"nextCursor":"next"})
            }
            "model/list" => {
                assert_eq!(request["params"]["cursor"], "next");
                json!({"data":[{"id":"raw-b","model":"b","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"high"}],"defaultReasoningEffort":"low","serviceTiers":[{"id":"priority","name":"Priority"},{"id":"priority"}],"defaultServiceTier":"priority"}],"nextCursor":if mode=="repeated"{Some("next")}else{None}})
            }
            "config/read" => {
                assert!(Path::new(request["params"]["cwd"].as_str().unwrap()).is_absolute());
                assert_eq!(request["params"]["includeLayers"], false);
                json!({"config":{"model":mode,"private":"private-sentinel"}})
            }
            method => panic!("unexpected method {method} in pre-session listing"),
        };
        emit(json!({"id":request["id"],"result":result}));
    }
}

use std::path::Path;
pub(super) async fn run() -> kar::Result<()> {
    let mut rt = kar::runtime("codex")?;
    rt.program = std::env::current_exe()?;
    let tmp = TempDir::new()?;
    let mut opts = SessionOptions::new(tmp.path());
    for (name, selected) in [("b", true), ("unlisted", false)] {
        opts.env
            .insert("KAR_TEST_CODEX_CATALOG".into(), name.into());
        let list = rt.form_model_listing(&opts, vec![]).await;
        let rows = list.models();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "a");
        assert!(!rows[0].default);
        assert_eq!(rows[1].id, "b");
        assert_eq!(rows[1].default, selected);
        assert_eq!(rows[1].effort_levels.as_deref().unwrap(), ["low", "high"]);
        assert_eq!(rows[1].metadata["serviceTiers"], json!(["priority"]));
        assert_eq!(
            rows[1].metadata["serviceTierDetails"],
            json!([{"id":"priority","displayName":"Priority"}])
        );
        assert!(!serde_json::to_string(&list)?.contains("private-sentinel"));
    }
    let raw = rt.catalog(kar::Catalog::Models, opts.clone()).await?;
    assert_eq!(raw[0]["id"], "legacy-id");
    assert_eq!(raw[1]["hidden"], true);
    opts.env
        .insert("KAR_TEST_CODEX_CATALOG".into(), "repeated".into());
    assert!(matches!(
        rt.form_model_listing(&opts, vec![]).await,
        ModelListing::Unavailable {
            reason: ListingReason::Failed
        }
    ));
    let menu = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["models", "codex", "--picker", "--bin"])
        .arg(&rt.program)
        .arg("--cwd")
        .arg(tmp.path())
        .args(["--timeout-ms", "10000"])
        .env("KAR_TEST_CODEX_CATALOG", "b")
        .output()
        .await?;
    assert!(
        menu.status.success(),
        "{}",
        String::from_utf8_lossy(&menu.stderr)
    );
    let menu: Value = serde_json::from_slice(&menu.stdout)?;
    assert_eq!(menu["status"], "live");
    assert_eq!(menu["models"][1]["id"], "b");
    assert_eq!(menu["models"][1]["default"], true);
    // CLI reports isolate failures per installation, including when a deadline expires.
    for (selection, format, mode, succeeds) in [
        (None, "--json", "b", true),
        (Some("all"), "--json", "b", true),
        (Some("codex"), "--text", "b", true),
        (Some("all"), "--json", "stalled", false),
    ] {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"));
        cmd.arg("models")
            .args(selection)
            .arg(format)
            .arg("--cwd")
            .arg(tmp.path())
            .args(["--timeout-ms", if succeeds { "5000" } else { "1200" }])
            .env("KAR_TEST_CODEX_CATALOG", mode);
        for provider in kar::runtimes().into_iter().chain([kar::runtime("morph")?]) {
            cmd.env(
                format!("KAR_{}_BIN", provider.id.to_ascii_uppercase()),
                if provider.id == "codex" {
                    rt.program.clone()
                } else {
                    tmp.path().join("absent")
                },
            );
        }
        let out = cmd.output().await?;
        assert_eq!(
            out.status.success(),
            succeeds,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if format == "--text" {
            assert!(
                String::from_utf8_lossy(&out.stdout)
                    .contains("codex\tb effort [low*,high] tiers [priority*] (default)")
            );
            continue;
        }
        let rows: Vec<Value> = serde_json::from_slice(&out.stdout)?;
        assert_eq!(rows.len(), kar::runtimes().len() + 2);
        let row = |id: &str| rows.iter().find(|r| r["runtime"] == id).unwrap();
        assert_eq!(row("pi")["installation"]["kind"], "missing");
        if succeeds {
            assert_eq!(row("codex")["choices"], menu);
        } else {
            assert!(row("codex")["error"].is_string());
        }
    }
    Ok(())
}
