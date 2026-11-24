use super::*;

pub(super) async fn run() -> kar::Result<()> {
    use kar::{DeliverLanded, DeliverOptions, DeliverWhen, InputOptions};
    use std::sync::Arc;
    let (_dir, runtime, opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    let session = runtime.open(opts).await?;
    assert!(!session.capabilities().steering);
    let mut events = session.subscribe(None)?;
    let active = session.prompt("delivery-tool-wait").await?;
    loop {
        let r = events.next().await.expect("active session");
        if r.events
            .iter()
            .any(|e| matches!(e, Event::ToolStarted{id,..} if id=="held-tool"))
        {
            break;
        }
    }
    let first = session
        .deliver_with_options(
            "merged-first",
            DeliverOptions {
                when: DeliverWhen::AfterTurn,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(first.landed, DeliverLanded::Queued);
    let one = uuid::Uuid::new_v4();
    let two = uuid::Uuid::new_v4();
    let background = {
        let session = session.clone();
        tokio::spawn(async move {
            session
                .deliver_with_options(
                    "interrupt-one",
                    DeliverOptions {
                        when: DeliverWhen::Interrupt {
                            max_wait: Duration::from_secs(12),
                        },
                        input: InputOptions {
                            input_id: Some(one),
                            ..Default::default()
                        },
                        merge: Some(Arc::new(|items| {
                            Ok(kar::MergedInput {
                                text: items
                                    .iter()
                                    .map(|i| i.text.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" + "),
                                ..Default::default()
                            })
                        })),
                    },
                )
                .await
        })
    };
    loop {
        let r = events.next().await.expect("active session");
        if r.events
            .iter()
            .any(|e| matches!(e,Event::InputQueued{input_id} if input_id==&one.to_string()))
        {
            break;
        }
    }
    let result = session
        .deliver_with_options(
            "interrupt-two",
            DeliverOptions {
                when: DeliverWhen::Interrupt {
                    max_wait: Duration::ZERO,
                },
                input: InputOptions {
                    input_id: Some(two),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(active.wait().await?, Outcome::Aborted);
    let shared = background.await.unwrap()?;
    assert_eq!(shared.input_id, one);
    assert_eq!(result.input_id, two);
    assert_eq!(result.landed, DeliverLanded::Prompted);
    assert_eq!(result.merged_input_ids, [first.input_id, one, two]);
    assert_eq!(shared.merged_input_ids, result.merged_input_ids);
    let request = &result.control.unwrap().request;
    assert_eq!(
        request
            .native
            .pointer("/params/prompt/0/text")
            .and_then(Value::as_str),
        Some("merged-first + interrupt-one + interrupt-two")
    );
    assert_eq!(
        request.carries,
        result
            .merged_input_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    let idle = session
        .deliver_with_options(
            "normal",
            DeliverOptions {
                when: DeliverWhen::WhenIdle,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(idle.landed, DeliverLanded::Prompted);
    session.await_idle().await?;
    let running = session.prompt("wait").await?;
    let after_idle = uuid::Uuid::new_v4();
    let mut waiting = Box::pin(session.deliver_with_options(
        "normal",
        DeliverOptions {
            when: DeliverWhen::WhenIdle,
            input: InputOptions {
                input_id: Some(after_idle),
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err()
    );
    let key = after_idle.to_string();
    assert!(
        session
            .records()
            .iter()
            .all(|r| r.native["inputId"].as_str() != Some(&key)
                && r.prompt.as_ref().is_none_or(|p| p.input_id != key))
    );
    let (earlier, _) = session.queue("kept-by-caller").await?;
    let rejected_id = uuid::Uuid::new_v4();
    let rejected = session
        .deliver_with_options(
            "also-kept",
            DeliverOptions {
                when: DeliverWhen::Interrupt {
                    max_wait: Duration::ZERO,
                },
                input: InputOptions {
                    input_id: Some(rejected_id),
                    ..Default::default()
                },
                merge: Some(Arc::new(|_| {
                    Err(Error::Refused("caller declined the merge".into()))
                })),
            },
        )
        .await?;
    assert_eq!(running.wait().await?, Outcome::Aborted);
    assert_eq!(rejected.landed, DeliverLanded::Rejected);
    assert_eq!(rejected.undelivered_input_ids, [earlier, rejected_id]);
    assert_eq!(waiting.await?.landed, DeliverLanded::Prompted);
    assert!(
        session
            .records()
            .iter()
            .all(|r| !r.carries.iter().any(|id| id == &rejected_id.to_string()))
    );
    session.close().await?;
    unexpected_exit().await
}

async fn unexpected_exit() -> kar::Result<()> {
    use kar::{DeliverLanded, DeliverOptions, DeliverWhen, InputOptions};
    let (dir, runtime, mut opts) = rig(Protocol::Acp, ApprovalMode::Deny);
    let marker = dir.path().join("exit-now");
    opts.env.insert(
        "KAR_TEST_DELIVERY_EXIT".into(),
        marker.clone().into_os_string(),
    );
    let session = runtime.open(opts).await?;
    let mut journal = session.subscribe(None)?;
    let _turn = session.prompt("delivery-tool-exit").await?;
    while !journal
        .next()
        .await
        .unwrap()
        .events
        .iter()
        .any(|e| matches!(e, Event::ToolStarted { .. }))
    {}
    let (earlier, _) = session.queue("saved").await?;
    let id = uuid::Uuid::new_v4();
    let call = {
        let session = session.clone();
        tokio::spawn(async move {
            session
                .deliver_with_options(
                    "urgent",
                    DeliverOptions {
                        when: DeliverWhen::Interrupt {
                            max_wait: Duration::from_secs(15),
                        },
                        input: InputOptions {
                            input_id: Some(id),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
                .await
        })
    };
    while !journal
        .next()
        .await
        .unwrap()
        .events
        .iter()
        .any(|e| matches!(e,Event::InputQueued{input_id} if input_id==&id.to_string()))
    {}
    std::fs::write(marker, [])?;
    let receipt = call.await.unwrap()?;
    assert_eq!(receipt.landed, DeliverLanded::Rejected);
    assert_eq!(receipt.undelivered_input_ids, [earlier, id]);
    assert_eq!(
        session
            .records()
            .iter()
            .filter(|r| r.events.contains(&Event::PromptDispatched))
            .count(),
        1
    );
    Ok(())
}
