use kar::{Direction, Event, Outcome, RecordKind, Session, Turn};
use std::time::Duration;
use uuid::Uuid;

async fn attempt(session: &Session, active: bool) -> Uuid {
    let from = session.records().len();
    let result = session.abort().await;
    if active {
        result.unwrap();
    } else {
        assert!(matches!(result, Err(kar::Error::Refused(_))));
    }
    let rows = session.records();
    let rows = &rows[from..];
    let mut requests = rows.iter().filter(|r| {
        r.kind == RecordKind::Request
            && r.direction == Direction::ToRuntime
            && r.native["type"] == "abort"
            && r.native["id"]
                .as_str()
                .is_some_and(|id| Uuid::parse_str(id).is_ok())
    });
    let request = requests.next().expect("missing public abort request");
    assert!(requests.next().is_none());
    let mut decisions = rows.iter().filter(|r| {
        r.kind == RecordKind::Response
            && r.direction == Direction::Host
            && r.native["requestId"] == request.native["id"]
    });
    let answer = decisions.next().expect("missing correlated abort decision");
    assert!(decisions.next().is_none() && answer.seq > request.seq);
    assert_eq!(
        answer.native["type"],
        if active { "accepted" } else { "rejected" }
    );
    if !active {
        assert_eq!(answer.native["code"], "no_active_turn");
    }
    Uuid::parse_str(request.native["id"].as_str().unwrap()).unwrap()
}

fn terminal(session: &Session, start: u64, input: Uuid) -> Vec<(u64, Outcome)> {
    let mut ends = vec![];
    for r in session.records() {
        let root = r.seq >= start
            && r.agent_path.is_empty()
            && r.session_id.as_deref().is_none_or(|id| id == session.id());
        let mut closed = false;
        for event in &r.events {
            if matches!(event, Event::TurnStarted) {
                closed = false;
            } else {
                assert!(!closed, "event after terminal at seq {}", r.seq);
            }
            if root
                && matches!(event, Event::NativeInput {input_id, receipt: kar::InputReceipt::Cancelled} if *input_id == input)
            {
                ends.push((r.seq, Outcome::Aborted));
            }
            if let Event::TurnEnded { outcome } = event {
                if root {
                    ends.push((r.seq, outcome.clone()));
                }
                closed = true;
            }
        }
    }
    ends
}

/// The fixture holds one known model request open, with no following turn dispatched.
pub async fn check(session: &Session, turn: Turn) -> Outcome {
    let input = turn.input_id();
    let id = input.to_string();
    let start = session
        .records()
        .iter()
        .find(|r| r.prompt.as_ref().is_some_and(|p| p.input_id == id))
        .unwrap()
        .seq;
    let accepted = attempt(session, true).await;
    let outcome = tokio::time::timeout(Duration::from_secs(15), turn.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, Outcome::Completed | Outcome::Aborted));
    let before = terminal(session, start, input);
    assert_eq!(
        before.len(),
        1,
        "one terminal receipt or model end: {before:?}"
    );
    assert_eq!(before[0].1, outcome);
    assert!(!session.status().is_running());
    let late = attempt(session, false).await;
    assert_ne!(late, accepted);
    assert_eq!(before, terminal(session, start, input));
    println!(
        "verified correlated active/idle abort on {:?} with one {outcome:?} terminal",
        session.protocol()
    );
    outcome
}
