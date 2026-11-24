use kar::{
    ApprovalMode, Error, FailureClass, OpenError, Protocol, ResumeCwdReason, Runtime,
    RuntimeFailureCode, SessionOptions, UnavailableReason,
};
use serde_json::{Value, json};
use std::io::BufRead;

pub(super) fn peer(script: &str) {
    let script: Vec<Value> = serde_json::from_str(script).unwrap();
    let mut lines = std::io::stdin().lock().lines();
    let mut saved_page = None;
    for step in script {
        if let Some(wanted) = step["geminiDir"].as_str() {
            let args: Vec<_> = std::env::args().collect();
            assert!(
                args.windows(2)
                    .any(|args| args[0] == "--gemini-dir" && args[1] == wanted)
            );
        }
        let method = step["method"].as_str().unwrap();
        let packet: Value = loop {
            let Some(Ok(line)) = lines.next() else { return };
            let packet: Value = serde_json::from_str(&line).unwrap();
            if packet["method"] == "initialized" {
                continue;
            }
            break packet;
        };
        assert_eq!(packet["method"], method);
        if let Some(params) = step.get("params") {
            assert_eq!(&packet["params"], params)
        }
        if step["repeatPage"] == true {
            super::emit(
                saved_page
                    .take()
                    .expect("expected a previous list response"),
            );
        }
        for notice in step["beforeReply"].as_array().into_iter().flatten() {
            super::emit(notice.clone());
        }
        if let Some(error) = step.get("error") {
            super::emit(json!({"jsonrpc":"2.0","id":packet["id"],"error":error}));
        } else {
            let mut reply = json!({"jsonrpc":"2.0","id":packet["id"],"result":step["result"]});
            if packet["factoryApiVersion"].is_string() {
                reply["type"] = "response".into();
                reply["factoryApiVersion"] = packet["factoryApiVersion"].clone();
                reply["jsonrpc"] = "2.0".into();
            }
            if method == "session/list" && saved_page.is_none() {
                saved_page = Some(reply.clone());
            }
            super::emit(reply);
        }
        if let Some(notices) = step["afterReply"].as_array() {
            std::thread::sleep(std::time::Duration::from_millis(15));
            for notice in notices {
                super::emit(notice.clone());
            }
        }
    }
    // A successful open is followed only by shutdown. In particular a failed
    // cwd check or missing native id must never send a new or load request.
    for line in lines.map_while(Result::ok) {
        let p: Value = serde_json::from_str(&line).unwrap();
        assert!(!matches!(
            p["method"].as_str(),
            Some("session/new" | "session/fork" | "session/load" | "session/resume")
        ));
    }
}

fn init(caps: Value) -> Value {
    json!({"method":"initialize","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":caps}}})
}
fn list(page: Value) -> Value {
    json!({"method":"session/list","result":page})
}
fn error(method: &str, code: i64, message: &str, data: Value) -> Value {
    json!({"method":method,"error":{"code":code,"message":message,"data":data}})
}

async fn open(
    rt: &Runtime,
    opts: &SessionOptions,
    script: Vec<Value>,
) -> kar::Result<kar::Session> {
    let mut opts = opts.clone();
    opts.env.insert(
        "KAR_TEST_OPEN_SCRIPT".into(),
        serde_json::to_string(&script)?.into(),
    );
    rt.open(opts).await
}
async fn typed(rt: &Runtime, opts: &SessionOptions, script: Vec<Value>) -> OpenError {
    match open(rt, opts, script).await {
        Err(Error::Open(e)) => *e,
        Err(e) => panic!("expected typed open refusal, got {e:?}"),
        Ok(_) => panic!("unexpected successful open"),
    }
}

