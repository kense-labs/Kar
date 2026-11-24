//! The upstream runtime’s public cursor, observer-isolation, multi-turn and late-steer cases.
#[path = "disposal.rs"]
mod disposal;
use kar::{Error, Event, Outcome, Record, RecordKind, Session, Subscription};
use std::{sync::Arc, time::Duration};

async fn compare_delivery(sub: &mut Subscription, retained: &[Arc<Record>]) {
    for expected in retained {
        let actual = sub.next().await.expect("subscriber ended during a turn");
        assert_eq!(actual.cursor(), expected.cursor());
        assert_eq!(
            serde_json::to_value(&*actual).unwrap(),
            serde_json::to_value(&**expected).unwrap(),
            "different content at sequence {}",
            expected.seq
        );
    }
}

pub(crate) async fn check(session: &Session) -> kar::Result<()> {
    tokio::time::timeout(Duration::from_secs(60), three_turns(session))
        .await
        .expect("public stream cases timed out")
}

pub(crate) async fn dispose_mid_turn(session: &Session) -> kar::Result<()> {
    let _turn = session
        .prompt("please answer slow-ly (slow marker)")
        .await?;
    disposal::check(session, "exited").await
}

async fn three_turns(session: &Session) -> kar::Result<()> {
    let baseline = session.records();
    let since = baseline.last().unwrap().cursor();
    let mut observer = session.subscribe(Some(&since))?;
    let mut failing = session.subscribe(Some(&since))?;
    let task = tokio::spawn(async move {
        while let Some(record) = failing.next().await {
            if record.prompt.is_some() {
                panic!("intentional failure in an independent subscriber");
            }
        }
    });

    let mut next = baseline.len();
    let mut reconnect = None;
    for input in ["one", "two", "three"] {
        let turn = session.prompt(input).await?;
        let id = turn.input_id().to_string();
        assert_eq!(turn.wait().await?, Outcome::Completed);
        let records = session.records();
        let slice = &records[next..];
        assert!(slice.windows(2).all(|v| v[1].seq == v[0].seq + 1));
        let prompts: Vec<_> = slice.iter().filter(|r| r.prompt.is_some()).collect();
        assert_eq!(prompts.len(), 1);
        let prompt = prompts[0];
        assert_eq!(prompt.prompt.as_ref().unwrap().input_id, id);
        assert_eq!(prompt.kind, RecordKind::Request);
        let request = prompt
            .prompt
            .as_ref()
            .unwrap()
            .request_id
            .as_deref()
            .unwrap();
        let replies: Vec<_> = slice
            .iter()
            .filter(|r| {
                r.kind == RecordKind::Response
                    && r.direction == kar::Direction::Host
                    && r.native["requestId"] == request
            })
            .collect();
        assert_eq!(replies.len(), 1, "one public decision per attempt");
        assert_eq!(replies[0].native["type"], "accepted");
        assert!(prompt.seq < replies[0].seq);
        let mut ends = Vec::new();
        for r in slice {
            if !r.agent_path.is_empty() {
                continue;
            }
            assert_eq!(r.session_id.as_deref(), Some(session.id()));
            let mut ended = false;
            for event in &r.events {
                match event {
                    Event::TurnStarted => ended = false,
                    _ if ended => panic!("unexpected event after turn end at {}: {event:?}", r.seq),
                    Event::TurnEnded { outcome } => {
                        assert_eq!(outcome, &Outcome::Completed);
                        ended = true;
                        ends.push(r.seq);
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(ends.len(), 1, "each root turn must end exactly once");
        assert!(
            ends[0] > replies[0].seq,
            "ordinary acceptance must precede its end"
        );

        compare_delivery(&mut observer, slice).await;
        if let Some(sub) = &mut reconnect {
            compare_delivery(sub, slice).await;
        } else {
            let cursor = prompt.cursor();
            let mut sub = session.subscribe(Some(&cursor))?;
            compare_delivery(&mut sub, &records[(prompt.seq + 1) as usize..]).await;
            reconnect = Some(sub);
        }
        next = records.len();
    }
    assert!(task.await.unwrap_err().is_panic());
    if session.capabilities().steering {
        let from = session.records().len();
        assert!(matches!(
            session.steer("too late").await,
            Err(Error::Refused(_))
        ));
        let rows = session.records();
        let request = rows[from..]
            .iter()
            .find(|r| {
                r.kind == RecordKind::Request
                    && r.native["type"] == "steer"
                    && r.native["input"] == "too late"
            })
            .unwrap();
        let decisions: Vec<_> = rows[from..]
            .iter()
            .filter(|r| {
                r.kind == RecordKind::Response
                    && r.native["requestId"] == request.native["id"]
                    && r.native["type"] == "rejected"
            })
            .collect();
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0].seq > request.seq);
        assert_eq!(decisions[0].native["code"], "no_active_turn");
        println!("verified late-steer refusal on {:?}", session.protocol());
    }
    disposal::check(session, "exited").await?;
    let records = session.records();
    let mut reconnect = reconnect.unwrap();
    for sub in [&mut observer, &mut reconnect] {
        compare_delivery(sub, &records[next..]).await;
        assert!(sub.next().await.is_none(), "duplicate delivery after close");
    }
    Ok(())
}
