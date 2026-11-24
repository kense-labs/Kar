use super::*;
use kar::forms::{ListingReason, ModelListing};

pub(super) fn peer(mode: &str) {
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
    Ok(())
}