pub(super) async fn run() -> kar::Result<()> {
    factory_pending().await?;
    grok_forms().await?;
    cursor_models().await?;
    model_confirmations().await?;
    let (_dir, mut acp, mut opts) = super::rig(Protocol::Acp, ApprovalMode::Deny);
    let cwd = opts.cwd.canonicalize()?;
    opts.resume = Some("target".into());
    acp.id = "devin".into();
    let enabled = init(json!({"list":{},"resume":{}}));
    let found = list(json!({"sessions":[{"sessionId":"target","cwd":cwd}]}));
    for (hello, pages, reason) in [
        (init(json!({})), vec![], ResumeCwdReason::ListingUnavailable),
        (
            enabled.clone(),
            vec![list(json!({"sessions":[]}))],
            ResumeCwdReason::NotListed,
        ),
        (
            enabled.clone(),
            vec![list(json!({"sessions":[{"sessionId":"target"}]}))],
            ResumeCwdReason::CwdUnreported,
        ),
        (
            enabled.clone(),
            vec![list(
                json!({"sessions":[{"sessionId":"target","cwd":"/some/other/project"}]}),
            )],
            ResumeCwdReason::OtherCwd {
                saved_cwd: "/some/other/project".into(),
            },
        ),
        (
            enabled.clone(),
            vec![list(
                json!({"sessions":[{},{"sessionId":"target","cwd":cwd}]}),
            )],
            ResumeCwdReason::ListingUnavailable,
        ),
        (
            enabled.clone(),
            vec![list(json!({"sessions":[],"nextCursor":12}))],
            ResumeCwdReason::ListingUnavailable,
        ),
        (
            enabled.clone(),
            vec![list(json!({"sessions":[],"nextCursor":"again"})); 2],
            ResumeCwdReason::ListingUnavailable,
        ),
    ] {
        let mut script = vec![hello];
        script.extend(pages);
        let e = typed(&acp, &opts, script).await;
        assert!(
            matches!(e,OpenError::ResumeCwd {session_id,requested_cwd,reason:r} if session_id=="target" && std::path::Path::new(&requested_cwd)==cwd && r==reason)
        );
    }
    let s = open(&acp,&opts,vec![enabled.clone(),list(json!({"sessions":[{"sessionId":"private","title":"other-workspace-title"}],"nextCursor":"private-cursor"})),
        json!({"method":"session/list","params":{"cursor":"private-cursor"},"result":found["result"]}),json!({"method":"session/resume","repeatPage":true,"result":{}})]).await?;
    assert_eq!(s.id(), "target");
    assert!(
        s.records()
            .iter()
            .all(|r| !r.native.to_string().contains("other-workspace-title")
                && !r.native.to_string().contains("private-cursor"))
    );
    s.close().await?;
    for result in [
        Value::Null,
        json!({"sessionId":null}),
        json!({"sessionId":"wrong"}),
    ] {
        let result = open(
            &acp,
            &opts,
            vec![
                enabled.clone(),
                found.clone(),
                json!({"method":"session/resume","result":result}),
            ],
        )
        .await;
        assert!(matches!(result, Err(Error::Protocol(_))));
    }
    let secret = "secret-from-open-test";
    opts.env.insert("TEST_API_KEY".into(), secret.into());
    let native = error(
        "session/resume",
        -32016,
        "Missing secret-from-open-test",
        json!({"cognition.ai/errorKind":"session_not_found","token":"hidden-field"}),
    );
    let e = typed(&acp, &opts, vec![enabled.clone(), found.clone(), native]).await;
    assert!(
        matches!(e,OpenError::SessionNotFound {ref cause,..} if cause.method=="session/resume")
    );
    let serialized = serde_json::to_string(&e)?;
    assert!(!serialized.contains(secret) && !serialized.contains("hidden-field"));
    let e = typed(
        &acp,
        &opts,
        vec![
            enabled,
            error("session/list", -32000, "sign in", Value::Null),
        ],
    )
    .await;
    assert!(
        matches!(e,OpenError::RuntimeFailure {failure,..} if failure.class==FailureClass::Auth)
    );
    let e = typed(
        &acp,
        &opts,
        vec![error("initialize", -32603, "Invalid API key", Value::Null)],
    )
    .await;
    assert!(
        matches!(e,OpenError::RuntimeFailure {failure,..} if failure.class==FailureClass::Auth)
    );
    acp.id = "cursor".into();
    let missing = |detail: &str, code: i64| {
        vec![
            init(json!({})),
            error(
                "session/load",
                code,
                "Invalid params",
                json!({"message":detail}),
            ),
        ]
    };
    let e = typed(&acp, &opts, missing("Session \"target\" not found", -32602)).await;
    assert!(
        matches!(e,OpenError::SessionNotFound {session_id,message,cause}
        if session_id=="target" && message=="Session \"target\" not found" && cause.method=="session/load")
    );
    for script in [
        missing("Session \"other\" not found", -32602),
        missing("Session \"target\" not found", -32603),
    ] {
        assert!(matches!(
            open(&acp, &opts, script).await,
            Err(Error::Refused(_))
        ));
    }
    acp.id = "grok".into();
    let e = typed(
        &acp,
        &opts,
        vec![
            init(json!({})),
            error(
                "session/load",
                -32603,
                "Path not found.",
                json!({"code":"FS_NOT_FOUND"}),
            ),
        ],
    )
    .await;
    assert!(matches!(e,OpenError::SessionNotFound {ref session_id,..} if session_id=="target"));
    let e = open(
        &acp,
        &opts,
        vec![
            init(json!({})),
            error("session/load", -32603, "Path not found.", Value::Null),
        ],
    )
    .await;
    assert!(matches!(e, Err(Error::Refused(_))));
    opts.resume = None;
    opts.model = Some("unknown/model".into());
    for (info, method) in [
        (json!({"sessionId":"s"}), "session/set_model"),
        (
            json!({"sessionId":"s","configOptions":[{"id":"route","category":"model"}]}),
            "session/set_config_option",
        ),
    ] {
        let e = typed(
            &acp,
            &opts,
            vec![
                init(json!({})),
                json!({"method":"session/new","result":info}),
                error(method, -32602, "invalid selection", Value::Null),
            ],
        )
        .await;
        assert!(
            matches!(e,OpenError::RuntimeFailure {failure,..} if failure.class==FailureClass::ModelUnavailable)
        );
    }

    let (_dir, codex, mut opts) = super::rig(Protocol::Codex, ApprovalMode::Deny);
    opts.resume = Some("t".into());
    for (message, reason) in [
        (
            "session t is archived. restore it first",
            UnavailableReason::Archived,
        ),
        (
            "thread t already has an active writer",
            UnavailableReason::InUse,
        ),
    ] {
        let e = typed(
            &codex,
            &opts,
            vec![
                json!({"method":"initialize","result":{}}),
                error("thread/resume", -32600, message, Value::Null),
            ],
        )
        .await;
        assert!(
            matches!(e,OpenError::SessionUnavailable {reason:r, session_id,..} if r==reason && session_id=="t")
        );
    }
    let e = open(
        &codex,
        &opts,
        vec![
            json!({"method":"initialize","result":{}}),
            error(
                "thread/resume",
                -32600,
                "sub-agent unavailable",
                Value::Null,
            ),
        ],
    )
    .await;
    assert!(matches!(e, Err(Error::Refused(_))));
    opts.settings.service_tier = Some("priority".into());
    for (message, expected) in [
        (
            "failed to load configuration: bad path",
            RuntimeFailureCode::RuntimeConfigInvalid,
        ),
        (
            "Invalid request: unknown variant `priority`, expected `fast` or `flex`",
            RuntimeFailureCode::RuntimeOutdated,
        ),
    ] {
        let e = typed(
            &codex,
            &opts,
            vec![
                json!({"method":"initialize","result":{}}),
                error("thread/resume", -32600, message, Value::Null),
            ],
        )
        .await;
        assert!(
            matches!(e,OpenError::RuntimeFailure {code:Some(actual),failure,..} if actual==expected && failure.class==FailureClass::InvalidRequest)
        );
    }
    println!("Native open errors, paginated workspace checks and redacted evidence passed");
    Ok(())
}

