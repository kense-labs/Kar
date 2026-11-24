use super::*;
use kar::{Catalog, Error, Event, InstallationSnapshot, Runtime};

const UNKNOWN: &str = "kar-no-such-effort";

fn configure(f: &Fixture, model: &str, reasoning: Option<Value>) {
    let settings = f.dir.path().join("home/.qwen/settings.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    let entry = &mut config["modelProviders"]["openai"][0];
    entry["id"] = model.into();
    if let Some(menu) = reasoning {
        entry["capabilities"]["reasoning"] = menu;
    } else {
        entry.as_object_mut().unwrap().remove("capabilities");
    }
    config["model"]["name"] = model.into();
    std::fs::write(&settings, config.to_string()).unwrap();
}

async fn refusal(rt: &Runtime, options: &SessionOptions, level: &str) -> String {
    let mut options = options.clone();
    options.settings.effort = Some(level.into());
    match rt.open(options).await {
        Err(Error::Refused(detail)) => {
            assert!(
                detail.contains(level),
                "missing requested level in {detail}"
            );
            detail
        }
        Err(other) => panic!("unrelated open failure: {other}"),
        Ok(session) => {
            session.close().await.unwrap();
            panic!("Qwen accepted unavailable effort {level}");
        }
    }
}

#[tokio::test]
#[ignore = "requires actual Qwen Code with a local model"]
async fn effort_never_substituted() {
    let f = fixture().await;
    let rt = runtime("qwen").unwrap();
    let mut options = f.opts.clone();
    options.model = None;
    let detail = refusal(&rt, &options, UNKNOWN).await;
    assert!(detail.contains("Invalid effort value"), "{detail}");
    assert!(f.requests.lock().unwrap().is_empty());
    f.http.abort();
}

#[tokio::test]
#[ignore = "requires actual Qwen Code with native model discovery and a local model"]
async fn effort_listed_levels_apply_across_resume() {
    let f = fixture().await;
    let rt = runtime("qwen").unwrap();
    let installation = rt
        .probe_installation(&f.opts.installation_options())
        .await
        .unwrap();
    assert!(matches!(
        installation,
        InstallationSnapshot::Available { .. }
    ));
    let mut options = f.opts.clone();
    options.model = None;
    configure(
        &f,
        "gpt-5.1",
        Some(json!({"efforts":["low","medium","high"],"defaultEffort":"medium"})),
    );
    let listing = rt.catalog(Catalog::Models, options.clone()).await.unwrap();
    let rows = listing.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let model = &rows[0];
    assert_eq!(model["id"], "gpt-5.1");
    assert_eq!(model["displayName"], "Loopback model");
    assert_eq!(
        model["effortLevels"],
        model["capabilities"]["reasoning"]["efforts"]
    );
    assert_eq!(model["effortLevels"], json!(["low", "medium", "high"]));
    assert_eq!(model["defaultEffort"], "medium");
    options.model = Some(model["id"].as_str().unwrap().into());
    assert!(
        refusal(&rt, &options, "xhigh")
            .await
            .contains("does not advertise")
    );
    assert!(f.requests.lock().unwrap().is_empty());

    let levels = model["effortLevels"].as_array().unwrap();
    for (value, input) in levels
        .iter()
        .zip(["QWEN_EFFORT_FIRST", "QWEN_EFFORT_SECOND"])
    {
        let level = value.as_str().unwrap();
        options.settings.effort = Some(level.into());
        let session = rt.open(options.clone()).await.unwrap();
        if let Some(id) = &options.resume {
            assert_eq!(session.id(), id);
        }
        assert_eq!(session.effort().as_deref(), Some(level));
        assert_eq!(session.info()["effort_status"]["effort"], level);
        assert_eq!(session.info()["effort_status"]["applied"], true);
        assert_eq!(session.info()["effort_status"]["override"], Value::Null);
        assert!(
            session
                .records()
                .iter()
                .any(|r| r.events.contains(&Event::Effort {
                    effort: level.to_owned()
                }))
        );
        assert_eq!(
            session.prompt(input).await.unwrap().wait().await.unwrap(),
            Outcome::Completed
        );
        options.resume = Some(session.id().to_owned());
        session.close().await.unwrap();
    }
    let requests_after_turns = {
        let requests = f.requests.lock().unwrap();
        for ((marker, level), ordinal) in ["QWEN_EFFORT_FIRST", "QWEN_EFFORT_SECOND"]
            .iter()
            .zip(levels)
            .zip(0..)
        {
            let mut matching = requests.iter().filter(|req| {
                let Some(msg) = req["messages"].as_array().and_then(|m| m.last()) else {
                    return false;
                };
                let body = &msg["content"];
                msg["role"] == "user"
                    && (body.as_str() == Some(*marker)
                        || body.as_array().is_some_and(|parts| {
                            parts
                                .iter()
                                .any(|part| part["type"] == "text" && part["text"] == *marker)
                        }))
            });
            let req = matching
                .next()
                .expect("a model request for the explicit user input");
            assert_eq!(req["model"], model["id"]);
            assert_eq!(&req["reasoning_effort"], level);
            assert!(
                matching.next().is_none(),
                "duplicate model request for {marker}"
            );
            if ordinal == 1 {
                assert!(req["messages"].to_string().contains("QWEN_EFFORT_FIRST"));
            }
        }
        requests.len()
    };

    configure(&f, "kar-text-only", None);
    let mut plain = f.opts.clone();
    plain.model = None;
    plain
        .env
        .insert("OPENAI_MODEL".into(), "kar-text-only".into());
    let listing = rt.catalog(Catalog::Models, plain.clone()).await.unwrap();
    assert_eq!(listing.as_array().unwrap().len(), 1);
    assert_eq!(listing[0]["id"], "kar-text-only");
    assert!(listing[0].get("effortLevels").is_none());
    for selected in [Some("kar-text-only".to_owned()), None] {
        plain.model = selected;
        assert!(
            refusal(&rt, &plain, "low")
                .await
                .contains("does not advertise")
        );
    }
    assert_eq!(f.requests.lock().unwrap().len(), requests_after_turns);
    f.http.abort();
}
