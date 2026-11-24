use super::rig;
use kar::{ApprovalMode, FailureClass as Class, Outcome, Protocol, Session, TurnFailure};
use serde_json::{Value, json};

async fn run_turn(session: &Session, script: Value) -> kar::Result<Option<TurnFailure>> {
    let turn = session.prompt(format!("failure-fixture {script}")).await?;
    if script.get("error").is_some() {
        assert!(turn.report().await.is_err());
    } else {
        let report = turn.report().await?;
        assert_eq!(report.failure, session.snapshot().last_failure);
        assert_eq!(report.outcome == Outcome::Failed, report.failure.is_some());
    }
    Ok(session.snapshot().last_failure)
}

pub(super) async fn run() -> kar::Result<()> {
    let (_dir, mut r, opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    let info =
        json!({"code":-32603,"message":"Overloaded","data":{"errorName":"provider.free-limit"}});
    r.id = "opencode".into();
    let s = r.open(opts.clone()).await?;
    let f = run_turn(&s, json!({"error":info})).await?.unwrap();
    assert_eq!(
        (f.class, f.reason.as_str(), f.status),
        (Class::Quota, "Overloaded", None)
    );
    s.close().await?;

    r.id = "grok".into();
    let s = r.open(opts.clone()).await?;
    // This peer omits the image flag, as some real Grok ACP versions do.
    assert!(s.capabilities().image_input);
    let state = |session: &str, class: &str| {
        json!({"method":"_x.ai/session_notification",
        "params":{"sessionId":session,"update":{"sessionUpdate":"retry_state","type":"exhausted","error_type":class}}})
    };
    let err = json!({"code":-32603,"message":"Internal error","data":{"http_status":502}});
    let f = run_turn(
        &s,
        json!({"frames":[state(s.id(),"context_length"),state("child","auth")],"error":err}),
    )
    .await?
    .unwrap();
    assert_eq!((f.class, f.status), (Class::InputTooLarge, Some(502)));
    assert_eq!(
        run_turn(&s, json!({"error":err})).await?.unwrap().class,
        Class::Provider
    );
    for (detail, class) in [
        (
            "API error (status 429): insufficient_quota: exhausted",
            Class::Quota,
        ),
        (
            "API error (status 429): credit_balance_exhausted: zero credits",
            Class::Billing,
        ),
        ("quota exceeded", Class::RateLimited),
    ] {
        let f = run_turn(
            &s,
            json!({"error":{"code":-32003,"message":"Rate limited","data":detail}}),
        )
        .await?
        .unwrap();
        assert_eq!(
            (f.class, f.status, f.reason),
            (class, Some(429), format!("Rate limited: {detail}"))
        );
    }
    assert_eq!(
        run_turn(&s, json!({"error":info})).await?.unwrap().class,
        Class::Overloaded
    );
    s.close().await?;

    r.id = "codebuddy".into();
    let s = r.open(opts.clone()).await?;
    let native = json!({"error":{"message":"account allowance","data":{"category":"quota","statusCode":503}}});
    let mut reply = json!({"stopReason":"end_turn","_meta":{"codebuddy.ai/outcome":"FAILED_MODEL_REQUEST","codebuddy.ai/errorMessage":native.to_string()}});
    let f = run_turn(&s, json!({"result":reply})).await?.unwrap();
    assert_eq!(
        (f.class, f.status, f.reason.as_str()),
        (Class::Quota, Some(503), "account allowance")
    );
    reply["stopReason"] = "cancelled".into();
    assert!(run_turn(&s, json!({"result":reply})).await?.is_none());
    for (meta, class) in [
        (json!({}), Class::ModelRefused),
        (
            json!({"codebuddy.ai/errorMessage":"local command failed"}),
            Class::Unknown,
        ),
    ] {
        let f = run_turn(&s, json!({"result":{"stopReason":"refusal","_meta":meta}}))
            .await?
            .unwrap();
        assert_eq!(f.class, class);
    }
    s.close().await?;

    r.id = "antigravity".into();
    let s = r.open(opts).await?;
    let update = |body: Value| json!({"method":"session/update","params":{"sessionId":s.id(),"update":body}});
    let chunk = |text: &str| {
        update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}))
    };
    let notice = "\n⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h2m3s.\n";
    let (first, last) = notice.split_once("Please").unwrap();
    let message = [chunk(first), chunk(&format!("Please{last}"))];
    let result = json!({"stopReason":"end_turn"});
    let f = run_turn(&s, json!({"frames":message,"result":result}))
        .await?
        .unwrap();
    assert_eq!((f.class, f.reason.as_str()), (Class::Quota, notice));
    let time = s
        .records()
        .iter()
        .rev()
        .find(|r| {
            r.native
                .pointer("/params/update/sessionUpdate")
                .is_some_and(|v| v == "agent_message_chunk")
        })
        .unwrap()
        .timestamp_ms;
    assert_eq!(f.resets_at_ms, Some(time + 3_723_000));
    let tool =
        update(json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed"}));
    let usage = update(json!({"sessionUpdate":"usage_update","used":80,"size":1000}));
    for (frames, result, expected) in [
        (json!([chunk(notice), usage]), result.clone(), None),
        (
            json!([chunk(notice)]),
            json!({"stopReason":"end_turn","usage":{}}),
            None,
        ),
        (
            json!([tool, chunk(notice)]),
            json!({"stopReason":"end_turn","usage":{}}),
            Some(Class::Quota),
        ),
        (
            json!([chunk(&format!("the model quoted:{notice}"))]),
            result,
            None,
        ),
    ] {
        assert_eq!(
            run_turn(&s, json!({"frames":frames,"result":result}))
                .await?
                .map(|f| f.class),
            expected
        );
    }
    let f = run_turn(
        &s,
        json!({"error":{"code":-32603,"message":"Request failed","data":notice.trim()}}),
    )
    .await?
    .unwrap();
    assert_eq!(f.class, Class::Quota);
    assert!(f.resets_at_ms.is_some());
    s.close().await
}