async fn cursor_models() -> kar::Result<()> {
    use kar::forms::{ListingReason, ModelListing};
    let (_dir, mut rt, mut opts) = super::rig(Protocol::Acp, ApprovalMode::Deny);
    rt.id = "cursor".into();
    let hello = init(json!({}));
    for (code, expected) in [
        (-32601, ListingReason::Unsupported),
        (-32000, ListingReason::Unauthenticated),
        (-32603, ListingReason::Failed),
    ] {
        opts.env.insert(
            "KAR_TEST_OPEN_SCRIPT".into(),
            serde_json::to_string(&[
                hello.clone(),
                error(
                    "cursor/list_available_models",
                    code,
                    "native refusal",
                    Value::Null,
                ),
            ])?
            .into(),
        );
        assert!(
            matches!(rt.form_model_listing(&opts,vec![]).await,ModelListing::Unavailable{reason} if reason==expected)
        );
    }
    opts.settings.effort = Some("high".into());
    let state = |model: &str, level: &str| {
        json!({"configOptions":[
        {"id":"model","type":"select","category":"model","currentValue":model},
        {"id":"thinking","type":"select","category":"thought_level","currentValue":"true"},
        {"id":"reasoning_effort","type":"select","category":"thought_level","currentValue":level}]})
    };
    for (model, level) in [("base", "low"), ("substituted", "high")] {
        let mut before = state("base", "low");
        before["sessionId"] = "s".into();
        let result = open(&rt,&opts,vec![hello.clone(),json!({"method":"session/new","result":before}),
            json!({"method":"session/set_config_option","params":{"sessionId":"s","configId":"reasoning_effort","value":"high"},"result":state(model,level)})]).await;
        assert!(matches!(result, Err(Error::Refused(_))));
    }
    Ok(())
}

