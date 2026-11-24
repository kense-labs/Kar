use kar::{Direction, Error, RecordKind, Session};

pub(crate) async fn check(session: &Session, expected: &str) -> kar::Result<()> {
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(session.close(), session.close())
    })
    .await
    .expect("dispose must finish within the public 15-second deadline");
    first?;
    second?;
    let records = session.records();
    let mut requests = records.iter().filter(|r| {
        r.direction == Direction::ToRuntime
            && r.kind == RecordKind::Request
            && r.native["type"] == "dispose"
    });
    let dispose = requests
        .next()
        .expect("explicit close must record a dispose request");
    assert!(requests.next().is_none(), "close recorded twice");
    assert_eq!(dispose.session_id.as_deref(), Some(session.id()));
    let id = dispose.native["id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    let mut answers = records.iter().filter(|r| {
        r.direction == Direction::Host
            && r.kind == RecordKind::Response
            && r.native["requestId"] == id
    });
    let answer = answers.next().expect("no correlated dispose response");
    assert!(answers.next().is_none(), "dispose answered twice");
    assert_eq!(answer.native["type"], expected);
    if expected == "accepted" {
        assert!(answer.events.contains(&kar::Event::Released));
        assert!(
            !answer
                .events
                .iter()
                .any(|e| matches!(e, kar::Event::Exited { .. }))
        );
        let released = answer.session_release().expect("controller release facts");
        assert_eq!(released.reason, kar::ReleaseReason::Dispose);
        assert!(
            released.native_turn_may_continue && released.work_started_after_cleanup_may_continue
        );
    }
    if expected == "exited" {
        assert!(
            answer
                .native
                .get("code")
                .is_some_and(|code| code.is_i64() || code.is_null())
        );
    }
    assert!(answer.seq > dispose.seq);
    assert_eq!(answer.session_id, dispose.session_id);
    let late = session
        .submit_prompt(None, "after dispose", Vec::new())
        .await?;
    let kar::PromptDecision::Rejected {
        receipt,
        error: Error::Closed(_),
    } = late
    else {
        panic!("closed session must return a recorded refusal")
    };
    let code = if expected == "exited" {
        "runtime_exited"
    } else {
        "disposed"
    };
    assert_eq!(receipt.response.native["code"], code);
    assert_eq!(
        receipt.request.control_request_id(),
        receipt.response.control_response_to()
    );
    let after = session.records();
    assert_eq!(after.len(), records.len() + 2);
    assert_eq!(receipt.request.seq, records.len() as u64);
    assert!(matches!(
        session.withdraw(uuid::Uuid::new_v4()).await,
        Err(Error::Closed(_))
    ));
    let withdrawn = session.records();
    assert_eq!(withdrawn[after.len()].native["type"], "withdraw");
    assert_eq!(withdrawn[after.len() + 1].native["code"], code);
    session.close().await?;
    assert_eq!(session.records().len(), withdrawn.len());
    Ok(())
}
