use super::*;
use serde_json::json;

fn values(v: Value) -> FormValues {
    v.as_object().unwrap().clone()
}
fn listed() -> ModelListing {
    ModelListing::from_result(
        Ok(serde_json::from_value(json!([
            {"id":"standard","default":true,"effortLevels":["high"]},
            {"id":"small","effortLevels":["low"]},
            {"id":"old","disabled":{"reason":"retired"}}
        ]))
        .unwrap()),
        vec![],
    )
}

#[test]
fn grok_form_preserves_stored_models_and_effort_without_a_live_catalog() {
    let mut form = config_form("grok").unwrap();
    let wire = serde_json::to_value(&form).unwrap();
    assert_eq!(wire["protocol"], 1);
    assert_eq!(
        wire["fields"][0]["modes"],
        json!(["inherit", "preset", "custom"])
    );
    assert_eq!(wire["fields"][1]["modelField"], "model");
    assert_eq!(
        serde_json::from_value::<RuntimeConfigForm>(wire).unwrap(),
        form
    );
    assert!(config_form("codex").is_none());
    let live = listed();
    let create = |f: &RuntimeConfigForm, v: &Value| {
        f.validate(
            &values(v.clone()),
            ValidationContext::Create { models: &live },
        )
    };
    for v in [
        json!({"reasoningEffort":"high"}),
        json!({"model":{"kind":"inherit"},"reasoningEffort":"high"}),
        json!({"model":{"kind":"custom","name":"small"},"reasoningEffort":"low"}),
    ] {
        assert!(create(&form, &v).is_empty());
    }
    let stored = values(json!({"model":{"kind":"preset","id":"old"},"reasoningEffort":"low"}));
    let unavailable = ModelListing::from_result(Err(ListingReason::MachineOffline), vec![]);
    assert_eq!(
        form.validate(&stored, ValidationContext::Create { models: &live })[0].code,
        IssueCode::ModelDisabled
    );
    assert!(
        form.validate(
            &stored,
            ValidationContext::Edit {
                models: &unavailable,
                stored: &stored
            }
        )
        .is_empty()
    );
    let custom = values(json!({"model":{"kind":"custom","name":"small"},"reasoningEffort":"low"}));
    form.fields[0].kind = ConfigFieldKind::Model {
        modes: vec![ModelMode::Preset],
        launch_modes: Some(vec![ModelMode::Preset, ModelMode::Custom]),
    };
    let issues = form.validate(
        &custom,
        ValidationContext::Edit {
            models: &unavailable,
            stored: &stored,
        },
    );
    assert_eq!(
        issues.iter().map(|i| i.code).collect::<Vec<_>>(),
        [IssueCode::ModeNotAllowed, IssueCode::EffortNotOffered]
    );
    assert!(
        form.validate(
            &custom,
            ValidationContext::Edit {
                models: &unavailable,
                stored: &custom
            }
        )
        .is_empty()
    );
    let launch = form.map_launch(&custom).unwrap();
    assert_eq!(launch.model.as_deref(), Some("small"));
    assert_eq!(launch.effort.as_deref(), Some("low"));
    assert_eq!(
        form.map_launch(&values(json!({"model":{"kind":"inherit"}})))
            .err()
            .unwrap()[0]
            .code,
        IssueCode::ModeNotAllowed
    );
    let fallback = ModelListing::from_result(Ok(vec![]), live.models().to_owned());
    assert!(matches!(
        fallback,
        ModelListing::Fallback {
            reason: ListingReason::Empty,
            ..
        }
    ));
}

#[test]
fn conditional_fields_and_write_only_secrets_are_checked_at_each_boundary() {
    let make = |id: &str, kind: Value, parent: Option<&str>| {
        let mut f = json!({"id":id,"label":{"key":id,"en":id},"required":true});
        f.as_object_mut()
            .unwrap()
            .extend(kind.as_object().unwrap().clone());
        if let Some(parent) = parent {
            f["visibleWhen"] = json!({"field":parent,"in":["on"]});
        }
        f
    };
    let form:RuntimeConfigForm = serde_json::from_value(json!({"protocol":1,"runtime":"fixture","fields":[
        make("provider",json!({"kind":"select","options":[{"value":"on","label":{"key":"on","en":"On"}},{"value":"off","label":{"key":"off","en":"Off"}}]}),None),
        make("mode",json!({"kind":"text"}),Some("provider")),
        make("uri",json!({"kind":"text","format":"url"}),Some("mode")),
        make("token",json!({"kind":"secret"}),Some("mode")),
        make("flag",json!({"kind":"boolean"}),Some("mode"))
    ]})).unwrap();
    let listing = listed();
    let branch =
        values(json!({"provider":"off","mode":"on","uri":"wrong","token":{"kind":"clear"}}));
    assert!(
        form.validate(&branch, ValidationContext::Create { models: &listing })
            .is_empty()
    );
    assert_eq!(form.active_fields(&FormValues::new()).count(), 1);
    let mapped = form.map_launch(&branch).unwrap();
    assert_eq!(mapped.unmapped, values(json!({"provider":"off"})));
    assert!(mapped.secrets.is_empty());
    let mut active = values(
        json!({"provider":"on","mode":"on","uri":"invalid","token":{"kind":"keep"},"flag":false,"unknown":true}),
    );
    let bad = form.validate(&active, ValidationContext::Create { models: &listing });
    assert_eq!(
        bad.iter().map(|i| i.code).collect::<Vec<_>>(),
        [
            IssueCode::UnknownField,
            IssueCode::InvalidShape,
            IssueCode::Required
        ]
    );
    active.remove("unknown");
    active.insert("uri".into(), json!("https://example.org"));
    assert!(
        form.validate(
            &active,
            ValidationContext::Edit {
                models: &listing,
                stored: &active
            }
        )
        .is_empty()
    );
    assert_eq!(
        form.map_launch(&active).err().unwrap()[0].code,
        IssueCode::InvalidShape
    );
    active.insert("token".into(), json!({"present":false}));
    assert_eq!(
        form.map_launch(&active).err().unwrap()[0].code,
        IssueCode::SecretMissing
    );
    active.insert(
        "token".into(),
        json!({"present":true,"value":"fixture-only"}),
    );
    let mapped = form.map_launch(&active).unwrap();
    assert_eq!(mapped.secrets.get("token").unwrap(), "fixture-only");
    assert!(!mapped.unmapped.contains_key("token"));
    assert_eq!(mapped.unmapped["flag"], false);
}