async fn grok_forms() -> kar::Result<()> {
    use kar::forms::{ListingReason, ModelListing, ValidationContext};
    let (_dir, mut rt, mut opts) = super::rig(Protocol::Acp, ApprovalMode::Deny);
    rt.id = "grok".into();
    let hello = json!({"method":"initialize","result":{"protocolVersion":1,
        "authMethods":[{"id":"cached_token"},{"id":"preferred"}],"_meta":{"defaultAuthMethodId":"preferred"}}});
    let auth = json!({"method":"authenticate","params":{"methodId":"preferred"},"result":{}});
    let query = "_x.ai/models/list";
    let state = json!({"current_model_id":"visible","available_models":[
        {"model_id":" visible ","display_name":" Visible ","_meta":{"reasoningEfforts":[{"id":"high"},"low"],"reasoningEffort":"high"}},
        {"id":"hidden","hidden":true},{"modelId":"private","user_selectable":false},
        {"id":"plain","reasoning_efforts":["low"],"_meta":{"supportsReasoningEffort":false}}
    ]});
    let script = |opts: &mut SessionOptions, steps: Vec<Value>| {
        opts.env.insert(
            "KAR_TEST_OPEN_SCRIPT".into(),
            serde_json::to_string(&steps).unwrap().into(),
        );
    };
    script(
        &mut opts,
        vec![
            hello.clone(),
            auth.clone(),
            json!({"method":query,"result":{"result":state}}),
        ],
    );
    let listing = rt.form_model_listing(&opts, vec![]).await;
    let ModelListing::Live { models } = &listing else {
        panic!("unexpected listing {listing:?}");
    };
    assert_eq!(
        serde_json::to_value(models)?,
        json!([
        {"id":"visible","displayName":"Visible","default":true,"effortLevels":["high","low"],"defaultEffort":"high"},{"id":"plain"}])
    );
    let form = kar::forms::config_form("grok").unwrap();
    let selected = json!({"model":{"kind":"preset","id":"visible"},"reasoningEffort":"low"});
    let values = selected.as_object().unwrap();
    assert!(
        form.validate(values, ValidationContext::Create { models: &listing })
            .is_empty()
    );
    let mut launch = opts.clone();
    form.map_launch(values).unwrap().apply_to(&mut launch)?;
    let option = |effort: &str| json!({"id":"reasoning_effort","category":"thought_level","currentValue":effort,"options":[{"value":"high"},{"value":"low"}]});
    let steps = vec![
        hello.clone(),
        auth.clone(),
        json!({"method":"session/new","result":{"sessionId":"s","configOptions":[option("high")]}}),
        json!({"method":"session/set_model","params":{"sessionId":"s","modelId":"visible"},"result":{}}),
        json!({"method":"session/set_config_option","params":{"sessionId":"s","configId":"reasoning_effort","value":"low"},"result":{"configOptions":[option("low")]}}),
    ];
    open(&rt, &launch, steps).await?.close().await?;
    for (reply, reason) in [
        (
            json!({"error":{"code":-32601,"message":"missing extension"}}),
            ListingReason::Unsupported,
        ),
        (
            json!({"error":{"code":-32000,"message":"login required"}}),
            ListingReason::Unauthenticated,
        ),
        (
            json!({"result":{"error":{"message":"bad model config"},"result":state}}),
            ListingReason::Failed,
        ),
        (
            json!({"result":{"availableModels":[]}}),
            ListingReason::Empty,
        ),
    ] {
        let mut step = reply;
        step["method"] = query.into();
        script(&mut opts, vec![hello.clone(), auth.clone(), step]);
        let fallback = rt.form_model_listing(&opts, models.clone()).await;
        assert!(
            matches!(fallback,ModelListing::Fallback{reason:r,..} if r==reason),
            "{fallback:?}"
        );
    }
    Ok(())
}

async fn factory_pending() -> kar::Result<()> {
    let (_dir, rt, opts) = super::rig(Protocol::Factory, ApprovalMode::Host);
    let request = |id: &str, method: &str, params: Value| json!({"jsonrpc":"2.0","type":"request","id":id,"method":method,"params":params});
    let notice = |session: &str, notification: Value| {
        json!({"jsonrpc":"2.0","type":"notification","method":"droid.session_notification",
               "params":{"sessionId":session,"notification":notification}})
    };
    let withdraw = json!({"type":"ask_user_resolved","requestIds":["a","b","a","p","missing"]});
    let permitted =
        json!({"type":"permission_resolved","requestId":"p","selectedOption":"proceed_once"});
    let pick = json!({"title":"Tasks","rows":[{"id":"id-1","label":"First","detail":"Details"}],
                      "multi":true,"preselectedIds":["id-1"]});
    let frames = vec![
        request("a", "droid.ask_user", json!({})),
        request("b", "droid.ask_user", json!({})),
        request("p", "droid.request_permission", json!({})),
        notice("s", withdraw.clone()),
        request("a", "droid.pick_options", pick),
        notice("s", withdraw),
        notice("s", permitted.clone()),
        notice("s", permitted),
        request(
            "done",
            "droid.request_permission",
            json!({"options":[
            {"value":"cancel","label":"Cancel"},{"value":"proceed_auto_run_high","label":"Auto-run high"},
            {"value":"proceed_edit","label":"Edit the spec"}]}),
        ),
    ];
    let step =
        json!({"method":"droid.initialize_session","result":{"sessionId":"s"},"afterReply":frames});
    let session = open(&rt, &opts, vec![step]).await?;
    let mut records = session.subscribe(None)?;
    let mut cleared = Vec::new();
    while let Some(record) = records.next().await {
        for event in &record.events {
            if let kar::Event::Exited { reason } = event {
                panic!("{reason}");
            }
            if let kar::Event::RequestResolved { id } = event {
                assert_eq!(record.native["params"]["sessionId"], "s");
                cleared.push(id.clone());
            }
        }
        if record.native["id"] == "done" {
            break;
        }
    }
    assert_eq!(cleared, [json!("a"), json!("b"), json!("p")]);
    let pending = session.snapshot().pending_requests;
    assert_eq!(pending.len(), 2);
    let picker = &pending[0];
    assert_eq!(picker.method, "droid.pick_options");
    let form = picker.question_form().unwrap();
    assert_eq!(form.fields[0].default, Some(json!(["id-1"])));
    assert_eq!(
        form.fields[0].options[0].description.as_deref(),
        Some("Details")
    );
    let pick = |ids: Value| kar::QuestionReply::Submit {
        values: json!({"selectedIds":ids}).as_object().unwrap().clone(),
    };
    assert!(
        session
            .answer_question(picker, pick(json!(["First"])))
            .await
            .is_err()
    );
    session
        .answer_question(picker, pick(json!(["id-1"])))
        .await?;
    assert!(
        session
            .answer_question(picker, kar::QuestionReply::Cancel)
            .await
            .is_err()
    );
    let options = pending[1].policy_options();
    assert_eq!(options.len(), 2);
    session
        .select_policy(&pending[1], options[1].choice)
        .await?;
    let mut answers = Vec::new();
    while answers.len() < 2 {
        let r = records.next().await.unwrap();
        if r.direction == kar::Direction::ToRuntime && r.native["type"] == "response" {
            answers.push(r.native["result"].clone());
        }
    }
    assert_eq!(
        answers,
        [
            json!({"cancelled":false,"selectedIds":["id-1"]}),
            json!({"selectedOption":"proceed_auto_run_high"})
        ]
    );
    assert!(session.snapshot().pending_requests.is_empty());
    session.close().await
}

async fn model_confirmations() -> kar::Result<()> {
    let (_dir, codex, mut opts) = super::rig(Protocol::Codex, ApprovalMode::Deny);
    opts.model = Some("wanted".into());
    for (method, resume) in [("thread/start", None), ("thread/resume", Some("s".into()))] {
        opts.resume = resume;
        let e = typed(
            &codex,
            &opts,
            vec![
                json!({"method":"initialize","result":{}}),
                json!({"method":method,"result":{"thread":{"id":"s"},"model":"actual"}}),
            ],
        )
        .await;
        assert!(
            matches!(e, OpenError::ModelConfirmation { requested, reported:Some(reported), catalog_selector_ids:None, cause:None, .. } if requested=="wanted" && reported=="actual")
        );
        let opened = open(
            &codex,
            &opts,
            vec![
                json!({"method":"initialize","result":{}}),
                json!({"method":method,"result":{"thread":{"id":"s"}}}),
            ],
        )
        .await?;
        opened.close().await?;
    }
    let (_dir, pi, mut opts) = super::rig(Protocol::Pi, ApprovalMode::Deny);
    opts.model = Some("one/two".into());
    assert!(
        matches!(pi.open(opts).await,Err(Error::Open(e)) if matches!(*e,OpenError::ModelConfirmation{reported:None,catalog_selector_ids:None,..}))
    );

    let (_dir, mut rt, mut opts) = super::rig(Protocol::Acp, ApprovalMode::Deny);
    rt.id = "antigravity".into();
    let profile = opts.cwd.join("separate-gemini-profile");
    opts.env
        .insert("GEMINI_HOME".into(), profile.clone().into_os_string());
    let mut hello = init(json!({}));
    hello["geminiDir"] = json!(profile);
    opts.handshake_timeout = std::time::Duration::from_millis(600);
    opts.model = Some("alpha-high".into());
    let menu = |model: &str, effort: &str, levels: Vec<&str>| {
        json!({"sessionId":"s","configOptions":[
            {"id":"model","category":"model","currentValue":model,"options":[{"value":"alpha"},{"value":"beta"}]},
            {"id":"thinking","category":"thought_level","currentValue":effort,"options":levels.iter().map(|v| json!({"value":v})).collect::<Vec<_>>()}
        ]})
    };
    let default = menu("beta", "low", vec!["low"]);
    let selected = menu("alpha", "low", vec!["low", "high"]);
    let final_state = menu("alpha", "high", vec!["low", "high"]);
    let notice = |id: &str, info: &Value| json!({"method":"session/update","params":{"sessionId":id,"update":{"sessionUpdate":"config_option_update","configOptions":info["configOptions"]}}});
    let new = json!({"method":"session/new","result":default});
    let model = json!({"method":"session/set_config_option","params":{"sessionId":"s","configId":"model","value":"alpha"},
        "result":selected, "beforeReply":[notice("s",&selected),notice("s",&default),notice("foreign",&selected)]});
    let mut eventual = model.clone();
    eventual["afterReply"] = json!([notice("s", &selected)]);
    let applied = open(&rt,&opts,vec![hello,new.clone(),eventual,
        json!({"method":"session/set_config_option","params":{"sessionId":"s","configId":"thinking","value":"high"},"result":final_state,"beforeReply":[notice("s",&final_state)]})]).await?;
    assert_eq!(
        applied.info()["configOptions"],
        final_state["configOptions"]
    );
    applied.close().await?;
    let e = typed(
        &rt,
        &opts,
        vec![init(json!({})), new.clone(), model.clone()],
    )
    .await;
    assert!(
        matches!(e, OpenError::ModelConfirmation{requested,reported:Some(r),catalog_selector_ids:Some(ids),..} if requested=="alpha-high" && r=="beta" && ids==["alpha","beta"])
    );
    opts.model = Some("alpha".into());
    let mut silent = model.clone();
    silent["beforeReply"] = json!([notice("foreign", &selected)]);
    assert!(matches!(
        open(&rt, &opts, vec![init(json!({})), new.clone(), silent]).await,
        Err(Error::Timeout(_))
    ));
    let mut refused = model;
    refused["error"] = json!({"code":-32602,"message":"invalid selection"});
    assert!(
        matches!(typed(&rt,&opts,vec![init(json!({})),new,refused]).await, OpenError::RuntimeFailure{failure,..} if failure.class==FailureClass::ModelUnavailable)
    );
    Ok(())
}
